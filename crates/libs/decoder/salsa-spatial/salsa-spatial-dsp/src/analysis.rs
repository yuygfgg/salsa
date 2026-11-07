//! Encoder-side aligned source/core statistics.

use num_complex::Complex;
use rayon::prelude::*;
use salsa_spatial::{
    CORE_CHANNEL_COUNT, FFT_BINS, HOP_SAMPLES, PARAM_BAND_COUNT, TILE_HOPS, bin_weight,
    parameter_band_range,
};

use crate::reconstruct::SILENCE_EPSILON;

pub const ENERGY_POOL_HOPS: usize = 4;
pub const CORRELATION_POOL_HOPS: usize = TILE_HOPS;
pub const CORE_ERROR_EPSILON: f64 = SILENCE_EPSILON;

/// Measured core-error powers and the normalized envelope for one band.
#[derive(Debug, Clone, PartialEq)]
pub struct CoreErrorMeasurement {
    pub error_power: [f64; CORE_CHANNEL_COUNT],
    pub core_power: [f64; CORE_CHANNEL_COUNT],
    pub nu: [f64; CORE_CHANNEL_COUNT],
    pub unobservable: [bool; CORE_CHANNEL_COUNT],
}

/// Weighted statistics for one parameter band and one hop.
#[derive(Debug, Clone, PartialEq)]
pub struct BandStatistics {
    pub band: usize,
    pub source_power: Vec<f64>,
    /// Cross covariance E[s y^H] with the locally decoded core.
    pub source_decoded: Vec<[Complex<f64>; CORE_CHANNEL_COUNT]>,
    pub core_power: [f64; CORE_CHANNEL_COUNT],
    pub decoded_core: [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    pub error_power: [f64; CORE_CHANNEL_COUNT],
    pub nu: [f64; CORE_CHANNEL_COUNT],
    pub unobservable: [bool; CORE_CHANNEL_COUNT],
}

impl BandStatistics {
    fn empty(band: usize, object_count: usize) -> Self {
        Self {
            band,
            source_power: vec![0.0; object_count],
            source_decoded: vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; object_count],
            core_power: [0.0; CORE_CHANNEL_COUNT],
            decoded_core: [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
            error_power: [0.0; CORE_CHANNEL_COUNT],
            nu: [0.0; CORE_CHANNEL_COUNT],
            unobservable: [false; CORE_CHANNEL_COUNT],
        }
    }

    fn add_assign(&mut self, other: &BandStatistics) {
        for (left, right) in self.source_power.iter_mut().zip(&other.source_power) {
            *left += right;
        }
        for (left, right) in self.source_decoded.iter_mut().zip(&other.source_decoded) {
            for (a, b) in left.iter_mut().zip(right) {
                *a += b;
            }
        }
        for channel in 0..CORE_CHANNEL_COUNT {
            self.core_power[channel] += other.core_power[channel];
            self.error_power[channel] += other.error_power[channel];
            for other_channel in 0..CORE_CHANNEL_COUNT {
                self.decoded_core[channel][other_channel] +=
                    other.decoded_core[channel][other_channel];
            }
        }
    }

    fn normalize_error(&mut self) {
        let (nu, unobservable) =
            normalize_nu(&self.error_power, &self.core_power, CORE_ERROR_EPSILON);
        self.nu = nu;
        self.unobservable = unobservable;
    }
}

/// Per-hop statistics on the shared STFT grid.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameStatistics {
    pub sample: u64,
    pub hop: usize,
    pub bands: Vec<BandStatistics>,
}

/// Accumulates aligned source, ideal-core and decoded-core spectra.
#[derive(Debug, Clone)]
pub struct StatisticsAccumulator {
    object_count: usize,
    hops: Vec<FrameStatistics>,
    next_sample: u64,
    epsilon: f64,
}

impl StatisticsAccumulator {
    pub fn new(object_count: usize) -> Self {
        Self {
            object_count,
            hops: Vec::new(),
            next_sample: 0,
            epsilon: CORE_ERROR_EPSILON,
        }
    }

