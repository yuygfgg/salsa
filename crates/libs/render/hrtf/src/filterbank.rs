//! The two 64-band QMF / 77-band hybrid filterbanks.
//!
//! `PublicFilterbank` is the shared bank the SOFA compiler also uses (13-tap
//! hybrid filters, six-slot high-band delay, 961 samples of latency).
//! `RosellaFilterbank` is the bank captured from the reference headphone
//! renderer (11-tap hybrid filters, three-slot delay, 448 samples). Both are
//! driven through the same four stateful half-steps.

use std::sync::Arc;

use num_complex::Complex64;
use rustfft::{Fft as RustFft, FftPlanner};

use crate::tables_public as public;
use crate::tables_rosella as rosella;

pub const QMF_BANDS: usize = 64;
pub const QMF_HOP: usize = 64;
pub const HYBRID_BANDS: usize = 77;
pub const HYBRID_LOW: usize = 16;
pub const PUBLIC_LATENCY: usize = 961;
pub const ROSELLA_LATENCY: usize = 448;

type Complex = Complex64;

/// Which analysis/synthesis bank the Rosella renderer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterbankKind {
    /// Shared public bank: 13-tap hybrid, six-slot delay, 961 samples.
    #[default]
    Public,
    /// Captured Rosella bank: 11-tap hybrid, three-slot delay, 448 samples.
    Captured,
}

impl FilterbankKind {
    pub fn latency(self) -> usize {
        match self {
            Self::Public => PUBLIC_LATENCY,
            Self::Captured => ROSELLA_LATENCY,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "public" | "1" => Some(Self::Public),
            "captured" | "rosella" | "2" => Some(Self::Captured),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Captured => "captured",
        }
    }
}

/// Analysis/synthesis half-steps shared by both banks.
pub trait Filterbank: Send {
    fn kind(&self) -> FilterbankKind;
    fn latency(&self) -> usize {
        self.kind().latency()
    }
    fn reset(&mut self);

    /// Full-rate hops `[slots][channels][64]` -> QMF `[slots][channels][64]`.
    fn analyze_qmf(&mut self, hops: &[f64], slots: usize, qmf: &mut Vec<Complex>);
    /// QMF -> hybrid `[slots][channels][77]`.
    fn analyze_hybrid(&mut self, qmf: &[Complex], slots: usize, hybrid: &mut Vec<Complex>);
    /// Hybrid -> QMF.
    fn synthesize_hybrid(&mut self, hybrid: &[Complex], slots: usize, qmf: &mut Vec<Complex>);
    /// QMF -> full-rate samples `[slots*64][channels]`.
    fn synthesize_qmf(&mut self, qmf: &[Complex], slots: usize, hops: &mut Vec<f64>);
}

/// Build either bank with `channels` independent chains.
pub fn create(kind: FilterbankKind, channels: usize) -> Box<dyn Filterbank> {
    match kind {
        FilterbankKind::Public => Box::new(PublicFilterbank::new(channels)),
        FilterbankKind::Captured => Box::new(RosellaFilterbank::new(channels)),
    }
}

fn fft_plan(planner: &mut FftPlanner<f64>, size: usize, inverse: bool) -> Arc<dyn RustFft<f64>> {
    if inverse {
        planner.plan_fft_inverse(size)
    } else {
        planner.plan_fft_forward(size)
    }
}

// ---------------------------------------------------------------------------
// Public bank
// ---------------------------------------------------------------------------

const PUBLIC_QMF_TAPS: usize = 10;
const PUBLIC_SYNTHESIS_RANK: usize = 4;
const PUBLIC_SYNTHESIS_TAPS: usize = 10;
const PUBLIC_HYBRID_TAPS: usize = 13;
const PUBLIC_HYBRID_DELAY: usize = 6;
const PUBLIC_FFT: usize = 128;
const PUBLIC_JOIN_TERMS: usize = 78;
const PUBLIC_JOIN_OUTPUTS: usize = 32;
const PUBLIC_JOIN_BLOCK: usize = 32;

