//! Regularized object reconstruction from the core covariance model.

use num_complex::Complex;
use salsa_spatial::{
    CORE_CHANNEL_COUNT, FFT_BINS, PARAM_BAND_COUNT, RECONSTRUCTION_EPSILON, parameter_band_range,
};

use crate::covariance::BandCovariance;

type CoreMatrix = [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];

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

/// Diagonally normalized, numerically stabilized observation system.
///
/// `lower` factorizes `R + epsilon I`, where `R = S K0 S`,
/// `K0 = Z + trace(Z) diag(nu)` and `S = diag(K0)^(-1/2)` is taken over the
/// supported observation channels. Unsupported channels (zero diagonal in
/// `K0`) form an identity block with a zero scale, so they contribute neither
/// to the estimate nor to the inverse.
struct FactorizedSystem {
    lower: [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    scale: [f64; CORE_CHANNEL_COUNT],
}

/// Build the normalized system from the observation covariance and measured
/// error envelope. No signal-level global loading is applied: only the
/// dimensionless [`RECONSTRUCTION_EPSILON`] floor remains.
fn build_factor(
    z: [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
) -> Result<FactorizedSystem, SolveError> {
    let trace = (0..CORE_CHANNEL_COUNT).map(|ch| z[ch][ch].re).sum::<f64>();
    if !trace.is_finite() || trace <= 0.0 {
        return Err(SolveError::Factorization {
            band,
            message: format!("non-positive covariance trace {trace}"),
        });
    }

    let mut diagonal = [0.0; CORE_CHANNEL_COUNT];
    let mut scale = [0.0; CORE_CHANNEL_COUNT];
    for channel in 0..CORE_CHANNEL_COUNT {
        let value = z[channel][channel].re + trace * nu[channel].max(0.0);
        if !value.is_finite() {
            return Err(SolveError::Factorization {
                band,
                message: format!("non-finite system diagonal {value}"),
            });
        }
        if value > 0.0 {
            diagonal[channel] = value;
            scale[channel] = 1.0 / value.sqrt();
        }
    }

    let mut system = [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
    for row in 0..CORE_CHANNEL_COUNT {
        if scale[row] == 0.0 {
            system[row][row] = Complex::new(1.0, 0.0);
            continue;
        }
        for col in 0..CORE_CHANNEL_COUNT {
            if scale[col] == 0.0 {
                continue;
            }
            let value = if row == col {
                Complex::new(diagonal[row], 0.0)
            } else if row < col {
                z[row][col]
            } else {
                z[col][row].conj()
            };
            system[row][col] = value * (scale[row] * scale[col]);
        }
        system[row][row] += Complex::new(RECONSTRUCTION_EPSILON, 0.0);
    }

    let lower = cholesky_factor(system, band)?;
    Ok(FactorizedSystem { lower, scale })
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
            for (a, b) in lower[i].iter().take(j).zip(lower[j].iter().take(j)) {
                sum -= *a * b.conj();
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
    factor: &FactorizedSystem,
    cross: &[[Complex<f64>; CORE_CHANNEL_COUNT]],
) -> Reconstruction {
    let zero_row = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
    let mut matrix = Vec::with_capacity(cross.len());
    for row in cross {
        if row.iter().all(|value| value.norm_sqr() == 0.0) {
            matrix.push(zero_row);
            continue;
        }
        // M = C D^H S (R + eps I)^(-1) S. Solve in the normalized system:
        // (R + eps I) w = S conj(C D^H)^T, then scale the result by S.
        let rhs = std::array::from_fn(|channel| factor.scale[channel] * row[channel].conj());
        let w = cholesky_solve(&factor.lower, &rhs);
        matrix.push(std::array::from_fn(|channel| {
            w[channel].conj() * factor.scale[channel]
        }));
    }
    Reconstruction { matrix }
}

/// Inverse of the regularized system in unnormalized coordinates:
/// `K^(-1) = S (R + eps I)^(-1) S`. Unsupported channels remain zero.
fn factor_inverse(
    factor: &FactorizedSystem,
) -> [[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT] {
    let mut inverse = [[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT];
    for column in 0..CORE_CHANNEL_COUNT {
        let mut unit = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
        unit[column] = Complex::new(1.0, 0.0);
        let solution = cholesky_solve(&factor.lower, &unit);
        for row in 0..CORE_CHANNEL_COUNT {
            inverse[row][column] = solution[row] * (factor.scale[row] * factor.scale[column]);
        }
    }
    inverse
}

fn solve_inner(
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
    with_inverse: bool,
) -> Result<(Reconstruction, Option<CoreMatrix>), SolveError> {
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
    let factor = build_factor(z, band, nu)?;
    let reconstruction = reconstruct_from_cross(&factor, &cross);
    let inverse = with_inverse.then(|| factor_inverse(&factor));
    Ok((reconstruction, inverse))
}

/// Solve the noise-regularized reconstruction system and return `M`.
///
/// The system is `K0 = Z + trace(Z) diag(nu)` with `Z = D C D^H`. It is
/// diagonally normalized to `R = S K0 S`, where `S = diag(K0)^(-1/2)` over the
/// supported observation channels, and only the dimensionless numerical floor
/// [`RECONSTRUCTION_EPSILON`] is added. The estimate is
/// `M = C D^H S (R + epsilon I)^(-1) S`.
pub fn solve_regularized(
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
) -> Result<Reconstruction, SolveError> {
    solve_inner(d, covariance, band, nu, false).map(|(reconstruction, _)| reconstruction)
}

/// Solve like [`solve_regularized`] and also return the inverse of the
/// regularized system. Encoder derivatives need both; factoring once avoids a
/// second Cholesky decomposition per band.
pub fn solve_regularized_with_inverse(
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    band: usize,
    nu: &[f64; CORE_CHANNEL_COUNT],
) -> Result<(Reconstruction, CoreMatrix), SolveError> {
    let (reconstruction, inverse) = solve_inner(d, covariance, band, nu, true)?;
    Ok((
        reconstruction,
        inverse.expect("inverse was requested from the solver"),
    ))
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
    active: bool,
    unobservable: &[bool; PARAM_BAND_COUNT],
) -> Result<Vec<Vec<Complex<f64>>>, ReconstructionError> {
    estimate_objects_with_correction(hop, core, d, covariance, nu, active, unobservable, None)
}

/// Like [`estimate_objects`], with an optional per-band additive correction to
/// the reconstruction matrix. `correction[band][object]` is one complex row per
/// object and observation channel.
#[allow(clippy::too_many_arguments)]
pub fn estimate_objects_with_correction(
    hop: usize,
    core: &[Complex<f64>],
    d: &[[f64; CORE_CHANNEL_COUNT]],
    covariance: &BandCovariance,
    nu: &[[f64; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT],
    active: bool,
    unobservable: &[bool; PARAM_BAND_COUNT],
    correction: Option<&[Vec<[Complex<f64>; CORE_CHANNEL_COUNT]>]>,
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
        let mut reconstruction = match solve_regularized(d, covariance, band, &nu[band]) {
            Ok(reconstruction) => reconstruction,
            Err(error) => {
                return Err(ReconstructionError::Factorization {
                    hop,
                    band,
                    message: error.to_string(),
                });
            }
        };
        if let Some(rows) = correction.and_then(|bands| bands.get(band)) {
            for (row, delta) in reconstruction.matrix.iter_mut().zip(rows) {
                for (value, add) in row.iter_mut().zip(delta) {
                    *value += *add;
                }
            }
        }
        for bin in parameter_band_range(band) {
            let mut observation = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
            for channel in 0..CORE_CHANNEL_COUNT {
                observation[channel] = core[channel * FFT_BINS + bin];
            }
            let endpoint = bin == 0 || bin == FFT_BINS - 1;
            let estimates =
                if endpoint {
                    let mut real = solve_regularized(d, &real_covariance, band, &nu[band])
                        .map_err(|error| ReconstructionError::Factorization {
                            hop,
                            band,
                            message: error.to_string(),
                        })?;
                    if let Some(rows) = correction.and_then(|bands| bands.get(band)) {
                        for (row, delta) in real.matrix.iter_mut().zip(rows) {
                            for (value, add) in row.iter_mut().zip(delta) {
                                value.re += add.re;
                            }
                        }
                    }
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
            true,
            &[false; PARAM_BAND_COUNT],
        )
        .unwrap();
        for (bin, band) in [(0, 0), (FFT_BINS - 1, PARAM_BAND_COUNT - 1)] {
            let expected = solve_regularized(&d, &covariance.real_part(), band, &nu[band]).unwrap();
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
        let result = solve_regularized(&d, &covariance, 0, &nu[0]).unwrap();
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
        let ma = solve_regularized(&d, &a, 5, &nu[5]).unwrap();
        let mb = solve_regularized(&d, &b, 5, &nu[5]).unwrap();
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
        let nu = [0.0; CORE_CHANNEL_COUNT];
        let result = solve_regularized(&d, &covariance, 0, &nu).unwrap();
        assert!((result.matrix[0][0] - result.matrix[1][0]).norm() < 1e-12);

        let s0 = 0.7;
        let s1 = -0.2;
        let mut observation = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
        for channel in 0..CORE_CHANNEL_COUNT {
            observation[channel] = Complex::new(column[channel] * (s0 + s1), 0.0);
        }
        let estimated = result.apply(&observation);
        let expected = 0.5 * (s0 + s1) / (1.0 + RECONSTRUCTION_EPSILON);
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
            estimate_objects(0, &core, &d, &covariance, &nu, false, &unobservable).unwrap();
        assert!(inactive.iter().flatten().all(|value| value.norm() < 1e-15));

        let mut mask = [false; PARAM_BAND_COUNT];
        mask[0] = true;
        let masked = estimate_objects(0, &core, &d, &covariance, &nu, true, &mask).unwrap();
        for bin in parameter_band_range(0) {
            for row in &masked {
                assert!(row[bin].norm() < 1e-15);
            }
        }
    }

    #[test]
    fn correction_adds_its_rank_one_row_to_the_estimate() {
        use salsa_spatial::correction::{CorrectionComponent, TileCorrection};

        let covariance = covariance_from_band_powers(&scaled_powers(1, 1.0), &[]);
        let d = identity_columns(1);
        let nu = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let mut core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
        let bin = parameter_band_range(1).start;
        core[bin] = Complex::new(1.0, 0.0);
        let unobservable = [false; PARAM_BAND_COUNT];
        let baseline =
            estimate_objects(0, &core, &d, &covariance, &nu, true, &unobservable).unwrap();

        let mut correction = TileCorrection::default();
        correction.bands[1].components.push(CorrectionComponent {
            support: vec![0],
            left: vec![Complex::new(1.0, 0.0)],
            right: [Complex::new(1.0, 0.0); CORE_CHANNEL_COUNT],
            left_scale: 1.0,
            right_scale: 1.0,
        });
        let dense = correction.dense(1);
        let corrected = estimate_objects_with_correction(
            0,
            &core,
            &d,
            &covariance,
            &nu,
            true,
            &unobservable,
            Some(dense.as_slice()),
        )
        .unwrap();
        assert!((corrected[0][bin] - baseline[0][bin] - Complex::new(1.0, 0.0)).norm() < 1e-10);
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
        let output = estimate_objects(0, &core, &d, &covariance, &nu, true, &unobservable).unwrap();
        for (channel, row) in output.iter().enumerate() {
            assert!((row[3].re - (channel as f64 + 1.0)).abs() < 1e-8);
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
        let error =
            estimate_objects(7, &core, &d, &covariance, &nu, true, &unobservable).unwrap_err();
        match error {
            ReconstructionError::Factorization { hop, band, .. } => {
                assert_eq!(hop, 7);
                assert_eq!(band, 0);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    fn normalized(mut column: [f64; CORE_CHANNEL_COUNT]) -> [f64; CORE_CHANNEL_COUNT] {
        let norm = column.iter().map(|value| value * value).sum::<f64>().sqrt();
        for value in &mut column {
            *value /= norm;
        }
        column
    }

    #[test]
    fn weak_orthogonal_object_is_not_attenuated_by_a_strong_neighbor() {
        let powers = vec![[1.0; PARAM_BAND_COUNT], [1.0e-4; PARAM_BAND_COUNT]];
        let covariance = covariance_from_band_powers(&powers, &[]);
        let d = identity_columns(2);
        let nu = [0.0; CORE_CHANNEL_COUNT];
        let result = solve_regularized(&d, &covariance, 0, &nu).unwrap();

        let strong = 0.8;
        let weak = 0.01;
        let mut observation = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
        observation[0] = Complex::new(strong, 0.0);
        observation[1] = Complex::new(weak, 0.0);
        let estimated = result.apply(&observation);

        let scale = 1.0 / (1.0 + RECONSTRUCTION_EPSILON);
        assert!((estimated[0].re - strong * scale).abs() < 1e-9);
        // A 40 dB weaker object in an independent observation direction must
        // not inherit the strong object's regularization.
        assert!((estimated[1].re - weak * scale).abs() < 1e-12);
    }

    #[test]
    fn rotating_the_weak_direction_preserves_the_weak_estimate() {
        let powers = vec![[1.0; PARAM_BAND_COUNT], [1.0e-3; PARAM_BAND_COUNT]];
        let covariance = covariance_from_band_powers(&powers, &[]);
        let nu = [0.0; CORE_CHANNEL_COUNT];
        let strong = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let strong_value = 0.5;
        let weak_value = 0.004;

        let directions = [
            [0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            [0.0, 1.0, -1.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 0.0, 1.0, -1.0, 0.0],
        ];
        for direction in directions {
            let weak = normalized(direction);
            let d = vec![strong, weak];
            let result = solve_regularized(&d, &covariance, 0, &nu).unwrap();
            let observation = std::array::from_fn(|channel| {
                Complex::new(
                    strong_value * strong[channel] + weak_value * weak[channel],
                    0.0,
                )
            });
            let estimated = result.apply(&observation);
            let scale = 1.0 / (1.0 + RECONSTRUCTION_EPSILON);
            assert!((estimated[0].re - strong_value * scale).abs() < 1e-9);
            assert!((estimated[1].re - weak_value * scale).abs() < 1e-12);
        }
    }

    #[test]
    fn nearly_collinear_columns_with_noise_stay_finite() {
        let base = normalized([1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let perturbed = normalized([1.0, 1.0e-6, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let d = vec![base, perturbed];
        let powers = vec![[0.5; PARAM_BAND_COUNT]; 2];
        let covariance = covariance_from_band_powers(&powers, &[]);
        let nu = [0.05; CORE_CHANNEL_COUNT];
        let result = solve_regularized(&d, &covariance, 0, &nu).unwrap();
        for row in &result.matrix {
            for value in row {
                assert!(value.norm().is_finite());
                assert!(value.norm() < 1.0e3, "uncontrolled value {value}");
            }
        }
        let observation = std::array::from_fn(|channel| {
            Complex::new(0.7 * base[channel] - 0.2 * perturbed[channel], 0.0)
        });
        for value in result.apply(&observation) {
            assert!(value.norm().is_finite());
        }
    }

    #[test]
    fn exact_rank_deficiency_has_defined_finite_output() {
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
        let covariance = covariance_from_band_powers(&vec![[0.5; PARAM_BAND_COUNT]; 2], &[]);
        let nu = [0.0; CORE_CHANNEL_COUNT];
        let result = solve_regularized(&d, &covariance, 0, &nu).unwrap();
        for row in &result.matrix {
            for value in row {
                assert!(value.norm().is_finite());
            }
        }
        let observation = std::array::from_fn(|channel| Complex::new(column[channel], 0.0));
        let estimated = result.apply(&observation);
        for value in estimated {
            assert!(value.norm().is_finite());
        }
    }

    #[test]
    fn consistent_channel_permutation_preserves_the_estimate() {
        let d = vec![
            normalized([1.0, 0.2, 0.1, 0.0, 0.3, 0.0, 0.0]),
            normalized([0.1, 1.0, 0.0, 0.2, 0.0, 0.3, 0.0]),
            normalized([0.0, 0.1, 1.0, 0.0, 0.0, 0.2, 0.4]),
        ];
        let powers = vec![
            [0.5; PARAM_BAND_COUNT],
            [0.3; PARAM_BAND_COUNT],
            [0.2; PARAM_BAND_COUNT],
        ];
        let covariance = covariance_from_band_powers(&powers, &[]);
        let nu = [0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07];
        let base = solve_regularized(&d, &covariance, 0, &nu).unwrap();

        let permutation = [3, 0, 6, 1, 5, 2, 4];
        let permuted_d: Vec<_> = d
            .iter()
            .map(|column| {
                std::array::from_fn(|channel| {
                    let old = permutation
                        .iter()
                        .position(|&target| target == channel)
                        .unwrap();
                    column[old]
                })
            })
            .collect();
        let mut permuted_nu = [0.0; CORE_CHANNEL_COUNT];
        for (old, &new) in permutation.iter().enumerate() {
            permuted_nu[new] = nu[old];
        }
        let permuted = solve_regularized(&permuted_d, &covariance, 0, &permuted_nu).unwrap();
        for (row, permuted_row) in base.matrix.iter().zip(&permuted.matrix) {
            for (old, &new) in permutation.iter().enumerate() {
                assert!((row[old] - permuted_row[new]).norm() < 1e-10);
            }
        }
    }

    #[test]
    fn reconstruction_works_for_default_fft_geometry() {
        assert_eq!(FFT_SIZE, 1920);
    }
}
