//! Thin DSP-side wrapper around the orthonormal 24-point DCT-II basis.

pub use salsa_spatial::DctBasis;
use salsa_spatial::{DCT_COEFF_COUNT, PARAM_BAND_COUNT};

/// DCT-II analysis with the shared basis.
pub fn analysis(values: &[f64; PARAM_BAND_COUNT]) -> [f64; DCT_COEFF_COUNT] {
    DctBasis::new().analysis(values)
}

/// DCT-II synthesis with the shared basis.
pub fn synthesis(coefficients: &[f64; DCT_COEFF_COUNT]) -> [f64; PARAM_BAND_COUNT] {
    DctBasis::new().synthesis(coefficients)
}

/// Reusable wrapper that owns one basis instance.
#[derive(Debug, Clone, Default)]
pub struct Dct {
    basis: DctBasis,
}

impl Dct {
    pub fn new() -> Self {
        Self {
            basis: DctBasis::new(),
        }
    }

    pub fn analysis(&self, values: &[f64; PARAM_BAND_COUNT]) -> [f64; DCT_COEFF_COUNT] {
        self.basis.analysis(values)
    }

    pub fn synthesis(&self, coefficients: &[f64; DCT_COEFF_COUNT]) -> [f64; PARAM_BAND_COUNT] {
        self.basis.synthesis(coefficients)
    }

    pub fn basis(&self) -> &DctBasis {
        &self.basis
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basis_rows_are_orthonormal() {
        let basis = DctBasis::new();
        for a in 0..DCT_COEFF_COUNT {
            for b in 0..DCT_COEFF_COUNT {
                let dot: f64 = (0..PARAM_BAND_COUNT)
                    .map(|band| basis.basis[a][band] * basis.basis[b][band])
                    .sum();
                let expected = if a == b { 1.0 } else { 0.0 };
                assert!((dot - expected).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn wrapper_matches_shared_basis() {
        let values: [f64; PARAM_BAND_COUNT] =
            std::array::from_fn(|band| (band as f64 * 0.37).sin());
        let coefficients = analysis(&values);
        let expected = DctBasis::new().analysis(&values);
        assert_eq!(coefficients, expected);
        let restored = synthesis(&coefficients);
        let expected_restored = DctBasis::new().synthesis(&coefficients);
        assert_eq!(restored, expected_restored);
    }
}