struct PublicQmfAnalysis {
    channels: usize,
    coefficients_by_lag: Vec<f64>,
    history: Vec<f64>,
    joined: Vec<f64>,
    even: Vec<f64>,
    odd: Vec<f64>,
    premultiply: Vec<Complex>,
    post: Vec<Complex>,
    even_post: Vec<Complex>,
    plan: Arc<dyn RustFft<f64>>,
}

impl PublicQmfAnalysis {
    fn new(channels: usize) -> Self {
        let coefficients = &public::QMF_ANALYSIS_COEFFICIENTS;
        let mut coefficients_by_lag = vec![0.0f64; PUBLIC_QMF_TAPS * QMF_BANDS];
        for band in 0..QMF_BANDS {
            for tap in 0..PUBLIC_QMF_TAPS {
                coefficients_by_lag[tap * QMF_BANDS + band] =
                    coefficients[band * PUBLIC_QMF_TAPS + tap];
            }
        }
        let mut premultiply = vec![Complex::new(0.0, 0.0); QMF_BANDS];
        let mut post = vec![Complex::new(0.0, 0.0); QMF_BANDS];
        let mut even_post = vec![Complex::new(0.0, 0.0); QMF_BANDS];
        for band in 0..QMF_BANDS {
            let phase = band as f64;
            premultiply[band] = Complex::from_polar(1.0, -std::f64::consts::PI * phase / 128.0);
            post[band] =
                Complex::from_polar(1.0, -3.0 * (phase + 0.5) * std::f64::consts::PI / 128.0);
            even_post[band] = Complex::new(0.0, if band % 2 == 0 { 1.0 } else { -1.0 });
        }
        let mut planner = FftPlanner::<f64>::new();
        let plan = fft_plan(&mut planner, PUBLIC_FFT, false);
        Self {
            channels,
            coefficients_by_lag,
            history: vec![0.0; 9 * channels * QMF_BANDS],
            joined: Vec::new(),
            even: Vec::new(),
            odd: Vec::new(),
            premultiply,
            post,
            even_post,
            plan,
        }
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
    }

    fn transform(&self, values: &[f64]) -> [Complex; PUBLIC_FFT] {
        let mut spectrum = [Complex::new(0.0, 0.0); PUBLIC_FFT];
        for band in 0..QMF_BANDS {
            spectrum[band] = Complex::new(values[band], 0.0) * self.premultiply[band];
        }
        self.plan.process(&mut spectrum);
        for band in 0..QMF_BANDS {
            spectrum[band] *= self.post[band];
        }
        spectrum
    }

    fn process(&mut self, hops: &[f64], slots: usize, output: &mut Vec<Complex>) {
        let stride = self.channels * QMF_BANDS;
        let joined_slots = 9 + slots;
        let history_size = 9 * self.channels * QMF_BANDS;
        let joined_size = joined_slots * self.channels * QMF_BANDS;
        self.joined.resize(joined_size, 0.0);
        self.joined[..history_size].copy_from_slice(&self.history);
        self.joined[history_size..history_size + slots * stride]
            .copy_from_slice(&hops[..slots * stride]);

        let accumulator_size = slots * stride;
        self.even.resize(accumulator_size, 0.0);
        self.odd.resize(accumulator_size, 0.0);
        self.even.fill(0.0);
        self.odd.fill(0.0);

        for first in (0..slots).step_by(32) {
            let block = 32.min(slots - first);
            for lag in 0..PUBLIC_QMF_TAPS {
                let target = if lag % 2 == 0 {
                    &mut self.even
                } else {
                    &mut self.odd
                };
                let row = &self.coefficients_by_lag[lag * QMF_BANDS..(lag + 1) * QMF_BANDS];
                let source_slot = 9 - lag + first;
                let source_base = source_slot * stride;
                let target_base = first * stride;
                for row_index in 0..block * self.channels {
                    let source = &self.joined[source_base + row_index * QMF_BANDS..][..QMF_BANDS];
                    let destination =
                        &mut target[target_base + row_index * QMF_BANDS..][..QMF_BANDS];
                    for band in 0..QMF_BANDS {
                        destination[band] += source[band] * row[band];
                    }
                }
            }
        }

        self.history
            .copy_from_slice(&self.joined[joined_size - history_size..joined_size]);

        output.resize(slots * stride, Complex::new(0.0, 0.0));
        for slot in 0..slots {
            for channel in 0..self.channels {
                let base = (slot * self.channels + channel) * QMF_BANDS;
                let even = self.transform(&self.even[base..base + QMF_BANDS]);
                let odd = self.transform(&self.odd[base..base + QMF_BANDS]);
                let destination = &mut output[base..base + QMF_BANDS];
                for band in 0..QMF_BANDS {
                    destination[band] = odd[band] + even[band] * self.even_post[band];
                }
            }
        }
    }
}

