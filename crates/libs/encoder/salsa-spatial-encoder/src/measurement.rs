//! Aligned STFT measurements for one tile.
//!
//! Source objects, the ideal core and the locally decoded core are analysed
//! with the shared 1920/240 filterbank and accumulated in `f64` with the exact
//! parameter-band bin weights. Powers use each anchor's window center.
//! Correlation candidates use the complete tile.

use num_complex::Complex;
use rayon::prelude::*;
use salsa_spatial::{
    FFT_BINS, FFT_SIZE, HOP_SAMPLES, PARAM_BAND_COUNT, TILE_HOPS, TrajectoryKnot, bin_weight,
    parameter_band_range,
};
use salsa_spatial_dsp::{BatchAnalyzer, StatisticsAccumulator, StftConfig};

use crate::source::TileSource;
use crate::trajectory::position_at;

/// Per-band source cross covariance over the active object set.
///
/// `bands[band][ai * active_objects.len() + aj]` stores
/// `E[s_i conj(s_j)]` for the compact active indices.  Inactive objects have
/// exactly zero power, so keeping their cross terms would only add work and
/// allocations.
pub type SourceCross = Vec<Vec<Complex<f64>>>;

pub struct TileMeasurements {
    pub hop_count: usize,
    pub stats: StatisticsAccumulator,
    /// `[hop][band][ai * active_objects.len() + aj]`
    pub source_cross_hops: Vec<SourceCross>,
    pub source_cross: SourceCross,
    /// Compact indices of objects that are active in at least one hop.
    pub active_objects: Vec<usize>,
    pub source_positions: Vec<Vec<[f64; 3]>>,
    pub recon_positions: Vec<Vec<[f64; 3]>>,
    pub downmix_columns: Vec<Vec<[f64; 7]>>,
    pub source_active_hops: Vec<Vec<bool>>,
}

/// Retained half-window and signal units for continuous encoder analysis.
pub(crate) struct MeasurementContext {
    objects: Vec<Vec<f32>>,
    core: [Vec<f32>; 7],
    headroom_gain: f64,
}

impl MeasurementContext {
    pub fn new(headroom_gain: f64) -> Self {
        Self {
            objects: Vec::new(),
            core: Default::default(),
            headroom_gain,
        }
    }

    pub fn prefix_frames(&self) -> usize {
        self.core[0].len()
    }

