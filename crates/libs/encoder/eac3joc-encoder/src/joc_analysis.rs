// SPDX-License-Identifier: MPL-2.0
//! Real-signal, per-parameter-band JOC matrix analysis.
//!
//! The JOC decoder filters the core into 64 complex QMF subbands and applies
//! one gain per parameter band and object. This analyzer estimates the core
//! covariance and object/core cross-covariance in each band from the DAMF
//! essence, then solves the per-band Wiener least-squares problem.
//!
//! A Hann-windowed `rustfft` bank approximates the decoder's QMF split: at
//! 48 kHz a 1024-point FFT gives 8 bins per 375 Hz QMF subband.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use eac3joc::parameter_band_map;
use objectaudio_damf::{CafReader, Manifest, Metadata};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

use crate::joc::{self, ENC_BANDS};
use crate::render;

/// FFT analysis size and hop. At 48 kHz a 1024-point FFT gives 8 bins per 375 Hz QMF subband.
const FFT_SIZE: usize = 1024;
const HOP: usize = 512;
const BINS_PER_SUBBAND: usize = FFT_SIZE / 128;

/// Per-frame spectral statistics (real part only — the signals are real, so the negative-frequency
/// contribution is the conjugate).
struct Stats {
    core: [[[f64; 5]; 5]; ENC_BANDS],
    cross: Vec<[[f64; 5]; ENC_BANDS]>,
    power: Vec<[f64; ENC_BANDS]>,
}

impl Stats {
    fn new(nobj: usize) -> Self {
        Self {
            core: [[[0.0; 5]; 5]; ENC_BANDS],
            cross: vec![[[0.0; 5]; ENC_BANDS]; nobj],
            power: vec![[0.0; ENC_BANDS]; nobj],
        }
    }

    fn clear(&mut self) {
        for band in self.core.iter_mut() {
            for row in band.iter_mut() {
                *row = [0.0; 5];
            }
        }
        for obj in self.cross.iter_mut() {
            *obj = [[0.0; 5]; ENC_BANDS];
        }
        for obj in self.power.iter_mut() {
            *obj = [0.0; ENC_BANDS];
        }
    }

    fn scale(&mut self, s: f64) {
        for band in self.core.iter_mut() {
            for row in band.iter_mut() {
                for v in row.iter_mut() {
                    *v *= s;
                }
            }
        }
        for obj in self.cross.iter_mut() {
            for band in obj.iter_mut() {
                for v in band.iter_mut() {
                    *v *= s;
                }
            }
        }
        for obj in self.power.iter_mut() {
            for v in obj.iter_mut() {
                *v *= s;
            }
        }
    }

    fn copy_from(&mut self, other: &Stats) {
        self.core = other.core;
        self.cross.clone_from(&other.cross);
        self.power.clone_from(&other.power);
    }

    /// `self = (1 − a)·self + a·other`.
    fn blend(&mut self, other: &Stats, a: f64) {
        let b = 1.0 - a;
        for (s, o) in self.core.iter_mut().zip(&other.core) {
            for (sr, or) in s.iter_mut().zip(o) {
                for (v, w) in sr.iter_mut().zip(or) {
                    *v = *v * b + *w * a;
                }
            }
        }
        for (s, o) in self.cross.iter_mut().zip(&other.cross) {
            for (sb, ob) in s.iter_mut().zip(o) {
                for (v, w) in sb.iter_mut().zip(ob) {
                    *v = *v * b + *w * a;
                }
            }
        }
        for (s, o) in self.power.iter_mut().zip(&other.power) {
            for (v, w) in s.iter_mut().zip(o) {
                *v = *v * b + *w * a;
            }
        }
    }

    fn core_energy(&self, band: usize) -> f64 {
        (0..5).map(|i| self.core[band][i][i]).sum::<f64>() / 5.0
    }
}

/// Per-object fine/coarse costs for the exact fallback knapsack.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ObjectRate {
    pub fine_bits: usize,
    pub coarse_bits: usize,
    pub coarse_distortion: f64,
}

/// Streaming per-frame JOC band analyzer over a DAMF essence file.
pub(crate) struct JocAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    fft_buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    window: Vec<f32>,
    band_bins: [(usize, usize); ENC_BANDS],
    bed_channels: usize,
    channels: usize,
    nobj: usize,
    nsig: usize,
    objects: Vec<render::ObjEnv>,
    obj_vals: Vec<f32>,

    reader: CafReader,
    eof: bool,
    samples_read: u64,

    joc_lambda: f64,
    ridge: f64,
    refine_rounds: usize,
    rd_passes: usize,
    smooth: f64,
    power_gate: f64,
    rates: Vec<ObjectRate>,

    block: Vec<f32>,
    fill: usize,
    spectra: Vec<f32>,

    raw: Stats,
    smoothed: Stats,
    initialized: bool,
    frame_blocks: usize,
    matrices: Vec<[[f32; 5]; ENC_BANDS]>,
}