struct PublicHybridAnalysis {
    channels: usize,
    low_by_term: Vec<f64>,
    history: Vec<f64>,
    high_history: Vec<Complex>,
    joined: Vec<f64>,
    next_high_history: Vec<Complex>,
}

impl PublicHybridAnalysis {
    fn new(channels: usize) -> Self {
        let low_kernel = &public::HYBRID_ANALYSIS_LOW_KERNEL;
        let outputs = PUBLIC_JOIN_OUTPUTS;
        let mut low_by_term = vec![0.0f64; PUBLIC_JOIN_TERMS * outputs];
        for lag in 0..PUBLIC_HYBRID_TAPS {
            for point in 0..3 {
                for input in 0..2 {
                    let term = (lag * 3 + point) * 2 + input;
                    let source = (point * 2 + input) * PUBLIC_HYBRID_TAPS + lag;
                    for output in 0..outputs {
                        low_by_term[term * outputs + output] =
                            low_kernel[source * outputs + output];
                    }
                }
            }
        }
        Self {
            channels,
            low_by_term,
            history: vec![0.0; 12 * channels * 6],
            high_history: vec![Complex::new(0.0, 0.0); 6 * channels * 61],
            joined: Vec::new(),
            next_high_history: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
        self.high_history.fill(Complex::new(0.0, 0.0));
    }

    fn process(&mut self, qmf: &[Complex], slots: usize, output: &mut Vec<Complex>) {
        let joined_slots = 12 + slots;
        let history_size = 12 * self.channels * 6;
        let joined_size = joined_slots * self.channels * 6;
        self.joined.resize(joined_size, 0.0);
        self.joined[..history_size].copy_from_slice(&self.history);
        for slot in 0..slots {
            for channel in 0..self.channels {
                let source = &qmf[(slot * self.channels + channel) * QMF_BANDS..][..QMF_BANDS];
                let destination =
                    &mut self.joined[((12 + slot) * self.channels + channel) * 6..][..6];
                for band in 0..3 {
                    destination[band * 2] = source[band].re;
                    destination[band * 2 + 1] = source[band].im;
                }
            }
        }
        output.resize(slots * self.channels * HYBRID_BANDS, Complex::new(0.0, 0.0));
        let rows = slots * self.channels;
        let mut staged = vec![0.0f64; PUBLIC_JOIN_BLOCK * PUBLIC_JOIN_TERMS];
        let mut joined_values = vec![0.0f64; PUBLIC_JOIN_BLOCK * PUBLIC_JOIN_OUTPUTS];
        for first in (0..rows).step_by(PUBLIC_JOIN_BLOCK) {
            let count = PUBLIC_JOIN_BLOCK.min(rows - first);
            for index in 0..count {
                let row = first + index;
                let slot = row / self.channels;
                let channel = row % self.channels;
                let values = &mut staged[index * PUBLIC_JOIN_TERMS..][..PUBLIC_JOIN_TERMS];
                for lag in 0..PUBLIC_HYBRID_TAPS {
                    let source_slot = 12 - lag + slot;
                    let source = &self.joined[(source_slot * self.channels + channel) * 6..][..6];
                    for point in 0..3 {
                        for input in 0..2 {
                            values[(lag * 3 + point) * 2 + input] = source[point * 2 + input];
                        }
                    }
                }
            }
            for index in 0..count {
                let values = &staged[index * PUBLIC_JOIN_TERMS..][..PUBLIC_JOIN_TERMS];
                let destination =
                    &mut joined_values[index * PUBLIC_JOIN_OUTPUTS..][..PUBLIC_JOIN_OUTPUTS];
                destination.fill(0.0);
                for term in 0..PUBLIC_JOIN_TERMS {
                    let value = values[term];
                    if value == 0.0 {
                        continue;
                    }
                    let weights =
                        &self.low_by_term[term * PUBLIC_JOIN_OUTPUTS..][..PUBLIC_JOIN_OUTPUTS];
                    for output_index in 0..PUBLIC_JOIN_OUTPUTS {
                        destination[output_index] += value * weights[output_index];
                    }
                }
            }
            for index in 0..count {
                let destination = &mut output[(first + index) * HYBRID_BANDS..][..HYBRID_BANDS];
                let values = &joined_values[index * PUBLIC_JOIN_OUTPUTS..][..PUBLIC_JOIN_OUTPUTS];
                for band in 0..HYBRID_LOW {
                    destination[band] = Complex::new(values[band * 2], values[band * 2 + 1]);
                }
            }
        }
        self.history
            .copy_from_slice(&self.joined[joined_size - history_size..joined_size]);

        for slot in 0..slots {
            for channel in 0..self.channels {
                let destination = &mut output
                    [(slot * self.channels + channel) * HYBRID_BANDS + HYBRID_LOW..]
                    [..HYBRID_BANDS - HYBRID_LOW];
                if slot < PUBLIC_HYBRID_DELAY {
                    let source = &self.high_history[(slot * self.channels + channel) * 61..][..61];
                    destination.copy_from_slice(source);
                } else {
                    let source = &qmf[((slot - PUBLIC_HYBRID_DELAY) * self.channels + channel)
                        * QMF_BANDS
                        + 3..][..61];
                    destination.copy_from_slice(source);
                }
            }
        }

        self.next_high_history
            .resize(6 * self.channels * 61, Complex::new(0.0, 0.0));
        for entry in 0..6 {
            let combined = slots + entry;
            for channel in 0..self.channels {
                let destination =
                    &mut self.next_high_history[(entry * self.channels + channel) * 61..][..61];
                if combined < PUBLIC_HYBRID_DELAY {
                    let source =
                        &self.high_history[(combined * self.channels + channel) * 61..][..61];
                    destination.copy_from_slice(source);
                } else {
                    let source = &qmf[((combined - PUBLIC_HYBRID_DELAY) * self.channels
                        + channel)
                        * QMF_BANDS
                        + 3..][..61];
                    destination.copy_from_slice(source);
                }
            }
        }
        self.high_history.copy_from_slice(&self.next_high_history);
    }
}

#[derive(Clone, Copy)]
struct PublicHybridEntry {
    index: [usize; 4],
    gain: f64,
}

struct PublicHybridSynthesis {
    mapping: Vec<PublicHybridEntry>,
}

impl PublicHybridSynthesis {
    fn new() -> Self {
        let indices = &public::HYBRID_SYNTHESIS_INDICES;
        let values = &public::HYBRID_SYNTHESIS_VALUES;
        let rows = indices.len() / 4;
        let mut mapping = Vec::with_capacity(rows);
        for index in 0..rows {
            mapping.push(PublicHybridEntry {
                index: [
                    indices[index * 4] as usize,
                    indices[index * 4 + 1] as usize,
                    indices[index * 4 + 2] as usize,
                    indices[index * 4 + 3] as usize,
                ],
                gain: values[index],
            });
        }
        Self { mapping }
    }

