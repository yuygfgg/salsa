//! Regularized object reconstruction from the core covariance model.

use nalgebra::DMatrix;
use num_complex::Complex;
use salsa_spatial::{CORE_CHANNEL_COUNT, FFT_BINS, PARAM_BAND_COUNT, parameter_band_range};

use crate::covariance::BandCovariance;

pub const SILENCE_EPSILON: f64 = 1e-24;

/// Error from solving one band's regularized system.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SolveError {
    #[error("Cholesky factorization failed in band {band}: {message}")]
    Factorization { band: usize, message: String },
    #[error("invalid reconstruction input: {0}")]
    InvalidInput(String),
}

/// Error from estimating a hop of object spectra.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ReconstructionError {
    #[error("factorization failed at hop {hop}, band {band}: {message}")]
    Factorization {
        hop: usize,
        band: usize,
        message: String,
    },
    #[error("invalid reconstruction input: {0}")]
    InvalidInput(String),
}

/// Estimator `M = W^H` for one band. Each row maps the seven core observations
/// to one object.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconstruction {
    pub matrix: Vec<[Complex<f64>; CORE_CHANNEL_COUNT]>,
}

impl Reconstruction {
    pub fn object_count(&self) -> usize {
        self.matrix.len()
    }

    pub fn apply(&self, core: &[Complex<f64>; CORE_CHANNEL_COUNT]) -> Vec<Complex<f64>> {
        self.matrix
            .iter()
            .map(|row| {
                let mut sum = Complex::new(0.0, 0.0);
                for channel in 0..CORE_CHANNEL_COUNT {
                    sum += row[channel] * core[channel];
                }
                sum
            })
            .collect()
    }
}