impl JocAnalyzer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        manifest: &Manifest,
        metadata: &Metadata,
        pan_mode: render::PanMode,
        joc_lambda: f64,
        ridge: f64,
        refine_rounds: usize,
        rd_passes: usize,
        smooth: f64,
        power_gate: f64,
    ) -> Result<Self> {
        let reader = CafReader::open(&manifest.audio)
            .with_context(|| format!("opening {}", manifest.audio.display()))?;
        let info = reader.info();
        let channels = info.channels;
        let bed_channels = manifest.beds.len();
        ensure!(
            bed_channels == 1 && manifest.beds[0].name.eq_ignore_ascii_case("LFE"),
            "the JOC analyzer supports an LFE-only bed"
        );
        let nobj = manifest.objects.len();
        ensure!(
            channels == bed_channels + nobj,
            "CAF has {channels} channels, need {}",
            bed_channels + nobj
        );

        let mut keys = render::gain_keyframes(metadata, pan_mode);
        let objects = manifest
            .objects
            .iter()
            .enumerate()
            .map(|(index, object)| render::ObjEnv {
                ess_ch: bed_channels + index,
                keys: keys.remove(&object.id).unwrap_or_default(),
                cursor: 0,
            })
            .collect();

        let window = (0..FFT_SIZE)
            .map(|i| {
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / FFT_SIZE as f64).cos()) as f32
            })
            .collect();
        let map = parameter_band_map(ENC_BANDS).expect("12-band JOC map");
        let mut band_bins = [(0usize, 0usize); ENC_BANDS];
        for (subband, &band) in map.iter().enumerate() {
            let lo = subband * BINS_PER_SUBBAND;
            let hi = (subband + 1) * BINS_PER_SUBBAND;
            let entry = &mut band_bins[band as usize];
            if entry.0 == 0 && entry.1 == 0 {
                *entry = (lo, hi);
            } else {
                entry.1 = hi;
            }
        }

        let nsig = 5 + nobj;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Ok(Self {
            fft,
            fft_buf: vec![Complex32::default(); FFT_SIZE],
            scratch,
            window,
            band_bins,
            bed_channels,
            channels,
            nobj,
            nsig,
            objects,
            obj_vals: vec![0.0; nobj],
            reader,
            eof: false,
            samples_read: 0,
            joc_lambda,
            ridge,
            refine_rounds,
            rd_passes,
            smooth,
            power_gate,
            rates: vec![ObjectRate::default(); nobj],
            block: vec![0.0; nsig * FFT_SIZE],
            fill: 0,
            spectra: vec![0.0; nsig * (FFT_SIZE / 2 + 1) * 2],
            raw: Stats::new(nobj),
            smoothed: Stats::new(nobj),
            initialized: false,
            frame_blocks: 0,
            matrices: vec![[[0.0; 5]; ENC_BANDS]; nobj],
        })
    }

    /// Return the matrices for the core frame spanning samples `[start, end)`, advancing the
    /// essence reader as needed. Frames must be requested in order.
    pub(crate) fn matrices_for_frame(
        &mut self,
        start: u64,
        end: u64,
    ) -> std::io::Result<&[[[f32; 5]; ENC_BANDS]]> {
        if end <= start || end <= self.samples_read {
            return Ok(&self.matrices);
        }
        self.read_until(end)?;
        self.finish_frame();
        Ok(&self.matrices)
    }

    fn read_until(&mut self, target: u64) -> std::io::Result<()> {
        while self.samples_read < target {
            let want = ((target - self.samples_read) as usize).min(4096);
            let pcm = if self.eof {
                Vec::new()
            } else {
                self.reader
                    .read_frames(want)
                    .map_err(|error| std::io::Error::other(error.to_string()))?
            };
            if pcm.len() < want * self.channels {
                self.eof = true;
            }
            let frames = pcm.len() / self.channels;
            for s in 0..want {
                let t = self.samples_read;
                if s < frames {
                    let base = s * self.channels;
                    self.consume_sample(t, Some(&pcm[base..base + self.channels]));
                } else {
                    self.consume_sample(t, None);
                }
                self.samples_read += 1;
            }
        }
        Ok(())
    }

    fn consume_sample(&mut self, t: u64, sample: Option<&[f32]>) {
        for o in 0..self.nobj {
            self.obj_vals[o] = sample.map_or(0.0, |pcm| pcm[self.bed_channels + o]);
        }

        let mut core = [0f32; 5];
        for o in 0..self.nobj {
            let value = self.obj_vals[o];
            if value == 0.0 {
                self.objects[o].gains_at(t);
                continue;
            }
            let g = self.objects[o].gains_at(t);
            for ch in 0..5 {
                core[ch] += value * g[ch];
            }
        }

        let n = FFT_SIZE;
        let pos = self.fill;
        for ch in 0..5 {
            self.block[ch * n + pos] = core[ch];
        }
        for o in 0..self.nobj {
            self.block[(5 + o) * n + pos] = self.obj_vals[o];
        }
        self.fill += 1;
        if self.fill == n {
            self.process_block();
        }
    }

    fn process_block(&mut self) {
        let n = FFT_SIZE;
        let half = n / 2;
        let nbins = half + 1;
        for s in 0..self.nsig {
            for i in 0..n {
                self.fft_buf[i] = Complex32::new(self.block[s * n + i] * self.window[i], 0.0);
            }
            self.fft
                .process_with_scratch(&mut self.fft_buf, &mut self.scratch);
            let base = s * nbins * 2;
            for k in 0..=half {
                self.spectra[base + 2 * k] = self.fft_buf[k].re;
                self.spectra[base + 2 * k + 1] = self.fft_buf[k].im;
            }
        }

        for b in 0..ENC_BANDS {
            let (lo, hi) = self.band_bins[b];
            let lo = lo.max(1);
            let hi = hi.min(half);
            if lo >= hi {
                continue;
            }
            for k in lo..hi {
                let mut cr = [0f32; 5];
                let mut ci = [0f32; 5];
                for ch in 0..5 {
                    let idx = (ch * nbins + k) * 2;
                    cr[ch] = self.spectra[idx];
                    ci[ch] = self.spectra[idx + 1];
                }
                for i in 0..5 {
                    for j in i..5 {
                        let v = (cr[i] * cr[j] + ci[i] * ci[j]) as f64;
                        self.raw.core[b][i][j] += v;
                        if i != j {
                            self.raw.core[b][j][i] += v;
                        }
                    }
                }
                for o in 0..self.nobj {
                    let idx = ((5 + o) * nbins + k) * 2;
                    let xr = self.spectra[idx];
                    let xi = self.spectra[idx + 1];
                    for ch in 0..5 {
                        self.raw.cross[o][b][ch] += (xr * cr[ch] + xi * ci[ch]) as f64;
                    }
                    self.raw.power[o][b] += (xr * xr + xi * xi) as f64;
                }
            }
        }

        for s in 0..self.nsig {
            self.block[s * n..s * n + n].copy_within(HOP..n, 0);
        }
        self.fill = n - HOP;
        self.frame_blocks += 1;
    }

    pub(crate) fn object_rates(&self) -> &[ObjectRate] {
        &self.rates
    }

    fn finish_frame(&mut self) {
        let blocks = self.frame_blocks;
        self.frame_blocks = 0;
        if blocks == 0 {
            return;
        }
        self.raw.scale(1.0 / blocks as f64);
        if !self.initialized {
            self.smoothed.copy_from(&self.raw);
            self.initialized = true;
        } else {
            self.smoothed.blend(&self.raw, self.smooth);
        }
        let mut matrices =
            joc::analyze_bands(&self.smoothed.core, &self.smoothed.cross, self.ridge);
        // Quantization-aware coordinate descent; most frames use fine per-band matrices. The
        // adaptive encoder can still fall back to coarse/broadband for the few frames that do
        // not fit the skip field.
        joc::refine_quantized(
            &self.smoothed.core,
            &self.smoothed.cross,
            &mut matrices,
            1,
            self.refine_rounds,
        );
        joc::refine_rate_distortion(
            &self.smoothed.core,
            &self.smoothed.cross,
            &mut matrices,
            self.joc_lambda,
            self.rd_passes,
        );
        self.matrices = matrices;
        for o in 0..self.nobj {
            for b in 0..ENC_BANDS {
                if self.raw.power[o][b] < self.raw.core_energy(b) * self.power_gate {
                    self.matrices[o][b] = [0.0; 5];
                }
            }
        }
        for o in 0..self.nobj {
            let fine = &self.matrices[o];
            let mut coarse = [[0f32; 5]; ENC_BANDS];
            for b in 0..ENC_BANDS {
                for ch in 0..5 {
                    coarse[b][ch] = joc::quantized_value(fine[b][ch], 0);
                }
            }
            let fine_bits = joc::matrix_bits_for_object(fine, 1, true);
            let coarse_bits = joc::matrix_bits_for_object(&coarse, 0, true);
            let d_fine = joc::object_distortion(&self.smoothed.core, &self.smoothed.cross[o], fine);
            let d_coarse =
                joc::object_distortion(&self.smoothed.core, &self.smoothed.cross[o], &coarse);
            self.rates[o] = ObjectRate {
                fine_bits,
                coarse_bits,
                coarse_distortion: (d_coarse - d_fine).max(0.0),
            };
        }
        self.raw.clear();
    }
}