    fn process(
        &self,
        hybrid: &[Complex],
        slots: usize,
        channels: usize,
        output: &mut Vec<Complex>,
    ) {
        let rows = slots * channels;
        let mut real = vec![0.0f64; rows * QMF_BANDS];
        let mut imaginary = vec![0.0f64; rows * QMF_BANDS];
        for row in 0..rows {
            let source = &hybrid[row * HYBRID_BANDS..][..HYBRID_BANDS];
            for entry in &self.mapping {
                let value = if entry.index[1] == 0 {
                    source[entry.index[0]].re
                } else {
                    source[entry.index[0]].im
                };
                if value == 0.0 {
                    continue;
                }
                let destination = if entry.index[3] == 0 {
                    &mut real[row * QMF_BANDS..][..QMF_BANDS]
                } else {
                    &mut imaginary[row * QMF_BANDS..][..QMF_BANDS]
                };
                destination[entry.index[2]] += value * entry.gain;
            }
        }
        output.resize(rows * QMF_BANDS, Complex::new(0.0, 0.0));
        for index in 0..rows * QMF_BANDS {
            output[index] = Complex::new(real[index], imaginary[index]);
        }
    }
}

struct PublicQmfSynthesis {
    channels: usize,
    basis_by_tap: Vec<f64>,
    taps: &'static [f64],
    history: Vec<f64>,
    flat: Vec<f64>,
    joined: Vec<f64>,
}

impl PublicQmfSynthesis {
    fn new(channels: usize) -> Self {
        let basis = &public::QMF_SYNTHESIS_BASIS;
        let taps = &public::QMF_SYNTHESIS_TAPS;
        let bands = QMF_BANDS;
        let ranks = PUBLIC_SYNTHESIS_RANK;
        let taps_per_band = PUBLIC_FFT;
        let mut basis_by_tap = vec![0.0f64; bands * taps_per_band * ranks];
        for band in 0..bands {
            for tap in 0..taps_per_band {
                for rank in 0..ranks {
                    basis_by_tap[(band * taps_per_band + tap) * ranks + rank] =
                        basis[(band * ranks + rank) * taps_per_band + tap];
                }
            }
        }
        Self {
            channels,
            basis_by_tap,
            taps,
            history: vec![0.0; 9 * channels * QMF_BANDS * PUBLIC_SYNTHESIS_RANK],
            flat: Vec::new(),
            joined: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
    }

    fn process(&mut self, qmf: &[Complex], slots: usize, output: &mut Vec<f64>) {
        let rows = slots * self.channels;
        let flat_size = rows * PUBLIC_FFT;
        self.flat.resize(flat_size, 0.0);
        for row in 0..rows {
            for band in 0..QMF_BANDS {
                self.flat[row * PUBLIC_FFT + band * 2] = qmf[row * QMF_BANDS + band].re;
                self.flat[row * PUBLIC_FFT + band * 2 + 1] = qmf[row * QMF_BANDS + band].im;
            }
        }
        let history_size = 9 * self.channels * QMF_BANDS * PUBLIC_SYNTHESIS_RANK;
        let joined_size = history_size + rows * QMF_BANDS * PUBLIC_SYNTHESIS_RANK;
        self.joined.resize(joined_size, 0.0);
        self.joined[..history_size].copy_from_slice(&self.history);
        {
            let values = &self.flat;
            let basis = &self.basis_by_tap;
            let destination = &mut self.joined[history_size..];
            for row in 0..rows {
                let source = &values[row * PUBLIC_FFT..][..PUBLIC_FFT];
                let out = &mut destination[row * QMF_BANDS * PUBLIC_SYNTHESIS_RANK..]
                    [..QMF_BANDS * PUBLIC_SYNTHESIS_RANK];
                for band in 0..QMF_BANDS {
                    for rank in 0..PUBLIC_SYNTHESIS_RANK {
                        let mut sum = 0.0;
                        for tap in 0..PUBLIC_FFT {
                            sum += source[tap]
                                * basis[(band * PUBLIC_FFT + tap) * PUBLIC_SYNTHESIS_RANK + rank];
                        }
                        out[band * PUBLIC_SYNTHESIS_RANK + rank] = sum;
                    }
                }
            }
        }
        self.history
            .copy_from_slice(&self.joined[joined_size - history_size..joined_size]);

        output.clear();
        output.resize(rows * QMF_BANDS, 0.0);
        for lag in 0..PUBLIC_SYNTHESIS_TAPS {
            for slot in 0..slots {
                let source_slot = 9 - lag + slot;
                for channel in 0..self.channels {
                    let source = &self.joined[(source_slot * self.channels + channel)
                        * QMF_BANDS
                        * PUBLIC_SYNTHESIS_RANK..][..QMF_BANDS * PUBLIC_SYNTHESIS_RANK];
                    let destination =
                        &mut output[(slot * self.channels + channel) * QMF_BANDS..][..QMF_BANDS];
                    for band in 0..QMF_BANDS {
                        let mut sum = 0.0;
                        for rank in 0..PUBLIC_SYNTHESIS_RANK {
                            sum += source[band * PUBLIC_SYNTHESIS_RANK + rank]
                                * self.taps[(band * PUBLIC_SYNTHESIS_TAPS + lag)
                                    * PUBLIC_SYNTHESIS_RANK
                                    + rank];
                        }
                        destination[band] += sum;
                    }
                }
            }
        }
    }
}

/// The shared public bank.
pub struct PublicFilterbank {
    channels: usize,
    qmf_analysis: PublicQmfAnalysis,
    hybrid_analysis: PublicHybridAnalysis,
    hybrid_synthesis: PublicHybridSynthesis,
    qmf_synthesis: PublicQmfSynthesis,
}

impl PublicFilterbank {
    pub fn new(channels: usize) -> Self {
        Self {
            channels,
            qmf_analysis: PublicQmfAnalysis::new(channels),
            hybrid_analysis: PublicHybridAnalysis::new(channels),
            hybrid_synthesis: PublicHybridSynthesis::new(),
            qmf_synthesis: PublicQmfSynthesis::new(channels),
        }
    }
}

impl Filterbank for PublicFilterbank {
    fn kind(&self) -> FilterbankKind {
        FilterbankKind::Public
    }

