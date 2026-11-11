//! Per-band source covariance built from diagonal powers and correlation pairs.

use num_complex::Complex;
use salsa_spatial::{
    CORE_CHANNEL_COUNT, CORRELATION_MAX_MAGNITUDE, CORRELATION_PHASE_STEPS, CorrelationPair,
    CovarianceAnchor, PARAM_BAND_COUNT,
};

type CoreMatrix = [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
type CrossMatrix = Vec<[Complex<f64>; CORE_CHANNEL_COUNT]>;
type BandCoefficients = [Complex<f64>; PARAM_BAND_COUNT];

/// One correlated object pair, with the per-band off-diagonal covariance entry
/// `C[a][b]`. The opposite entry is its conjugate.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrelatedPair {
    pub a: usize,
    pub b: usize,
    pub coefficient: [Complex<f64>; PARAM_BAND_COUNT],
}

/// Compact per-band source covariance `C = diag(sqrt(q)) H diag(sqrt(q))`.
#[derive(Debug, Clone, PartialEq)]
pub struct BandCovariance {
    object_count: usize,
    diagonal: Vec<Vec<f64>>,
    pairs: Vec<CorrelatedPair>,
}

impl BandCovariance {
    /// Build from explicit per-band diagonal powers and pair off-diagonals.
    pub fn new(object_count: usize, diagonal: Vec<Vec<f64>>, pairs: Vec<CorrelatedPair>) -> Self {
        assert_eq!(diagonal.len(), PARAM_BAND_COUNT);
        assert!(diagonal.iter().all(|row| row.len() == object_count));
        Self {
            object_count,
            diagonal,
            pairs,
        }
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn pairs(&self) -> &[CorrelatedPair] {
        &self.pairs
    }

    pub fn diagonal(&self, band: usize, object: usize) -> f64 {
        self.diagonal[band][object]
    }

    pub fn trace(&self, band: usize) -> f64 {
        self.diagonal[band].iter().sum()
    }

    /// Covariance of real observations at DC and Nyquist.
    pub fn real_part(&self) -> Self {
        let mut real = self.clone();
        for pair in &mut real.pairs {
            for coefficient in &mut pair.coefficient {
                coefficient.im = 0.0;
            }
        }
        real
    }

    /// Dense `N x N` Hermitian covariance for one band.
    pub fn dense(&self, band: usize) -> Vec<Vec<Complex<f64>>> {
        let count = self.object_count;
        let mut matrix = vec![vec![Complex::new(0.0, 0.0); count]; count];
        for (object, &power) in self.diagonal[band].iter().enumerate() {
            matrix[object][object] = Complex::new(power, 0.0);
        }
        for pair in &self.pairs {
            let coefficient = pair.coefficient[band];
            matrix[pair.a][pair.b] += coefficient;
            matrix[pair.b][pair.a] += coefficient.conj();
        }
        matrix
    }

    /// `D C D^H`, a 7x7 complex Hermitian matrix. `d[i]` is the real downmix
    /// column of object `i`.
    pub fn core_covariance(
        &self,
        band: usize,
        d: &[[f64; CORE_CHANNEL_COUNT]],
    ) -> [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT] {
        assert_eq!(d.len(), self.object_count);
        let mut matrix = [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
        for (column, &power) in d.iter().zip(&self.diagonal[band]) {
            if power == 0.0 {
                continue;
            }
            for row in 0..CORE_CHANNEL_COUNT {
                for col in 0..CORE_CHANNEL_COUNT {
                    matrix[row][col].re += power * column[row] * column[col];
                }
            }
        }
        for pair in &self.pairs {
            let coefficient = pair.coefficient[band];
            if coefficient == Complex::new(0.0, 0.0) {
                continue;
            }
            let a = d[pair.a];
            let b = d[pair.b];
            for row in 0..CORE_CHANNEL_COUNT {
                for col in 0..CORE_CHANNEL_COUNT {
                    matrix[row][col] +=
                        coefficient * (a[row] * b[col]) + coefficient.conj() * (b[row] * a[col]);
                }
            }
        }
        matrix
    }

    /// `C D^H`, an `N x 7` complex matrix. `d[i]` is the real downmix column of
    /// object `i`.
    pub fn cross_covariance(
        &self,
        band: usize,
        d: &[[f64; CORE_CHANNEL_COUNT]],
    ) -> Vec<[Complex<f64>; CORE_CHANNEL_COUNT]> {
        self.observation(band, d).1
    }

    /// Compute `Z = D C D^H` and `C D^H` together, walking the object and pair
    /// lists once. Reconstruction needs both for every band.
    pub fn observation(
        &self,
        band: usize,
        d: &[[f64; CORE_CHANNEL_COUNT]],
    ) -> (CoreMatrix, CrossMatrix) {
        assert_eq!(d.len(), self.object_count);
        let mut core = [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
        let mut cross = vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; self.object_count];
        for (object, (column, &power)) in d.iter().zip(&self.diagonal[band]).enumerate() {
            if power == 0.0 {
                continue;
            }
            for row in 0..CORE_CHANNEL_COUNT {
                let scaled = power * column[row];
                for col in 0..CORE_CHANNEL_COUNT {
                    core[row][col].re += scaled * column[col];
                }
                cross[object][row].re += scaled;
            }
        }
        for pair in &self.pairs {
            let coefficient = pair.coefficient[band];
            if coefficient == Complex::new(0.0, 0.0) {
                continue;
            }
            let a = d[pair.a];
            let b = d[pair.b];
            for row in 0..CORE_CHANNEL_COUNT {
                for col in 0..CORE_CHANNEL_COUNT {
                    core[row][col] +=
                        coefficient * (a[row] * b[col]) + coefficient.conj() * (b[row] * a[col]);
                }
                cross[pair.a][row] += coefficient * b[row];
                cross[pair.b][row] += coefficient.conj() * a[row];
            }
        }
        (core, cross)
    }

    fn pair_coefficient(&self, a: usize, b: usize, band: usize) -> Complex<f64> {
        let mut sum = Complex::new(0.0, 0.0);
        for pair in &self.pairs {
            if pair.a == a && pair.b == b {
                sum += pair.coefficient[band];
            } else if pair.a == b && pair.b == a {
                sum += pair.coefficient[band].conj();
            }
        }
        sum
    }

    /// Convex interpolation of the compact covariance entries.
    ///
    /// Diagonal powers and off-diagonal entries are both convex combinations of
    /// the endpoint values, so the result remains positive semidefinite even
    /// when the pair lists differ. The pair list is the union of both endpoints,
    /// which may temporarily give one object two partners.
    pub fn interpolate(&self, other: &BandCovariance, t: f64) -> BandCovariance {
        let t = t.clamp(0.0, 1.0);
        let first = 1.0 - t;
        let count = self.object_count.max(other.object_count);
        let mut diagonal = vec![vec![0.0; count]; PARAM_BAND_COUNT];
        for (band, row) in diagonal.iter_mut().enumerate() {
            for (object, value) in row.iter_mut().enumerate() {
                let a = self.diagonal[band].get(object).copied().unwrap_or(0.0);
                let b = other.diagonal[band].get(object).copied().unwrap_or(0.0);
                *value = first * a + t * b;
            }
        }

        let mut keys: Vec<(usize, usize)> = self
            .pairs
            .iter()
            .map(|pair| (pair.a, pair.b))
            .chain(other.pairs.iter().map(|pair| (pair.a, pair.b)))
            .collect();
        keys.sort_unstable();
        keys.dedup();

        let pairs = keys
            .into_iter()
            .map(|(a, b)| {
                let mut coefficient = [Complex::new(0.0, 0.0); PARAM_BAND_COUNT];
                for (band, value) in coefficient.iter_mut().enumerate() {
                    *value = first * self.pair_coefficient(a, b, band)
                        + t * other.pair_coefficient(a, b, band);
                }
                CorrelatedPair { a, b, coefficient }
            })
            .collect();

        BandCovariance {
            object_count: count,
            diagonal,
            pairs,
        }
    }
}

/// Build a covariance from explicit per-band object powers.
///
/// `q_bands[object][band]` is the relative power used for the diagonal. Each
/// pair coefficient is `sqrt(q_a q_b) * rho`, with `rho` reconstructed from
/// the quantized magnitude and phase. Missing per-band coefficients are zero.
pub fn covariance_from_band_powers(
    q_bands: &[[f64; PARAM_BAND_COUNT]],
    pairs: &[CorrelationPair],
) -> BandCovariance {
    let object_count = q_bands.len();
    let mut diagonal = vec![vec![0.0; object_count]; PARAM_BAND_COUNT];
    for (object, powers) in q_bands.iter().enumerate() {
        for band in 0..PARAM_BAND_COUNT {
            diagonal[band][object] = powers[band].max(0.0);
        }
    }

    let mut merged: Vec<((usize, usize), BandCoefficients)> = Vec::new();
    for pair in pairs {
        if pair.a == pair.b {
            continue;
        }
        let mut a = pair.a as usize;
        let mut b = pair.b as usize;
        if a >= object_count || b >= object_count {
            continue;
        }
        let mut flipped = false;
        if a > b {
            std::mem::swap(&mut a, &mut b);
            flipped = true;
        }
        let mut coefficient = [Complex::new(0.0, 0.0); PARAM_BAND_COUNT];
        for band in 0..PARAM_BAND_COUNT {
            let Some(quantized) = pair.rho[band] else {
                continue;
            };
            let magnitude = f64::from(quantized.magnitude.min(CORRELATION_MAX_MAGNITUDE)) / 16.0;
            if magnitude == 0.0 {
                continue;
            }
            let phase = 2.0
                * std::f64::consts::PI
                * f64::from(quantized.phase % CORRELATION_PHASE_STEPS as u8)
                / f64::from(CORRELATION_PHASE_STEPS);
            let phase = if flipped { -phase } else { phase };
            let rho = Complex::from_polar(magnitude, phase);
            let scale = (diagonal[band][a] * diagonal[band][b]).sqrt();
            coefficient[band] = rho * scale;
        }
        if let Some((_, existing)) = merged.iter_mut().find(|((x, y), _)| *x == a && *y == b) {
            for (slot, value) in existing.iter_mut().zip(coefficient) {
                *slot += value;
            }
        } else {
            merged.push(((a, b), coefficient));
        }
    }

    let pairs = merged
        .into_iter()
        .map(|((a, b), coefficient)| CorrelatedPair { a, b, coefficient })
        .collect();

    BandCovariance {
        object_count,
        diagonal,
        pairs,
    }
}

/// Build the placeholder band-averaged covariance from a decoded anchor.
pub fn covariance_from_anchor(anchor: &CovarianceAnchor) -> BandCovariance {
    let mut powers = vec![[0.0; PARAM_BAND_COUNT]; anchor.q.len()];
    for (object, band_powers) in powers.iter_mut().enumerate() {
        band_powers.fill(anchor.q[object].max(0.0));
    }
    covariance_from_band_powers(&powers, &anchor.pairs)
}

/// Convex interpolation helper.
pub fn interpolate_covariances(a: &BandCovariance, b: &BandCovariance, t: f64) -> BandCovariance {
    a.interpolate(b, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nalgebra::{DMatrix, linalg::Cholesky};

    fn pair(a: u16, b: u16, magnitude: u8, phase: u8) -> CorrelationPair {
        let mut rho = [None; PARAM_BAND_COUNT];
        rho.fill(Some(salsa_spatial::params::CorrelationCoefficient {
            magnitude,
            phase,
        }));
        CorrelationPair { a, b, rho }
    }

    fn assert_psd(covariance: &BandCovariance, band: usize) {
        let dense = covariance.dense(band);
        let count = covariance.object_count;
        let mut matrix = DMatrix::<Complex<f64>>::zeros(count, count);
        for row in 0..count {
            for col in 0..count {
                matrix[(row, col)] = dense[row][col];
            }
            matrix[(row, row)].re += 1e-12;
        }
        assert!(
            Cholesky::new(matrix).is_some(),
            "band {band} is not positive definite"
        );
    }

    #[test]
    fn trace_is_one_for_normalized_band_powers() {
        let mut q = [[0.0; PARAM_BAND_COUNT]; 3];
        q[0].fill(0.5);
        q[1].fill(0.3);
        q[2].fill(0.2);
        let covariance = covariance_from_band_powers(&q, &[pair(0, 1, 15, 7), pair(2, 0, 9, 13)]);
        for band in 0..PARAM_BAND_COUNT {
            assert!((covariance.trace(band) - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn covariance_stays_psd_through_pair_changes() {
        let mut q = [[0.0; PARAM_BAND_COUNT]; 3];
        q[0].fill(0.5);
        q[1].fill(0.3);
        q[2].fill(0.2);
        let a = covariance_from_band_powers(&q, &[pair(0, 1, 15, 0)]);
        let b = covariance_from_band_powers(&q, &[pair(0, 2, 15, 11)]);
        for step in 0..=20 {
            let t = step as f64 / 20.0;
            let covariance = a.interpolate(&b, t);
            for band in 0..PARAM_BAND_COUNT {
                assert_psd(&covariance, band);
            }
        }
    }

    #[test]
    fn interpolation_is_convex_for_powers_and_coefficients() {
        let mut q = [[0.0; PARAM_BAND_COUNT]; 2];
        q[0].fill(0.5);
        q[1].fill(0.5);
        let a = covariance_from_band_powers(&q, &[pair(0, 1, 0, 0)]);
        let b = covariance_from_band_powers(&q, &[pair(0, 1, 15, 0)]);
        let middle = a.interpolate(&b, 0.5);
        for band in 0..PARAM_BAND_COUNT {
            assert!((middle.diagonal(band, 0) - 0.5).abs() < 1e-12);
            let expected =
                0.5 * a.pairs()[0].coefficient[band] + 0.5 * b.pairs()[0].coefficient[band];
            assert!((middle.pairs()[0].coefficient[band] - expected).norm() < 1e-12);
        }
    }

    #[test]
    fn union_keeps_two_partners_during_transition() {
        let mut q = [[0.0; PARAM_BAND_COUNT]; 3];
        q[0].fill(0.4);
        q[1].fill(0.3);
        q[2].fill(0.3);
        let a = covariance_from_band_powers(&q, &[pair(0, 1, 15, 0)]);
        let b = covariance_from_band_powers(&q, &[pair(0, 2, 15, 0)]);
        let middle = a.interpolate(&b, 0.5);
        assert_eq!(middle.pairs().len(), 2);
    }

    #[test]
    fn cross_and_core_covariance_are_consistent() {
        let q = [[0.4; PARAM_BAND_COUNT], [0.6; PARAM_BAND_COUNT]];
        let covariance = covariance_from_band_powers(&q, &[pair(0, 1, 8, 5)]);
        let d = [
            [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ];
        let band = 3;
        let z = covariance.core_covariance(band, &d);
        let cdh = covariance.cross_covariance(band, &d);
        for row in 0..CORE_CHANNEL_COUNT {
            for col in 0..CORE_CHANNEL_COUNT {
                let mut expected = Complex::new(0.0, 0.0);
                for object in 0..2 {
                    expected += cdh[object][row].conj() * d[object][col];
                }
                assert!((z[row][col] - expected).norm() < 1e-12);
            }
        }
    }
}