    pub fn with_epsilon(mut self, epsilon: f64) -> Self {
        self.epsilon = epsilon;
        self
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn hop_count(&self) -> usize {
        self.hops.len()
    }

    pub fn per_hop(&self) -> &[FrameStatistics] {
        &self.hops
    }

    pub fn hop(&self, index: usize) -> Option<&FrameStatistics> {
        self.hops.get(index)
    }

    pub fn clear(&mut self) {
        self.hops.clear();
        self.next_sample = 0;
    }

    /// Add one aligned hop at an explicit start sample.
    pub fn push_hop(
        &mut self,
        sample: u64,
        source: &[Complex<f64>],
        core: &[Complex<f64>],
        decoded: &[Complex<f64>],
    ) -> &FrameStatistics {
        let expected_source = self.object_count * FFT_BINS;
        assert_eq!(source.len(), expected_source, "source spectrum shape");
        assert_eq!(core.len(), CORE_CHANNEL_COUNT * FFT_BINS, "core shape");
        assert_eq!(
            decoded.len(),
            CORE_CHANNEL_COUNT * FFT_BINS,
            "decoded core shape"
        );

        let objects: Vec<usize> = (0..self.object_count).collect();
        self.push_hop_objects(sample, source, core, decoded, &objects)
    }

    /// Add one aligned hop while only accumulating the listed source objects.
    ///
    /// Inactive objects have exactly zero source covariance, so skipping them
    /// avoids the full object-count multiply in the encoder's hot path.
    pub fn push_hop_objects(
        &mut self,
        sample: u64,
        source: &[Complex<f64>],
        core: &[Complex<f64>],
        decoded: &[Complex<f64>],
        objects: &[usize],
    ) -> &FrameStatistics {
        let expected_source = self.object_count * FFT_BINS;
        assert_eq!(source.len(), expected_source, "source spectrum shape");
        assert_eq!(core.len(), CORE_CHANNEL_COUNT * FFT_BINS, "core shape");
        assert_eq!(
            decoded.len(),
            CORE_CHANNEL_COUNT * FFT_BINS,
            "decoded core shape"
        );

        let bands: Vec<BandStatistics> = (0..PARAM_BAND_COUNT)
            .into_par_iter()
            .map(|band| self.accumulate_band_objects(band, source, core, decoded, objects))
            .collect();
        self.next_sample = sample.saturating_add(HOP_SAMPLES as u64);
        let hop = self.hops.len();
        self.hops.push(FrameStatistics { sample, hop, bands });
        self.hops.last().expect("just pushed")
    }

    /// Add one aligned hop using the accumulator's sample cursor.
    pub fn push_next_hop(
        &mut self,
        source: &[Complex<f64>],
        core: &[Complex<f64>],
        decoded: &[Complex<f64>],
    ) -> &FrameStatistics {
        let sample = self.next_sample;
        self.push_hop(sample, source, core, decoded)
    }

    fn accumulate_band_objects(
        &self,
        band: usize,
        source: &[Complex<f64>],
        core: &[Complex<f64>],
        decoded: &[Complex<f64>],
        objects: &[usize],
    ) -> BandStatistics {
        let mut statistics = BandStatistics::empty(band, self.object_count);
        for bin in parameter_band_range(band) {
            let weight = bin_weight(bin);
            for &object in objects {
                let source_value = source[object * FFT_BINS + bin];
                statistics.source_power[object] += weight * source_value.norm_sqr();
                for channel in 0..CORE_CHANNEL_COUNT {
                    statistics.source_decoded[object][channel] +=
                        weight * source_value * decoded[channel * FFT_BINS + bin].conj();
                }
            }
            for channel in 0..CORE_CHANNEL_COUNT {
                let core_value = core[channel * FFT_BINS + bin];
                let decoded_value = decoded[channel * FFT_BINS + bin];
                statistics.core_power[channel] += weight * core_value.norm_sqr();
                let error = decoded_value - core_value;
                statistics.error_power[channel] += weight * error.norm_sqr();
                for other in 0..CORE_CHANNEL_COUNT {
                    statistics.decoded_core[channel][other] +=
                        weight * decoded_value * decoded[other * FFT_BINS + bin].conj();
                }
            }
        }
        statistics.normalize_error();
        statistics
    }

    /// Sum `count` consecutive hop statistics starting at `start`.
    pub fn pool(&self, start: usize, count: usize) -> Option<FrameStatistics> {
        if count == 0 || start + count > self.hops.len() {
            return None;
        }
        let mut bands: Vec<BandStatistics> = (0..PARAM_BAND_COUNT)
            .map(|band| BandStatistics::empty(band, self.object_count))
            .collect();
        for hop in &self.hops[start..start + count] {
            for (destination, source) in bands.iter_mut().zip(&hop.bands) {
                destination.add_assign(source);
            }
        }
        for band in &mut bands {
            band.normalize_error();
        }
        Some(FrameStatistics {
            sample: self.hops[start].sample,
            hop: start,
            bands,
        })
    }

    /// Four-hop energy pool starting at `start`.
    pub fn pooled_energy(&self, start: usize) -> Option<FrameStatistics> {
        self.pool(start, ENERGY_POOL_HOPS)
    }

    /// Twenty-hop (100 ms) correlation pool starting at `start`.
    pub fn pooled_correlation(&self, start: usize) -> Option<FrameStatistics> {
        self.pool(start, CORRELATION_POOL_HOPS)
    }

    pub fn latest_pooled_energy(&self) -> Option<FrameStatistics> {
        self.hops
            .len()
            .checked_sub(ENERGY_POOL_HOPS)
            .and_then(|start| self.pooled_energy(start))
    }

    pub fn latest_pooled_correlation(&self) -> Option<FrameStatistics> {
        self.hops
            .len()
            .checked_sub(CORRELATION_POOL_HOPS)
            .and_then(|start| self.pooled_correlation(start))
    }
}

fn normalize_nu(
    error_power: &[f64; CORE_CHANNEL_COUNT],
    core_power: &[f64; CORE_CHANNEL_COUNT],
    epsilon: f64,
) -> ([f64; CORE_CHANNEL_COUNT], [bool; CORE_CHANNEL_COUNT]) {
    let total: f64 = core_power.iter().sum();
    if !total.is_finite() || total <= epsilon {
        return ([0.0; CORE_CHANNEL_COUNT], [true; CORE_CHANNEL_COUNT]);
    }
    let mut nu = [0.0; CORE_CHANNEL_COUNT];
    for channel in 0..CORE_CHANNEL_COUNT {
        nu[channel] = (error_power[channel] / total).max(0.0);
    }
    (nu, [false; CORE_CHANNEL_COUNT])
}

/// Measure `e = y - x` for one band from full 7-channel spectra.
pub fn measured_core_error(
    core: &[Complex<f64>],
    decoded: &[Complex<f64>],
    band: usize,
    epsilon: f64,
) -> CoreErrorMeasurement {
    assert_eq!(core.len(), CORE_CHANNEL_COUNT * FFT_BINS);
    assert_eq!(decoded.len(), CORE_CHANNEL_COUNT * FFT_BINS);
    let mut error_power = [0.0; CORE_CHANNEL_COUNT];
    let mut core_power = [0.0; CORE_CHANNEL_COUNT];
    for bin in parameter_band_range(band) {
        let weight = bin_weight(bin);
        for channel in 0..CORE_CHANNEL_COUNT {
            let core_value = core[channel * FFT_BINS + bin];
            let decoded_value = decoded[channel * FFT_BINS + bin];
            core_power[channel] += weight * core_value.norm_sqr();
            let error = decoded_value - core_value;
            error_power[channel] += weight * error.norm_sqr();
        }
    }
    let (nu, unobservable) = normalize_nu(&error_power, &core_power, epsilon);
    CoreErrorMeasurement {
        error_power,
        core_power,
        nu,
        unobservable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_spectrum(channels: usize, seed: f64) -> Vec<Complex<f64>> {
        (0..channels * FFT_BINS)
            .map(|index| {
                let phase = seed + index as f64 * 0.013;
                Complex::new(phase.sin(), 0.3 * phase.cos())
            })
            .collect()
    }

    #[test]
    fn per_hop_statistics_use_bin_weights() {
        let mut source = vec![Complex::new(0.0, 0.0); 2 * FFT_BINS];
        let core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        source[0] = Complex::new(1.0, 0.0);
        source[FFT_BINS + 1] = Complex::new(1.0, 0.0);
        let mut accumulator = StatisticsAccumulator::new(2);
        let frame = accumulator.push_hop(0, &source, &core, &core).clone();
        assert!((frame.bands[0].source_power[0] - 1.0).abs() < 1e-15);
        assert!((frame.bands[0].source_power[1] - 2.0).abs() < 1e-15);
        assert!(
            (frame.bands[0].source_power[1] - 2.0 * frame.bands[0].source_power[0]).abs() < 1e-15
        );
    }

    #[test]
    fn silence_marks_the_band_unobservable() {
        let core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        let decoded = vec![Complex::new(1.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        let measurement = measured_core_error(&core, &decoded, 0, CORE_ERROR_EPSILON);
        assert!(measurement.unobservable.iter().all(|flag| *flag));
        assert!(measurement.nu.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn normalized_error_uses_total_core_power() {
        let mut core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        let mut decoded = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        core[1] = Complex::new(1.0, 0.0);
        decoded[1] = Complex::new(2.0, 0.0);
        let measurement = measured_core_error(&core, &decoded, 0, CORE_ERROR_EPSILON);
        assert!((measurement.nu[0] - 1.0).abs() < 1e-15);
        for channel in 1..CORE_CHANNEL_COUNT {
            assert!(measurement.nu[channel].abs() < 1e-15);
        }
    }

    #[test]
    fn pooling_sums_hops_and_repools_error() {
        let source = deterministic_spectrum(2, 0.2);
        let core = deterministic_spectrum(CORE_CHANNEL_COUNT, 1.1);
        let mut decoded = core.clone();
        for value in &mut decoded {
            *value *= 0.9;
        }
        let mut accumulator = StatisticsAccumulator::new(2);
        for _ in 0..TILE_HOPS {
            accumulator.push_next_hop(&source, &core, &decoded);
        }
        assert_eq!(accumulator.hop_count(), TILE_HOPS);
        let one = &accumulator.per_hop()[0];
        let energy = accumulator.latest_pooled_energy().unwrap();
        let correlation = accumulator.latest_pooled_correlation().unwrap();
        for band in 0..PARAM_BAND_COUNT {
            assert!(
                (energy.bands[band].source_power[0]
                    - ENERGY_POOL_HOPS as f64 * one.bands[band].source_power[0])
                    .abs()
                    < 1e-10
            );
            assert!(
                (correlation.bands[band].source_power[1]
                    - CORRELATION_POOL_HOPS as f64 * one.bands[band].source_power[1])
                    .abs()
                    < 1e-10
            );
        }
        assert!(accumulator.pooled_correlation(1).is_none());
    }

    #[test]
    fn source_decoded_cross_uses_the_lossy_observation() {
        let source = deterministic_spectrum(1, 0.7);
        let core = deterministic_spectrum(CORE_CHANNEL_COUNT, 1.3);
        let mut accumulator = StatisticsAccumulator::new(1);
        let decoded = deterministic_spectrum(CORE_CHANNEL_COUNT, 0.43);
        let frame = accumulator.push_hop(0, &source, &core, &decoded).clone();
        for band in 0..PARAM_BAND_COUNT {
            let statistics = &frame.bands[band];
            for channel in 0..CORE_CHANNEL_COUNT {
                let mut expected = Complex::new(0.0, 0.0);
                for bin in parameter_band_range(band) {
                    expected +=
                        bin_weight(bin) * source[bin] * decoded[channel * FFT_BINS + bin].conj();
                }
                assert!((statistics.source_decoded[0][channel] - expected).norm() < 1e-12);
            }
        }
    }
}