    fn reset(&mut self) {
        self.qmf_analysis.reset();
        self.hybrid_analysis.reset();
        self.qmf_synthesis.reset();
    }

    fn analyze_qmf(&mut self, hops: &[f64], slots: usize, qmf: &mut Vec<Complex>) {
        self.qmf_analysis.process(hops, slots, qmf);
    }

    fn analyze_hybrid(&mut self, qmf: &[Complex], slots: usize, hybrid: &mut Vec<Complex>) {
        self.hybrid_analysis.process(qmf, slots, hybrid);
    }

    fn synthesize_hybrid(&mut self, hybrid: &[Complex], slots: usize, qmf: &mut Vec<Complex>) {
        self.hybrid_synthesis
            .process(hybrid, slots, self.channels, qmf);
    }

    fn synthesize_qmf(&mut self, qmf: &[Complex], slots: usize, hops: &mut Vec<f64>) {
        self.qmf_synthesis.process(qmf, slots, hops);
    }
}

// ---------------------------------------------------------------------------
// Captured Rosella bank
// ---------------------------------------------------------------------------

const ROSELLA_QMF_TAPS: usize = 10;
const ROSELLA_HYBRID_TAPS: usize = 11;
const ROSELLA_HYBRID_DELAY: usize = 3;
const ROSELLA_HYBRID_ORDER: [usize; 16] = [0, 7, 1, 6, 2, 9, 3, 4, 10, 5, 11, 15, 8, 12, 13, 14];

fn tap_sign(tap: usize) -> f64 {
    if (tap / 2).is_multiple_of(2) {
        1.0
    } else {
        -1.0
    }
}

fn rosella_complex(table: &[(f64, f64)], index: usize) -> Complex {
    Complex::new(table[index].0, table[index].1)
}

/// The bank captured from the reference Rosella headphone renderer.
pub struct RosellaFilterbank {
    channels: usize,
    analysis_position: usize,
    hybrid_position: usize,
    synthesis_position: usize,
    analysis_history: Vec<f64>,
    hybrid_history: Vec<Complex>,
    synthesis_history: Vec<Complex>,
    spectrum: Vec<Complex>,
    hybrid_eight: Vec<Complex>,
    hybrid_four: Vec<Complex>,
    qmf_plan: Arc<dyn RustFft<f64>>,
    eight_plan: Arc<dyn RustFft<f64>>,
    four_plan: Arc<dyn RustFft<f64>>,
}

impl RosellaFilterbank {
    pub fn new(channels: usize) -> Self {
        let mut planner = FftPlanner::<f64>::new();
        let qmf_plan = fft_plan(&mut planner, QMF_BANDS, false);
        let eight_plan = fft_plan(&mut planner, 8, false);
        let four_plan = fft_plan(&mut planner, 4, false);
        Self {
            channels,
            analysis_position: 0,
            hybrid_position: 0,
            synthesis_position: 0,
            analysis_history: vec![0.0; ROSELLA_QMF_TAPS * channels * QMF_BANDS],
            hybrid_history: vec![
                Complex::new(0.0, 0.0);
                ROSELLA_HYBRID_TAPS * channels * QMF_BANDS
            ],
            synthesis_history: vec![
                Complex::new(0.0, 0.0);
                ROSELLA_QMF_TAPS * channels * QMF_BANDS
            ],
            spectrum: vec![Complex::new(0.0, 0.0); QMF_BANDS],
            hybrid_eight: vec![Complex::new(0.0, 0.0); 8],
            hybrid_four: vec![Complex::new(0.0, 0.0); 4],
            qmf_plan,
            eight_plan,
            four_plan,
        }
    }
}

impl Filterbank for RosellaFilterbank {
    fn kind(&self) -> FilterbankKind {
        FilterbankKind::Captured
    }