/// Add the diagonal regularization to the observation covariance.
fn build_system(
    z: [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
    lambda: f64,
) -> Result<[[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT], SolveError> {
    let trace = (0..CORE_CHANNEL_COUNT).map(|ch| z[ch][ch].re).sum::<f64>();
    if !trace.is_finite() || trace <= 0.0 {
        return Err(SolveError::Factorization {
            band,
            message: format!("non-positive covariance trace {trace}"),
        });
    }
    let lambda_term = lambda.max(0.0) / CORE_CHANNEL_COUNT as f64;
    let mut system = [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
    for row in 0..CORE_CHANNEL_COUNT {
        for col in 0..CORE_CHANNEL_COUNT {
            system[row][col] = if row == col {
                Complex::new(
                    z[row][col].re + trace * (nu[row].max(0.0) + lambda_term),
                    0.0,
                )
            } else if row < col {
                z[row][col]
            } else {
                z[col][row].conj()
            };
        }
    }
    Ok(system)
}

/// In-place lower-triangular Cholesky factor of a Hermitian positive-definite
/// seven-channel system.
fn cholesky_factor(
    system: [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    band: usize,
) -> Result<[[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT], SolveError> {
    let mut lower = [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
    for i in 0..CORE_CHANNEL_COUNT {
        for j in 0..=i {
            let mut sum = system[i][j];
            for k in 0..j {
                sum -= lower[i][k] * lower[j][k].conj();
            }
            if i == j {
                let diagonal = sum.re;
                if !diagonal.is_finite() || diagonal <= 0.0 {
                    return Err(SolveError::Factorization {
                        band,
                        message: "matrix is not positive definite".to_string(),
                    });
                }
                lower[i][j] = Complex::new(diagonal.sqrt(), 0.0);
            } else {
                lower[i][j] = sum / lower[j][j].re;
            }
        }
    }
    Ok(lower)
}

/// Solve `L L^H x = b` for a fixed-size lower Cholesky factor.
fn cholesky_solve(
    lower: &[[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    rhs: &[Complex<f64>; CORE_CHANNEL_COUNT],
) -> [Complex<f64>; CORE_CHANNEL_COUNT] {
    let mut forward = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
    for i in 0..CORE_CHANNEL_COUNT {
        let mut sum = rhs[i];
        for k in 0..i {
            sum -= lower[i][k] * forward[k];
        }
        forward[i] = sum / lower[i][i].re;
    }
    let mut result = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
    for i in (0..CORE_CHANNEL_COUNT).rev() {
        let mut sum = forward[i];
        for k in (i + 1)..CORE_CHANNEL_COUNT {
            sum -= lower[k][i].conj() * result[k];
        }
        result[i] = sum / lower[i][i].re;
    }
    result
}

fn reconstruct_from_cross(
    lower: &[[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    cross: &[[Complex<f64>; CORE_CHANNEL_COUNT]],
) -> Reconstruction {
    let zero_row = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
    let mut matrix = Vec::with_capacity(cross.len());
    for row in cross {
        if row.iter().all(|value| value.norm_sqr() == 0.0) {
            matrix.push(zero_row);
            continue;
        }
        let rhs = std::array::from_fn(|channel| row[channel].conj());
        let w = cholesky_solve(lower, &rhs);
        matrix.push(std::array::from_fn(|channel| w[channel].conj()));
    }
    Reconstruction { matrix }
}

fn solve_inner(
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
    lambda: f64,
    with_inverse: bool,
) -> Result<
    (
        Reconstruction,
        Option<[[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT]>,
    ),
    SolveError,
> {
    if band >= PARAM_BAND_COUNT {
        return Err(SolveError::InvalidInput(format!(
            "band {band} out of range"
        )));
    }
    if d.len() != covariance.object_count() {
        return Err(SolveError::InvalidInput(format!(
            "downmix has {} columns but covariance has {} objects",
            d.len(),
            covariance.object_count()
        )));
    }
    let (z, cross) = covariance.observation(band, d);
    let system = build_system(z, band, nu, lambda)?;
    let lower = cholesky_factor(system, band)?;
    let reconstruction = reconstruct_from_cross(&lower, &cross);
    let inverse = if with_inverse {
        let mut inverse = [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
        for column in 0..CORE_CHANNEL_COUNT {
            let mut unit = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
            unit[column] = Complex::new(1.0, 0.0);
            let solution = cholesky_solve(&lower, &unit);
            for row in 0..CORE_CHANNEL_COUNT {
                inverse[row][column] = solution[row];
            }
        }
        Some(inverse)
    } else {
        None
    };
    Ok((reconstruction, inverse))
}

/// Solve `(Z + Gamma) W = D C` and return `M = W^H`.
///
/// `Z = D C D^H` and `Gamma = trace(Z) * (diag(nu) + (lambda/7) I)`. The
/// seven-dimensional Hermitian system is factored with a fixed-size f64
/// complex Cholesky decomposition.
pub fn solve_regularized(
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
    lambda: f64,
) -> Result<Reconstruction, SolveError> {
    solve_inner(d, covariance, band, nu, lambda, false).map(|(reconstruction, _)| reconstruction)
}

/// Solve like [`solve_regularized`] and also return the inverse of the
/// regularized system. Encoder derivatives need both; factoring once avoids a
/// second Cholesky decomposition per band.
pub fn solve_regularized_with_inverse(
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
    lambda: f64,
) -> Result<
    (
        Reconstruction,
        [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    ),
    SolveError,
> {
    let (reconstruction, inverse) = solve_inner(d, covariance, band, nu, lambda, true)?;
    Ok((
        reconstruction,
        inverse.expect("inverse was requested from the solver"),
    ))
}

/// Build the same regularized observation covariance used by reconstruction.
/// Encoder derivatives use this system to differentiate the seven-channel solve.
pub fn regularized_system(
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
    lambda: f64,
) -> Result<DMatrix<Complex<f64>>, SolveError> {
    if band >= PARAM_BAND_COUNT {
        return Err(SolveError::InvalidInput(format!(
            "band {band} out of range"
        )));
    }
    if d.len() != covariance.object_count() {
        return Err(SolveError::InvalidInput(format!(
            "downmix has {} columns but covariance has {} objects",
            d.len(),
            covariance.object_count()
        )));
    }

    let z = covariance.core_covariance(band, d);
    let trace = z
        .iter()
        .enumerate()
        .map(|(channel, row)| row[channel].re)
        .sum::<f64>();
    if !trace.is_finite() || trace <= 0.0 {
        return Err(SolveError::Factorization {
            band,
            message: format!("non-positive covariance trace {trace}"),
        });
    }

    let lambda_term = lambda.max(0.0) / CORE_CHANNEL_COUNT as f64;
    let mut system = DMatrix::<Complex<f64>>::zeros(CORE_CHANNEL_COUNT, CORE_CHANNEL_COUNT);
    for row in 0..CORE_CHANNEL_COUNT {
        for col in 0..CORE_CHANNEL_COUNT {
            let value = if row == col {
                Complex::new(
                    z[row][col].re + trace * (nu[row].max(0.0) + lambda_term),
                    0.0,
                )
            } else if row < col {
                z[row][col]
            } else {
                z[col][row].conj()
            };
            system[(row, col)] = value;
        }
    }

    Ok(system)
}

fn band_energy(core: &[Complex<f64>], band: usize) -> f64 {
    let mut energy = 0.0;
    for bin in parameter_band_range(band) {
        for channel in 0..CORE_CHANNEL_COUNT {
            energy += core[channel * FFT_BINS + bin].norm_sqr();
        }
    }
    energy
}

/// Apply the reconstruction estimator to one hop of decoded 7-channel core
/// spectra.
///
/// `core` is channel-major (`channel * FFT_BINS + bin`). The result is one
/// spectrum per object. Inactive scenes and unobservable bands emit zeros.
/// A non-silent band whose Hermitian system cannot be factored returns the hop
/// and band in [`ReconstructionError::Factorization`].
#[allow(clippy::too_many_arguments)]
pub fn estimate_objects(
    hop: usize,
    core: &[Complex<f64>],
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    nu: &[[f64; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT],
    lambda: f64,
    active: bool,
    unobservable: &[bool; PARAM_BAND_COUNT],
) -> Result<Vec<Vec<Complex<f64>>>, ReconstructionError> {
    if core.len() != CORE_CHANNEL_COUNT * FFT_BINS {
        return Err(ReconstructionError::InvalidInput(format!(
            "core spectrum has {} values, expected {}",
            core.len(),
            CORE_CHANNEL_COUNT * FFT_BINS
        )));
    }
    if d.len() != covariance.object_count() {
        return Err(ReconstructionError::InvalidInput(format!(
            "downmix has {} columns but covariance has {} objects",
            d.len(),
            covariance.object_count()
        )));
    }

    let object_count = covariance.object_count();
    let mut output = vec![vec![Complex::new(0.0, 0.0); FFT_BINS]; object_count];
    if !active {
        return Ok(output);
    }
    let real_covariance = covariance.real_part();

    for band in 0..PARAM_BAND_COUNT {
        if unobservable[band] {
            continue;
        }
        let energy = band_energy(core, band);
        if energy <= SILENCE_EPSILON {
            continue;
        }
        let reconstruction = match solve_regularized(d, covariance, band, &nu[band], lambda) {
            Ok(reconstruction) => reconstruction,
            Err(error) => {
                return Err(ReconstructionError::Factorization {
                    hop,
                    band,
                    message: error.to_string(),
                });
            }
        };
        for bin in parameter_band_range(band) {
            let mut observation = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
            for channel in 0..CORE_CHANNEL_COUNT {
                observation[channel] = core[channel * FFT_BINS + bin];
            }
            let endpoint = bin == 0 || bin == FFT_BINS - 1;
            let estimates = if endpoint {
                let real = solve_regularized(d, &real_covariance, band, &nu[band], lambda)
                    .map_err(|error| ReconstructionError::Factorization {
                        hop,
                        band,
                        message: error.to_string(),
                    })?;
                real.apply(&observation)
            } else {
                reconstruction.apply(&observation)
            };
            for (object, mut value) in estimates.into_iter().enumerate() {
                if endpoint {
                    value = Complex::new(value.re, 0.0);
                }
                output[object][bin] = value;
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::covariance::covariance_from_band_powers;
    use salsa_spatial::{CorrelationPair, FFT_SIZE};

    #[test]
    fn endpoints_solve_with_real_covariance_before_filtering() {
        use crate::covariance::CorrelatedPair;
        let covariance = BandCovariance::new(
            2,
            vec![vec![0.5, 0.5]; PARAM_BAND_COUNT],
            vec![CorrelatedPair {
                a: 0,
                b: 1,
                coefficient: [Complex::new(0.0, 0.45); PARAM_BAND_COUNT],
            }],
        );
        let d = identity_columns(2);
        let nu = [[0.02; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let mut core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        core[0] = Complex::new(1.0, 0.0);
        core[FFT_BINS - 1] = Complex::new(1.0, 0.0);
        let output = estimate_objects(
            0,
            &core,
            &d,
            &covariance,
            &nu,
            0.001,
            true,
            &[false; PARAM_BAND_COUNT],
        )
        .unwrap();
        for (bin, band) in [(0, 0), (FFT_BINS - 1, PARAM_BAND_COUNT - 1)] {
            let expected =
                solve_regularized(&d, &covariance.real_part(), band, &nu[band], 0.001).unwrap();
            assert!((output[0][bin].re - expected.matrix[0][0].re).abs() < 1e-12);
            assert_eq!(output[0][bin].im, 0.0);
        }
    }

    fn scaled_powers(object_count: usize, scale: f64) -> Vec<[f64; PARAM_BAND_COUNT]> {
        vec![[scale / object_count as f64; PARAM_BAND_COUNT]; object_count]
    }

    fn identity_columns(object_count: usize) -> Vec<[f64; CORE_CHANNEL_COUNT]> {
        (0..object_count)
            .map(|object| {
                let mut column = [0.0; CORE_CHANNEL_COUNT];
                column[object] = 1.0;
                column
            })
            .collect()
    }

    #[test]
    fn full_rank_diagonal_covariance_recovers_sources_exactly() {
        let object_count = 7;
        let powers = scaled_powers(object_count, 1.0);
        let covariance = covariance_from_band_powers(&powers, &[] as &[CorrelationPair]);
        let d = identity_columns(object_count);
        let sources: Vec<f64> = (0..object_count).map(|i| 0.1 + i as f64).collect();
        let mut observation = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
        for (channel, value) in sources.iter().enumerate() {
            observation[channel] = Complex::new(*value, 0.0);
        }
        let nu = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let result = solve_regularized(&d, &covariance, 0, &nu[0], 0.0).unwrap();
        let estimated = result.apply(&observation);
        for (actual, expected) in estimated.iter().zip(&sources) {
            assert!((actual.re - expected).abs() < 1e-10);
            assert!(actual.im.abs() < 1e-10);
        }
    }

    #[test]
    fn common_positive_scale_leaves_reconstruction_unchanged() {
        let object_count = 3;
        let d = identity_columns(object_count);
        let nu = [[0.01; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let a = covariance_from_band_powers(&scaled_powers(object_count, 1.0), &[]);
        let b = covariance_from_band_powers(&scaled_powers(object_count, 1234.0), &[]);
        let ma = solve_regularized(&d, &a, 5, &nu[5], 1e-3).unwrap();
        let mb = solve_regularized(&d, &b, 5, &nu[5], 1e-3).unwrap();
        for (row_a, row_b) in ma.matrix.iter().zip(&mb.matrix) {
            for (x, y) in row_a.iter().zip(row_b) {
                assert!((x - y).norm() < 1e-10, "{x} != {y}");
            }
        }
    }

    #[test]
    fn equal_downmix_two_object_case_matches_analytic() {
        let object_count = 2;
        let powers = vec![[0.5; PARAM_BAND_COUNT]; object_count];
        let covariance = covariance_from_band_powers(&powers, &[]);
        let column = [
            1.0 / 3.0f64.sqrt(),
            1.0 / 3.0f64.sqrt(),
            1.0 / 3.0f64.sqrt(),
            0.0,
            0.0,
            0.0,
            0.0,
        ];
        let d = vec![column, column];
        let lambda = 1e-3;
        let nu = [0.0; CORE_CHANNEL_COUNT];
        let result = solve_regularized(&d, &covariance, 0, &nu, lambda).unwrap();
        assert!((result.matrix[0][0] - result.matrix[1][0]).norm() < 1e-12);

        let s0 = 0.7;
        let s1 = -0.2;
        let mut observation = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
        for channel in 0..CORE_CHANNEL_COUNT {
            observation[channel] = Complex::new(column[channel] * (s0 + s1), 0.0);
        }
        let estimated = result.apply(&observation);
        let expected = 0.5 * (s0 + s1) / (1.0 + lambda / CORE_CHANNEL_COUNT as f64);
        for value in estimated {
            assert!((value.re - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn estimate_zeros_inactive_and_unobservable_bands() {
        let powers = scaled_powers(2, 1.0);
        let covariance = covariance_from_band_powers(&powers, &[]);
        let d = identity_columns(2);
        let core = vec![Complex::new(1.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        let nu = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let unobservable = [false; PARAM_BAND_COUNT];

        let inactive =
            estimate_objects(0, &core, &d, &covariance, &nu, 1e-3, false, &unobservable).unwrap();
        assert!(inactive.iter().flatten().all(|value| value.norm() < 1e-15));

        let mut mask = [false; PARAM_BAND_COUNT];
        mask[0] = true;
        let masked = estimate_objects(0, &core, &d, &covariance, &nu, 1e-3, true, &mask).unwrap();
        for bin in parameter_band_range(0) {
            for object in 0..2 {
                assert!(masked[object][bin].norm() < 1e-15);
            }
        }
    }

    #[test]
    fn estimate_recovers_objects_on_full_rank_core() {
        let object_count = 7;
        let powers = scaled_powers(object_count, 1.0);
        let covariance = covariance_from_band_powers(&powers, &[]);
        let d = identity_columns(object_count);
        let mut core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        for channel in 0..CORE_CHANNEL_COUNT {
            core[channel * FFT_BINS + 3] = Complex::new(channel as f64 + 1.0, 0.0);
        }
        let nu = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let unobservable = [false; PARAM_BAND_COUNT];
        let output =
            estimate_objects(0, &core, &d, &covariance, &nu, 0.0, true, &unobservable).unwrap();
        for channel in 0..CORE_CHANNEL_COUNT {
            assert!((output[channel][3].re - (channel as f64 + 1.0)).abs() < 1e-8);
        }
    }

    #[test]
    fn non_silent_factorization_failure_reports_hop_and_band() {
        let powers = scaled_powers(1, 0.0);
        let covariance = covariance_from_band_powers(&powers, &[]);
        let d = identity_columns(1);
        let mut core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        core[1] = Complex::new(1.0, 0.0);
        let nu = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let unobservable = [false; PARAM_BAND_COUNT];
        let error = estimate_objects(7, &core, &d, &covariance, &nu, 1e-3, true, &unobservable)
            .unwrap_err();
        match error {
            ReconstructionError::Factorization { hop, band, .. } => {
                assert_eq!(hop, 7);
                assert_eq!(band, 0);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn reconstruction_works_for_default_fft_geometry() {
        assert_eq!(FFT_SIZE, 1920);
    }
}
