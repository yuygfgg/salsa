//! Shared 1920-point STFT analysis, synthesis and per-band matrices.

use std::sync::Arc;

use num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use salsa_spatial::{
    FFT_BINS, FFT_SIZE, HOP_SAMPLES, PARAM_BAND_COUNT, bin_weight, parameter_band_for_bin,
    parameter_band_range,
};

/// Nonnegative-frequency complex spectrum including DC and Nyquist.
pub type Spectrum = Vec<Complex<f64>>;

const NEGLIGIBLE_WINDOW_POWER: f64 = 1e-24;

/// STFT geometry. The default is the salsa-spatial v1 profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StftConfig {
    pub fft_size: usize,
    pub hop: usize,
}

impl Default for StftConfig {
    fn default() -> Self {
        Self {
            fft_size: FFT_SIZE,
            hop: HOP_SAMPLES,
        }
    }
}

impl StftConfig {
    pub const fn new(fft_size: usize, hop: usize) -> Self {
        Self { fft_size, hop }
    }

    pub const fn bins(&self) -> usize {
        self.fft_size / 2 + 1
    }

    fn validate(&self) {
        assert!(self.fft_size >= 2 && self.fft_size.is_multiple_of(2));
        assert!(self.hop > 0 && self.hop <= self.fft_size);
    }
}

/// Periodic sine analysis/synthesis window:
/// `sqrt(0.5 - 0.5*cos(2*pi*n/N))`.
pub fn window(fft_size: usize) -> Vec<f64> {
    assert!(fft_size > 0);
    (0..fft_size)
        .map(|n| {
            let angle = 2.0 * std::f64::consts::PI * n as f64 / fft_size as f64;
            (0.5 - 0.5 * angle.cos()).max(0.0).sqrt()
        })
        .collect()
}

/// Streaming STFT analyzer aligned so frame `k` is centered on sample `k*hop`.
///
/// The first frame is centered on sample zero, so the region before the start
/// is zero padded. Spectra are emitted as soon as the complete future half of
/// the window is available. [`Stft::flush`] zero pads the tail.
pub struct Stft {
    config: StftConfig,
    window: Vec<f64>,
    fft: Arc<dyn Fft<f64>>,
    half: i64,
    buffer: Vec<f64>,
    buffer_start: i64,
    total: i64,
    next_center: i64,
}

impl Stft {
    pub fn new() -> Self {
        Self::with_config(StftConfig::default())
    }

    pub fn with_config(config: StftConfig) -> Self {
        config.validate();
        let window = window(config.fft_size);
        let fft = FftPlanner::<f64>::new().plan_fft_forward(config.fft_size);
        Self {
            config,
            window,
            fft,
            half: (config.fft_size / 2) as i64,
            buffer: Vec::new(),
            buffer_start: 0,
            total: 0,
            next_center: 0,
        }
    }

    pub fn config(&self) -> StftConfig {
        self.config
    }