    fn reset(&mut self) {
        self.analysis_position = 0;
        self.hybrid_position = 0;
        self.synthesis_position = 0;
        self.analysis_history.fill(0.0);
        self.hybrid_history.fill(Complex::new(0.0, 0.0));
        self.synthesis_history.fill(Complex::new(0.0, 0.0));
    }

    fn analyze_qmf(&mut self, hops: &[f64], slots: usize, qmf: &mut Vec<Complex>) {
        let stride = self.channels * QMF_BANDS;
        qmf.resize(slots * stride, Complex::new(0.0, 0.0));
        for slot in 0..slots {
            self.analysis_history[self.analysis_position * stride..][..stride]
                .copy_from_slice(&hops[slot * stride..][..stride]);
            for channel in 0..self.channels {
                for band in 0..QMF_BANDS {
                    let mut even = 0.0;
                    let mut odd = 0.0;
                    for tap in 0..ROSELLA_QMF_TAPS {
                        let history_slot = (self.analysis_position + tap + 1) % ROSELLA_QMF_TAPS;
                        let value = self.analysis_history
                            [history_slot * stride + channel * QMF_BANDS + band];
                        let contribution = value
                            * 0.5
                            * rosella::PROTOTYPE[band * ROSELLA_QMF_TAPS + tap]
                            * tap_sign(tap);
                        if tap % 2 == 0 {
                            even += contribution;
                        } else {
                            odd += contribution;
                        }
                    }
                    self.spectrum[band] =
                        Complex::new(odd, even) * rosella_complex(&rosella::ROTATION, band);
                }
                self.qmf_plan.process(&mut self.spectrum);
                let destination = &mut qmf[slot * stride + channel * QMF_BANDS..][..QMF_BANDS];
                for band in 0..QMF_BANDS / 2 {
                    destination[2 * band] = self.spectrum[band] / 64.0;
                    destination[2 * band + 1] = self.spectrum[63 - band].conj() / 64.0;
                }
            }
            self.analysis_position = (self.analysis_position + 1) % ROSELLA_QMF_TAPS;
        }
    }