    /// Measure one tile with its past and future half-windows. `core` covers
    /// the current tile. `decoded` includes the retained past and the next
    /// tile's first half-window. All positions use the source sample clock.
    pub fn measure_tile(
        &mut self,
        source: &TileSource,
        core: &[Vec<f32>; 7],
        decoded: &[Vec<f32>; 7],
        core_knots: &[Vec<TrajectoryKnot>],
        next: Option<(&TileSource, &[Vec<f32>; 7])>,
    ) -> TileMeasurements {
        let frames = source.frames;
        let object_count = source.object_count;
        let hop_count = frames.div_ceil(HOP_SAMPLES).clamp(1, TILE_HOPS);
        let config = StftConfig::default();

        let analyzer = BatchAnalyzer::new(config);
        let prefix = self.prefix_frames();
        let first_hop = prefix / HOP_SAMPLES;
        let analyze_window = |past: &[f32], current: &[f32], future: &[f32]| {
            let mut samples = Vec::with_capacity(past.len() + current.len() + future.len());
            samples.extend_from_slice(past);
            samples.extend_from_slice(current);
            samples.extend_from_slice(&future[..future.len().min(FFT_SIZE / 2)]);
            analyzer.analyze(&samples)[first_hop..first_hop + hop_count].to_vec()
        };
        let object_spectra: Vec<Vec<Vec<Complex<f64>>>> = source
            .object_pcm
            .par_iter()
            .enumerate()
            .map(|(object, samples)| {
                analyze_window(
                    self.objects.get(object).map_or(&[], Vec::as_slice),
                    samples,
                    next.map_or(&[], |(source, _)| source.object_pcm[object].as_slice()),
                )
            })
            .collect();
        let core_spectra: Vec<Vec<Vec<Complex<f64>>>> = core
            .par_iter()
            .enumerate()
            .map(|(channel, samples)| {
                analyze_window(
                    &self.core[channel],
                    samples,
                    next.map_or(&[], |(_, core)| core[channel].as_slice()),
                )
            })
            .collect();
        let decoded_spectra: Vec<Vec<Vec<Complex<f64>>>> = decoded
            .par_iter()
            .map(|samples| analyzer.analyze(samples)[first_hop..first_hop + hop_count].to_vec())
            .collect();

        // The estimator emits headroom-scaled objects. RD compares the restored
        // result with source PCM, so both observation paths use source units here.
        let restore = |mut spectra: Vec<Vec<Vec<Complex<f64>>>>| {
            for value in spectra.iter_mut().flatten().flatten() {
                *value /= self.headroom_gain;
            }
            spectra
        };
        let core_spectra = restore(core_spectra);
        let decoded_spectra = restore(decoded_spectra);
        let source_active_hops: Vec<Vec<bool>> = (0..hop_count)
            .map(|hop| {
                object_spectra
                    .iter()
                    .map(|spectra| {
                        spectra[hop].iter().any(|value| {
                            value.norm_sqr() > salsa_spatial_dsp::reconstruct::SILENCE_EPSILON
                        })
                    })
                    .collect()
            })
            .collect();

        // Only objects that are active in at least one hop contribute source
        // covariance. This keeps the expensive N x N work proportional to the
        // number of active objects instead of the full 128-object stress case.
        let mut active_objects = Vec::new();
        for object in 0..object_count {
            let active = source_active_hops.iter().any(|active| active[object]);
            if active {
                active_objects.push(object);
            }
        }
        let active_count = active_objects.len();
        let mut accumulator = StatisticsAccumulator::new(object_count);
        let mut source_flat = vec![Complex::new(0.0, 0.0); object_count * FFT_BINS];
        let mut core_flat = vec![Complex::new(0.0, 0.0); 7 * FFT_BINS];
        let mut decoded_flat = vec![Complex::new(0.0, 0.0); 7 * FFT_BINS];

        for hop in 0..hop_count {
            source_flat.fill(Complex::new(0.0, 0.0));
            for object in 0..object_count {
                if let Some(spectrum) = object_spectra[object].get(hop) {
                    source_flat[object * FFT_BINS..(object + 1) * FFT_BINS]
                        .copy_from_slice(spectrum);
                }
            }
            core_flat.fill(Complex::new(0.0, 0.0));
            decoded_flat.fill(Complex::new(0.0, 0.0));
            for channel in 0..7 {
                if let Some(spectrum) = core_spectra[channel].get(hop) {
                    core_flat[channel * FFT_BINS..(channel + 1) * FFT_BINS]
                        .copy_from_slice(spectrum);
                }
                if let Some(spectrum) = decoded_spectra[channel].get(hop) {
                    decoded_flat[channel * FFT_BINS..(channel + 1) * FFT_BINS]
                        .copy_from_slice(spectrum);
                }
            }
            accumulator.push_hop_objects(
                source.start_sample + (hop * HOP_SAMPLES) as u64,
                &source_flat,
                &core_flat,
                &decoded_flat,
                &active_objects,
            );
        }

        let source_cross_hops: Vec<SourceCross> = (0..hop_count)
            .into_par_iter()
            .map(|hop| {
                (0..PARAM_BAND_COUNT)
                    .map(|band| {
                        let mut rows = vec![Complex::new(0.0, 0.0); active_count * active_count];
                        for bin in parameter_band_range(band) {
                            let weight = bin_weight(bin);
                            for (ai, &i) in active_objects.iter().enumerate() {
                                let value_i = object_spectra[i][hop][bin] * weight;
                                for (aj, &j) in active_objects.iter().enumerate().skip(ai) {
                                    let value = value_i * object_spectra[j][hop][bin].conj();
                                    rows[ai * active_count + aj] += value;
                                    if ai != aj {
                                        rows[aj * active_count + ai] += value.conj();
                                    }
                                }
                            }
                        }
                        rows
                    })
                    .collect()
            })
            .collect();
        let mut source_cross =
            vec![vec![Complex::new(0.0, 0.0); active_count * active_count]; PARAM_BAND_COUNT];
        for hop in &source_cross_hops {
            for (total, band) in source_cross.iter_mut().zip(hop) {
                for (total, value) in total.iter_mut().zip(band) {
                    *total += value;
                }
            }
        }

        let source_positions = (0..hop_count)
            .map(|hop| {
                (0..object_count)
                    .map(|object| source.position(hop * HOP_SAMPLES, object).map(f64::from))
                    .collect()
            })
            .collect();
        let recon_positions = (0..hop_count)
            .map(|hop| {
                let sample = source.start_sample + (hop * HOP_SAMPLES) as u64;
                core_knots
                    .iter()
                    .map(|knots| position_at(knots, sample as i64))
                    .collect()
            })
            .collect();
        let downmix_columns = (0..hop_count)
            .map(|hop| {
                let sample = source.start_sample + (hop * HOP_SAMPLES) as u64;
                core_knots
                    .iter()
                    .map(|knots| salsa_spatial_dsp::interpolated_core_columns(knots, sample, 1)[0])
                    .collect()
            })
            .collect();

        let tail_start = frames.saturating_sub(FFT_SIZE / 2);
        self.objects = source
            .object_pcm
            .iter()
            .map(|samples| samples[tail_start..].to_vec())
            .collect();
        self.core = std::array::from_fn(|channel| core[channel][tail_start..].to_vec());

        TileMeasurements {
            hop_count,
            stats: accumulator,
            source_cross_hops,
            source_cross,
            active_objects,
            source_positions,
            recon_positions,
            downmix_columns,
            source_active_hops,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use salsa_spatial::{CORE_CHANNEL_COUNT, TILE_SAMPLES};
    use salsa_spatial_dsp::Stft;

    #[test]
    fn tile_statistics_match_the_continuous_decoder_window() {
        let signal: Vec<f32> = (0..3 * TILE_SAMPLES)
            .map(|i| {
                if i == TILE_SAMPLES - 1 {
                    1.0
                } else if i == TILE_SAMPLES + 1 {
                    -0.7
                } else {
                    (i as f32 * 0.071).sin() * 0.1
                }
            })
            .collect();
        let mut stft = Stft::new();
        let mut spectra = stft.push(&signal);
        spectra.extend(stft.flush());
        let sources: Vec<TileSource> = signal
            .chunks(TILE_SAMPLES)
            .enumerate()
            .map(|(i, pcm)| TileSource {
                start_sample: (i * TILE_SAMPLES) as u64,
                frames: pcm.len(),
                object_count: 1,
                object_pcm: vec![pcm.to_vec()],
                lfe_pcm: vec![0.0; pcm.len()],
                positions: vec![[-1.0, 1.0, 0.0]; pcm.len()],
                active: vec![true; pcm.len()],
                events: Vec::new(),
            })
            .collect();
        for gain in [1.0, 0.25] {
            let cores: Vec<[Vec<f32>; CORE_CHANNEL_COUNT]> = sources
                .iter()
                .map(|source| {
                    std::array::from_fn(|ch| {
                        if ch == 0 {
                            source.object_pcm[0]
                                .iter()
                                .map(|x| x * gain as f32)
                                .collect()
                        } else {
                            vec![0.0; source.frames]
                        }
                    })
                })
                .collect();
            let mut context = MeasurementContext::new(gain);
            for i in 0..sources.len() {
                let start = i * TILE_SAMPLES - context.prefix_frames();
                let end = ((i + 1) * TILE_SAMPLES + FFT_SIZE / 2).min(signal.len());
                let decoded: [Vec<f32>; CORE_CHANNEL_COUNT] = std::array::from_fn(|ch| {
                    if ch == 0 {
                        signal[start..end].iter().map(|x| x * gain as f32).collect()
                    } else {
                        vec![0.0; end - start]
                    }
                });
                let knots = vec![vec![TrajectoryKnot {
                    sample: 0,
                    position: [-1.0, 1.0, 0.0],
                }]];
                let next = sources.get(i + 1).zip(cores.get(i + 1));
                let measured = context.measure_tile(&sources[i], &cores[i], &decoded, &knots, next);
                for (hop, stats) in measured.stats.per_hop().iter().enumerate() {
                    let spectrum = &spectra[i * TILE_HOPS + hop];
                    for band in 0..PARAM_BAND_COUNT {
                        let expected: f64 = parameter_band_range(band)
                            .map(|bin| bin_weight(bin) * spectrum[bin].norm_sqr())
                            .sum();
                        let actual = &stats.bands[band];
                        assert!(
                            (actual.source_power[0] - expected).abs() < 1e-10 * expected.max(1.0),
                            "tile {i}, hop {hop}, band {band}: {} != {expected}",
                            actual.source_power[0]
                        );
                        assert!(
                            (actual.source_decoded[0][0].re - expected).abs()
                                < 1e-10 * expected.max(1.0)
                        );
                        assert_eq!(actual.error_power[0], 0.0);
                    }
                }
            }
        }
    }
}