    /// Push samples and return every hop spectrum that is now complete.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Spectrum> {
        if !samples.is_empty() {
            self.buffer
                .extend(samples.iter().map(|&sample| f64::from(sample)));
            self.total += samples.len() as i64;
        }
        self.emit(false)
    }

    /// Emit all remaining frames, zero padding the future.
    pub fn flush(&mut self) -> Vec<Spectrum> {
        self.emit(true)
    }

    pub fn finish(&mut self) -> Vec<Spectrum> {
        self.flush()
    }

    fn sample(&self, absolute: i64) -> f64 {
        if absolute < 0 || absolute >= self.total {
            return 0.0;
        }
        let index = absolute - self.buffer_start;
        if index < 0 || index as usize >= self.buffer.len() {
            return 0.0;
        }
        self.buffer[index as usize]
    }

    fn emit(&mut self, flushing: bool) -> Vec<Spectrum> {
        if self.total <= 0 {
            return Vec::new();
        }
        let mut output = Vec::new();
        loop {
            let center = self.next_center;
            if flushing {
                if center >= self.total + self.half {
                    break;
                }
            } else if center + self.half > self.total {
                break;
            }
            if center < 0 {
                self.next_center += self.config.hop as i64;
                continue;
            }
            output.push(self.compute_frame(center));
            self.next_center += self.config.hop as i64;
            self.trim();
        }
        output
    }

    fn compute_frame(&mut self, center: i64) -> Spectrum {
        let size = self.config.fft_size;
        let start = center - self.half;
        let mut scratch = Vec::with_capacity(size);
        for index in 0..size {
            let value = self.sample(start + index as i64) * self.window[index];
            scratch.push(Complex::new(value, 0.0));
        }
        self.fft.process(&mut scratch);
        scratch.truncate(self.config.bins());
        scratch
    }

    fn trim(&mut self) {
        let keep = self.next_center - self.half;
        if keep <= self.buffer_start {
            return;
        }
        let cut = ((keep - self.buffer_start) as usize).min(self.buffer.len());
        self.buffer.drain(0..cut);
        self.buffer_start += cut as i64;
    }
}