    fn analyze_hybrid(&mut self, qmf: &[Complex], slots: usize, hybrid: &mut Vec<Complex>) {
        let stride = self.channels * QMF_BANDS;
        hybrid.resize(slots * self.channels * HYBRID_BANDS, Complex::new(0.0, 0.0));
        for slot in 0..slots {
            self.hybrid_history[self.hybrid_position * stride..][..stride]
                .copy_from_slice(&qmf[slot * stride..][..stride]);
            for channel in 0..self.channels {
                let mut low = [Complex::new(0.0, 0.0); 16];
                for band in 0..3 {
                    let size = if band == 0 { 8 } else { 4 };
                    let spectrum = if band == 0 {
                        &mut self.hybrid_eight
                    } else {
                        &mut self.hybrid_four
                    };
                    spectrum.fill(Complex::new(0.0, 0.0));
                    for tap in 0..ROSELLA_HYBRID_TAPS {
                        let history_slot = (self.hybrid_position + tap + 1) % ROSELLA_HYBRID_TAPS;
                        let bin = (tap + ROSELLA_HYBRID_DELAY + 1 + 2 * size - ROSELLA_HYBRID_TAPS)
                            % size;
                        spectrum[bin] += self.hybrid_history
                            [history_slot * stride + channel * QMF_BANDS + band]
                            * rosella_complex(
                                &rosella::HYBRID,
                                (if band == 0 { 0 } else { ROSELLA_HYBRID_TAPS }) + tap,
                            );
                    }
                    if band == 0 {
                        self.eight_plan.process(&mut self.hybrid_eight);
                    } else {
                        self.four_plan.process(&mut self.hybrid_four);
                    }
                    let spectrum = if band == 0 {
                        &self.hybrid_eight
                    } else {
                        &self.hybrid_four
                    };
                    let offset = if band == 0 { 0 } else { 4 + band * 4 };
                    low[offset..offset + size].copy_from_slice(spectrum);
                }
                let destination =
                    &mut hybrid[(slot * self.channels + channel) * HYBRID_BANDS..][..HYBRID_BANDS];
                for band in 0..16 {
                    destination[band] = low[ROSELLA_HYBRID_ORDER[band]];
                }
                destination[1] = destination[1].conj();
                destination[3] = destination[3].conj();
                let delayed = (self.hybrid_position + ROSELLA_HYBRID_TAPS - ROSELLA_HYBRID_DELAY)
                    % ROSELLA_HYBRID_TAPS;
                let source = &self.hybrid_history[delayed * stride + channel * QMF_BANDS + 3..]
                    [..QMF_BANDS - 3];
                destination[16..].copy_from_slice(source);
            }
            self.hybrid_position = (self.hybrid_position + 1) % ROSELLA_HYBRID_TAPS;
        }
    }