impl Default for Stft {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience batch analyzer with the same frame alignment as [`Stft`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Analysis {
    pub config: StftConfig,
}

impl Analysis {
    pub fn new() -> Self {
        Self {
            config: StftConfig::default(),
        }
    }

    pub fn with_config(config: StftConfig) -> Self {
        Self { config }
    }

    pub fn run(&self, samples: &[f32]) -> Vec<Spectrum> {
        analyze(samples, self.config)
    }
}

/// Analyze a complete signal. Frame `k` is centered on sample `k*hop`.
pub fn analyze(samples: &[f32], config: StftConfig) -> Vec<Spectrum> {
    let mut stft = Stft::with_config(config);
    let mut spectra = stft.push(samples);
    spectra.extend(stft.flush());
    spectra
}

/// Batch analyzer that shares one FFT plan and window across many signals.
///
/// The encoder analyzes every source object and core channel for every tile.
/// Creating a fresh [`Stft`] (and therefore a fresh FFT plan) per signal costs
/// more than the transforms themselves, so this type plans once and reuses the
/// plan for all channels.
pub struct BatchAnalyzer {
    config: StftConfig,
    window: Vec<f64>,
    fft: Arc<dyn Fft<f64>>,
}

impl BatchAnalyzer {
    pub fn new(config: StftConfig) -> Self {
        config.validate();
        Self {
            config,
            window: window(config.fft_size),
            fft: FftPlanner::<f64>::new().plan_fft_forward(config.fft_size),
        }
    }

    /// Analyze a complete signal with the same frame alignment as
    /// [`Stft::push`] followed by [`Stft::flush`].
    pub fn analyze(&self, samples: &[f32]) -> Vec<Spectrum> {
        let total = samples.len() as i64;
        let size = self.config.fft_size;
        let half = (size / 2) as i64;
        let hop = self.config.hop as i64;
        let bins = self.config.bins();
        let mut spectra = Vec::new();
        let mut center = 0i64;
        let mut scratch = vec![Complex::new(0.0, 0.0); size];
        while center < total + half {
            if center >= 0 {
                let start = center - half;
                scratch.fill(Complex::new(0.0, 0.0));
                for (index, (slot, &window)) in scratch.iter_mut().zip(&self.window).enumerate() {
                    let sample_index = start + index as i64;
                    if sample_index >= 0 && sample_index < total {
                        let value = f64::from(samples[sample_index as usize]) * window;
                        *slot = Complex::new(value, 0.0);
                    }
                }
                self.fft.process(&mut scratch);
                spectra.push(scratch[..bins].to_vec());
            }
            center += hop;
        }
        spectra
    }
}

/// Streaming overlap-add synthesizer using the squared analysis window as the
/// normalization denominator.
pub struct Synthesis {
    config: StftConfig,
    window: Vec<f64>,
    ifft: Arc<dyn Fft<f64>>,
    ola: Vec<f64>,
    norm: Vec<f64>,
    base: i64,
    next_center: i64,
    finalized: i64,
    ready: Vec<f32>,
    produced: usize,
    total: Option<usize>,
}

impl Synthesis {
    pub fn new() -> Self {
        Self::with_config(StftConfig::default())
    }

    pub fn with_config(config: StftConfig) -> Self {
        config.validate();
        let window = window(config.fft_size);
        let ifft = FftPlanner::<f64>::new().plan_fft_inverse(config.fft_size);
        Self {
            config,
            window,
            ifft,
            ola: Vec::new(),
            norm: Vec::new(),
            base: 0,
            next_center: 0,
            finalized: 0,
            ready: Vec::new(),
            produced: 0,
            total: None,
        }
    }

    pub fn with_total(config: StftConfig, total_samples: usize) -> Self {
        let mut synthesis = Self::with_config(config);
        synthesis.total = Some(total_samples);
        synthesis
    }

    pub fn config(&self) -> StftConfig {
        self.config
    }

    pub fn set_total_samples(&mut self, total_samples: usize) {
        self.total = Some(total_samples);
    }

    /// Add one positive-frequency spectrum. Frames are consumed in analysis
    /// order starting with the frame centered on sample zero.
    pub fn push_spectrum(&mut self, spectrum: &[Complex<f64>]) {
        assert_eq!(spectrum.len(), self.config.bins());
        let size = self.config.fft_size;
        let half = (size / 2) as i64;
        let center = self.next_center;
        let start = center - half;
        let end = start + size as i64;
        self.ensure(start, end);

        let mut full = vec![Complex::new(0.0, 0.0); size];
        full[0] = Complex::new(spectrum[0].re, 0.0);
        for bin in 1..size / 2 {
            full[bin] = spectrum[bin];
            full[size - bin] = spectrum[bin].conj();
        }
        full[size / 2] = Complex::new(spectrum[size / 2].re, 0.0);
        self.ifft.process(&mut full);

        let scale = 1.0 / size as f64;
        let offset = (start - self.base) as usize;
        for (index, (&sample, &window)) in full.iter().zip(&self.window).enumerate() {
            let value = sample.re * scale * window;
            self.ola[offset + index] += value;
            self.norm[offset + index] += window * window;
        }

        self.next_center += self.config.hop as i64;
        let next_start = self.next_center - half;
        self.finalize_until(next_start);
    }

    pub fn push_spectra(&mut self, spectra: &[Spectrum]) {
        for spectrum in spectra {
            self.push_spectrum(spectrum);
        }
    }

    /// Return all samples that are final and have not been returned yet.
    pub fn drain(&mut self) -> Vec<f32> {
        let output = std::mem::take(&mut self.ready);
        self.produced += output.len();
        output
    }

    /// Flush the overlap-add tail and return the remaining samples up to
    /// `total_samples` for the whole stream (across earlier drains).
    pub fn finish(&mut self, total_samples: usize) -> Vec<f32> {
        self.total = Some(total_samples);
        let end = self.base + self.ola.len() as i64;
        self.finalize_until(end);
        let needed = total_samples.saturating_sub(self.produced);
        let mut output = std::mem::take(&mut self.ready);
        output.truncate(needed);
        self.produced += output.len();
        output
    }

    fn ensure(&mut self, start: i64, end: i64) {
        if self.ola.is_empty() {
            self.base = start;
            self.ola.resize((end - start) as usize, 0.0);
            self.norm.resize((end - start) as usize, 0.0);
            return;
        }
        if start < self.base {
            let prepend = (self.base - start) as usize;
            self.ola.splice(0..0, std::iter::repeat_n(0.0, prepend));
            self.norm.splice(0..0, std::iter::repeat_n(0.0, prepend));
            self.base = start;
        }
        let current_end = self.base + self.ola.len() as i64;
        if end > current_end {
            let extend = (end - current_end) as usize;
            self.ola.resize(self.ola.len() + extend, 0.0);
            self.norm.resize(self.norm.len() + extend, 0.0);
        }
    }

    fn finalize_until(&mut self, limit: i64) {
        let limit = match self.total {
            Some(total) => limit.min(total as i64),
            None => limit,
        };
        while self.finalized < limit {
            let absolute = self.finalized;
            let index = absolute - self.base;
            let value = if index >= 0 && (index as usize) < self.ola.len() {
                let denominator = self.norm[index as usize];
                if denominator > NEGLIGIBLE_WINDOW_POWER {
                    self.ola[index as usize] / denominator
                } else {
                    0.0
                }
            } else {
                0.0
            };
            self.ready.push(value as f32);
            self.finalized += 1;
        }
        let cut = (self.finalized - self.base).max(0) as usize;
        let cut = cut.min(self.ola.len());
        if cut > 0 {
            self.ola.drain(0..cut);
            self.norm.drain(0..cut);
            self.base += cut as i64;
        }
    }
}

impl Default for Synthesis {
    fn default() -> Self {
        Self::new()
    }
}

/// Synthesize a complete spectrum sequence and trim to `total_samples`.
pub fn synthesize(spectra: &[Spectrum], total_samples: usize) -> Vec<f32> {
    let mut synthesis = Synthesis::new();
    synthesis.push_spectra(spectra);
    synthesis.finish(total_samples)
}

/// One complex matrix per parameter band. Matrices are stored row-major.
#[derive(Debug, Clone)]
pub struct BandMatrices {
    pub rows: usize,
    pub cols: usize,
    data: Vec<Vec<Complex<f64>>>,
}

impl BandMatrices {
    pub fn zeros(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            data: (0..PARAM_BAND_COUNT)
                .map(|_| vec![Complex::new(0.0, 0.0); rows * cols])
                .collect(),
        }
    }

    pub fn from_fn(
        rows: usize,
        cols: usize,
        mut value: impl FnMut(usize, usize, usize) -> Complex<f64>,
    ) -> Self {
        let data = (0..PARAM_BAND_COUNT)
            .map(|band| {
                let mut matrix = vec![Complex::new(0.0, 0.0); rows * cols];
                for row in 0..rows {
                    for col in 0..cols {
                        matrix[row * cols + col] = value(band, row, col);
                    }
                }
                matrix
            })
            .collect();
        Self { rows, cols, data }
    }

    pub fn get(&self, band: usize, row: usize, col: usize) -> Complex<f64> {
        assert!(band < PARAM_BAND_COUNT && row < self.rows && col < self.cols);
        self.data[band][row * self.cols + col]
    }

    pub fn set(&mut self, band: usize, row: usize, col: usize, value: Complex<f64>) {
        assert!(band < PARAM_BAND_COUNT && row < self.rows && col < self.cols);
        self.data[band][row * self.cols + col] = value;
    }

    pub fn band(&self, band: usize) -> &[Complex<f64>] {
        assert!(band < PARAM_BAND_COUNT);
        &self.data[band]
    }
}

fn apply_matrix_for_bin(
    matrices: &BandMatrices,
    band: usize,
    bin: usize,
    input: &[Complex<f64>],
    output: &mut [Complex<f64>],
    endpoint: bool,
) {
    for row in 0..matrices.rows {
        let mut sum = Complex::new(0.0, 0.0);
        for col in 0..matrices.cols {
            let mut coefficient = matrices.get(band, row, col);
            if endpoint {
                coefficient = Complex::new(coefficient.re, 0.0);
            }
            sum += coefficient * input[col * FFT_BINS + bin];
        }
        output[row * FFT_BINS + bin] = sum;
    }
}

/// Apply one matrix per parameter band to a positive-frequency multichannel
/// spectrum stored channel-major (`channel * FFT_BINS + bin`).
///
/// Negative frequencies are represented implicitly by conjugate symmetry, so
/// only nonnegative bins are transformed. At DC and Nyquist the real part of
/// the matrix is used, matching the real endpoint convention.
pub fn apply_band_matrices(matrices: &BandMatrices, input: &[Complex<f64>]) -> Vec<Complex<f64>> {
    assert_eq!(input.len(), matrices.cols * FFT_BINS);
    let mut output = vec![Complex::new(0.0, 0.0); matrices.rows * FFT_BINS];
    for band in 0..PARAM_BAND_COUNT {
        let range = parameter_band_range(band);
        for bin in range {
            let endpoint = bin == 0 || bin == FFT_BINS - 1;
            apply_matrix_for_bin(matrices, band, bin, input, &mut output, endpoint);
        }
    }
    output
}

/// Apply the matrices to a full FFT-size spectrum, using the conjugate matrix
/// for negative frequencies so a conjugate-symmetric input stays symmetric.
pub fn apply_band_matrices_full(
    matrices: &BandMatrices,
    input: &[Complex<f64>],
    fft_size: usize,
) -> Vec<Complex<f64>> {
    assert!(fft_size >= 2 && fft_size.is_multiple_of(2));
    assert_eq!(input.len(), matrices.cols * fft_size);
    let half = fft_size / 2;
    let mut output = vec![Complex::new(0.0, 0.0); matrices.rows * fft_size];
    for bin in 0..fft_size {
        let (positive, conjugated) = if bin <= half {
            (bin, false)
        } else {
            (fft_size - bin, true)
        };
        let band = parameter_band_for_bin(positive).expect("bin in range");
        let endpoint = positive == 0 || positive == half;
        for row in 0..matrices.rows {
            let mut sum = Complex::new(0.0, 0.0);
            for col in 0..matrices.cols {
                let mut coefficient = matrices.get(band, row, col);
                if conjugated {
                    coefficient = coefficient.conj();
                }
                if endpoint {
                    coefficient = Complex::new(coefficient.re, 0.0);
                }
                sum += coefficient * input[col * fft_size + bin];
            }
            output[row * fft_size + bin] = sum;
        }
    }
    output
}

/// Weighted window power for a positive-frequency bin, used by statistics.
pub fn spectrum_bin_weight(bin: usize) -> f64 {
    bin_weight(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relative_rms(reference: &[f32], actual: &[f32]) -> f64 {
        assert_eq!(reference.len(), actual.len());
        let mut numerator = 0.0;
        let mut denominator = 0.0;
        for (&a, &b) in reference.iter().zip(actual) {
            let difference = f64::from(a) - f64::from(b);
            numerator += difference * difference;
            denominator += f64::from(a) * f64::from(a);
        }
        (numerator / denominator.max(1e-30)).sqrt()
    }

    fn test_signal(length: usize) -> Vec<f32> {
        (0..length)
            .map(|index| {
                let t = index as f64;
                (0.4 * (0.031 * t).sin() + 0.2 * (0.173 * t).cos() + 0.1 * (0.517 * t + 0.2).sin())
                    as f32
            })
            .collect()
    }

    #[test]
    fn window_is_periodic_and_normalized() {
        let window = window(FFT_SIZE);
        assert_eq!(window.len(), FFT_SIZE);
        for &value in &window {
            assert!((0.0..=1.0).contains(&value));
        }
        assert!(window[0].abs() < 1e-15);
        assert!((window[FFT_SIZE / 2] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn unchanged_spectra_roundtrip_below_1e_6() {
        for &length in &[1usize, 239, 240, 241, 960, 4_800, 11_037] {
            let signal = test_signal(length);
            let spectra = analyze(&signal, StftConfig::default());
            let reconstructed = synthesize(&spectra, length);
            assert_eq!(reconstructed.len(), length);
            let error = relative_rms(&signal, &reconstructed);
            assert!(error < 1e-6, "length {length}: {error}");
        }
    }

    #[test]
    fn impulses_at_stream_and_tile_boundaries_roundtrip() {
        for &length in &[960usize, 4_800, 5_000] {
            for &position in &[0usize, 1, 239, 240, 959, 960, 4_799, length - 1] {
                if position >= length {
                    continue;
                }
                let mut signal = vec![0.0f32; length];
                signal[position] = 1.0;
                let spectra = analyze(&signal, StftConfig::default());
                let reconstructed = synthesize(&spectra, length);
                let error = relative_rms(&signal, &reconstructed);
                assert!(error < 1e-6, "length {length}, impulse {position}: {error}");
            }
        }
    }

    #[test]
    fn streaming_matches_batch() {
        let signal = test_signal(3_000);
        let batch = analyze(&signal, StftConfig::default());
        let mut stft = Stft::new();
        let mut streaming = Vec::new();
        for chunk in signal.chunks(37) {
            streaming.extend(stft.push(chunk));
        }
        streaming.extend(stft.flush());
        assert_eq!(streaming.len(), batch.len());
        for (a, b) in streaming.iter().zip(&batch) {
            for (x, y) in a.iter().zip(b) {
                assert!((x - y).norm() < 1e-12);
            }
        }
    }

    #[test]
    fn streaming_synthesis_matches_one_shot() {
        let signal = test_signal(2_500);
        let spectra = analyze(&signal, StftConfig::default());
        let one_shot = synthesize(&spectra, signal.len());

        let mut synthesis = Synthesis::with_total(StftConfig::default(), signal.len());
        let mut streamed = Vec::new();
        for spectrum in &spectra {
            synthesis.push_spectrum(spectrum);
            streamed.extend(synthesis.drain());
        }
        streamed.extend(synthesis.finish(signal.len()));
        assert_eq!(streamed.len(), one_shot.len());
        assert!(relative_rms(&one_shot, &streamed) < 1e-9);
    }

    #[test]
    fn band_matrix_identity_roundtrips() {
        let signal = test_signal(1_920);
        let spectra = analyze(&signal, StftConfig::default());
        let identity = BandMatrices::from_fn(7, 7, |_, row, col| {
            if row == col {
                Complex::new(1.0, 0.0)
            } else {
                Complex::new(0.0, 0.0)
            }
        });
        let transformed: Vec<Spectrum> = spectra
            .iter()
            .map(|spectrum| {
                let mut input = vec![Complex::new(0.0, 0.0); 7 * FFT_BINS];
                for channel in 0..7 {
                    input[channel * FFT_BINS..(channel + 1) * FFT_BINS].copy_from_slice(spectrum);
                }
                let output = apply_band_matrices(&identity, &input);
                output[..FFT_BINS].to_vec()
            })
            .collect();
        let reconstructed = synthesize(&transformed, signal.len());
        assert!(relative_rms(&signal, &reconstructed) < 1e-6);
    }

    #[test]
    fn full_spectrum_application_preserves_conjugate_symmetry() {
        let size = 8;
        let matrices = BandMatrices::from_fn(1, 1, |_, _, _| Complex::new(0.3, 0.4));
        let mut input = vec![Complex::new(0.0, 0.0); size];
        for (bin, value) in input.iter_mut().take(size / 2 + 1).enumerate() {
            *value = Complex::new(bin as f64 + 1.0, 0.5 * bin as f64);
        }
        for bin in 1..size / 2 {
            input[size - bin] = input[bin].conj();
        }
        let output = apply_band_matrices_full(&matrices, &input, size);
        for bin in 1..size / 2 {
            assert!((output[bin] - output[size - bin].conj()).norm() < 1e-12);
        }
    }
}