    fn synthesize_hybrid(&mut self, hybrid: &[Complex], slots: usize, qmf: &mut Vec<Complex>) {
        qmf.resize(slots * self.channels * QMF_BANDS, Complex::new(0.0, 0.0));
        for row in 0..slots * self.channels {
            let source = &hybrid[row * HYBRID_BANDS..][..HYBRID_BANDS];
            let destination = &mut qmf[row * QMF_BANDS..][..QMF_BANDS];
            destination[0] = source[0]
                + source[2]
                + source[4]
                + source[6]
                + source[7]
                + source[9]
                + source[3].conj()
                + source[1].conj();
            destination[1] = source[12] + source[5] + source[8] + source[10];
            destination[2] = source[13] + source[14] + source[15] + source[11];
            destination[3..].copy_from_slice(&source[16..][..QMF_BANDS - 3]);
        }
    }

    fn synthesize_qmf(&mut self, qmf: &[Complex], slots: usize, hops: &mut Vec<f64>) {
        let stride = self.channels * QMF_BANDS;
        hops.resize(slots * stride, 0.0);
        for slot in 0..slots {
            for channel in 0..self.channels {
                let source = &qmf[slot * stride + channel * QMF_BANDS..][..QMF_BANDS];
                for band in 0..QMF_BANDS / 2 {
                    self.spectrum[band] = source[2 * band].conj();
                    self.spectrum[32 + band] = source[63 - 2 * band];
                }
                self.qmf_plan.process(&mut self.spectrum);
                let position = self.synthesis_position;
                let current = &mut self.synthesis_history
                    [position * stride + channel * QMF_BANDS..][..QMF_BANDS];
                for band in 0..QMF_BANDS {
                    current[band] = self.spectrum[band] * rosella_complex(&rosella::ROTATION, band);
                }
                for band in 0..QMF_BANDS {
                    let mut value = 0.0;
                    for lag in 0..ROSELLA_QMF_TAPS {
                        let history_slot =
                            (self.synthesis_position + ROSELLA_QMF_TAPS - lag) % ROSELLA_QMF_TAPS;
                        let sample = self.synthesis_history
                            [history_slot * stride + channel * QMF_BANDS + band];
                        let component = if lag % 2 == 0 { sample.re } else { sample.im };
                        value += component
                            * rosella::PROTOTYPE
                                [(QMF_BANDS - 1 - band) * ROSELLA_QMF_TAPS + 9 - lag]
                            * tap_sign(lag)
                            * 2.0;
                    }
                    hops[slot * stride + channel * QMF_BANDS + band] = value;
                }
            }
            self.synthesis_position = (self.synthesis_position + 1) % ROSELLA_QMF_TAPS;
        }
    }
}
