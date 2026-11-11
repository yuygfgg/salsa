//! Encoder-side fitting of a rank-r reconstruction-matrix correction.
//!
//! For each 100 ms tile we measure the source/decoded-core moments and the
//! current pair-model estimator `M0`. The empirical estimator
//! `M_star = R_sy R_yy^dagger` gives the missing mapping. We compress
//! `Delta = M_star - M0` into a few sparse-support rank-one factors per band,
//! quantize them on the transport grid, and evaluate the *actual* deterministic
//! reconstruction error of the quantized correction against the pair baseline.

use anyhow::{Result, bail};
use nalgebra::{DMatrix, DVector};
use num_complex::Complex;
use salsa_spatial::correction::{
    BandCorrectionSet, CORRECTION_MAX_KEYFRAMES, CORRECTION_MAX_RANK, CORRECTION_MAX_SUPPORT,
    CorrectionComponent, TileCorrection, encode_keyframes, quantize_complex,
};
use salsa_spatial::params::ParameterTile;
use salsa_spatial::{CORE_CHANNEL_COUNT, PARAM_BAND_COUNT};
use salsa_spatial_dsp::reconstruct::solve_regularized;
use salsa_spatial_dsp::{BandCovariance, covariance_from_band_powers};

use crate::measurement::TileMeasurements;
use crate::objective::{candidate_powers, compact_pairs, power_parameters};

type ComplexRow = [Complex<f64>; CORE_CHANNEL_COUNT];

/// Fitting options for one tile correction.
#[derive(Debug, Clone, Copy)]
pub struct CorrectionOptions {
    pub rank: usize,
    pub support: usize,
    /// Number of correction keyframes spread over one tile.
    pub keyframes: usize,
    /// Distortion reduction required per transmitted correction byte.
    pub rate_weight: f64,
}

impl Default for CorrectionOptions {
    fn default() -> Self {
        Self {
            rank: 4,
            support: 16,
            keyframes: 3,
            rate_weight: 0.6,
        }
    }
}

/// Measured result of fitting one tile.
#[derive(Debug, Clone, Default)]
pub struct CorrectionFit {
    pub corrections: Vec<TileCorrection>,
    pub bytes: usize,
    pub signal: f64,
    pub baseline_error: f64,
    pub corrected_error: f64,
    pub components: usize,
    pub bands: usize,
    pub keyframes: usize,
}

#[derive(Debug, Clone, Default)]
struct WindowFit {
    correction: TileCorrection,
    signal: f64,
    baseline_error: f64,
    corrected_error: f64,
    components: usize,
    bands: usize,
}

impl CorrectionFit {
    pub fn baseline_snr_db(&self) -> f64 {
        snr_db(self.signal, self.baseline_error)
    }

    pub fn corrected_snr_db(&self) -> f64 {
        snr_db(self.signal, self.corrected_error)
    }
}

fn snr_db(signal: f64, error: f64) -> f64 {
    if signal <= 0.0 || error <= 0.0 {
        0.0
    } else {
        10.0 * (signal / error).log10()
    }
}

/// Fit and quantize correction keyframes for one tile from measured moments.
///
/// `rank == 0` returns an empty result. The routine never changes `tile`.
pub fn fit_tile_corrections(
    measurements: &TileMeasurements,
    tile: &ParameterTile,
    options: CorrectionOptions,
) -> Result<CorrectionFit> {
    if options.rank == 0 || options.support == 0 {
        return Ok(CorrectionFit::default());
    }
    let hop_count = measurements.stats.per_hop().len();
    if hop_count == 0 {
        return Ok(CorrectionFit::default());
    }
    let keyframes = options
        .keyframes
        .clamp(1, CORRECTION_MAX_KEYFRAMES)
        .min(hop_count);
    let mut corrections = Vec::with_capacity(keyframes);
    let mut fit = CorrectionFit {
        keyframes,
        ..CorrectionFit::default()
    };
    for keyframe in 0..keyframes {
        let start = hop_count * keyframe / keyframes;
        let end = hop_count * (keyframe + 1) / keyframes;
        let window = if start < end {
            fit_window(measurements, tile, options, start, end)?
        } else {
            WindowFit::default()
        };
        fit.signal += window.signal;
        fit.baseline_error += window.baseline_error;
        fit.corrected_error += window.corrected_error;
        fit.components += window.components;
        fit.bands += window.bands;
        corrections.push(window.correction);
    }
    if corrections.iter().any(|correction| !correction.is_empty()) {
        fit.bytes = encode_keyframes(&corrections).len();
    }
    fit.corrections = corrections;
    Ok(fit)
}

/// Single-keyframe compatibility wrapper.
pub fn fit_tile_correction(
    measurements: &TileMeasurements,
    tile: &ParameterTile,
    mut options: CorrectionOptions,
) -> Result<CorrectionFit> {
    options.keyframes = 1;
    fit_tile_corrections(measurements, tile, options)
}

fn fit_window(
    measurements: &TileMeasurements,
    tile: &ParameterTile,
    options: CorrectionOptions,
    hop_start: usize,
    hop_end: usize,
) -> Result<WindowFit> {
    if options.rank == 0 || options.support == 0 {
        return Ok(WindowFit::default());
    }
    let rank = options.rank.min(CORRECTION_MAX_RANK);
    let support = options.support.min(CORRECTION_MAX_SUPPORT);
    let active = &measurements.active_objects;
    let active_count = active.len();
    if active_count == 0 {
        return Ok(WindowFit::default());
    }
    let mut compact_of = vec![usize::MAX; measurements.stats.object_count()];
    for (compact, &object) in active.iter().enumerate() {
        compact_of[object] = compact;
    }
    let pairs = compact_pairs(&tile.pairs, &compact_of);
    let values = power_parameters(tile);
    let stride = measurements.stats.object_count() * PARAM_BAND_COUNT;
    let covariances: Vec<BandCovariance> = tile
        .anchors
        .iter()
        .zip(values.chunks(stride))
        .map(|(anchor, values)| {
            covariance_from_band_powers(&candidate_powers(anchor, values, active), &pairs)
        })
        .collect();
    if covariances.is_empty() {
        return Ok(WindowFit::default());
    }

    let all_hops = measurements.stats.per_hop();
    let hops = &all_hops[hop_start..hop_end];
    let hop_count = hops.len();
    if hop_count == 0 {
        return Ok(WindowFit::default());
    }
    // [hop][band] -> compact object rows of M0, empty when the band is skipped.
    let mut m0_hops: Vec<Vec<Vec<ComplexRow>>> =
        vec![vec![Vec::new(); PARAM_BAND_COUNT]; hop_count];
    let mut r_sy: Vec<Vec<ComplexRow>> =
        vec![vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; active_count]; PARAM_BAND_COUNT];
    let mut r_yy =
        vec![[[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
    let mut m0_ry: Vec<Vec<ComplexRow>> =
        vec![vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; active_count]; PARAM_BAND_COUNT];
    let mut source_power = vec![vec![0.0; active_count]; PARAM_BAND_COUNT];

    for (index, hop) in hops.iter().enumerate() {
        let after = tile
            .anchors
            .partition_point(|anchor| anchor.sample <= hop.sample);
        let left = after.saturating_sub(1);
        let right = after.min(tile.anchors.len() - 1);
        let first = &tile.anchors[left];
        let second = &tile.anchors[right];
        let amount = if first.sample == second.sample {
            0.0
        } else {
            hop.sample.saturating_sub(first.sample) as f64 / (second.sample - first.sample) as f64
        };
        let covariance = covariances[left].interpolate(&covariances[right], amount);
        let d_recon: Vec<[f64; CORE_CHANNEL_COUNT]> = active
            .iter()
            .map(|&object| measurements.downmix_columns[hop_start + index][object])
            .collect();
        for band in 0..PARAM_BAND_COUNT {
            if covariance.trace(band) <= salsa_spatial_dsp::reconstruct::SILENCE_EPSILON
                || (0..CORE_CHANNEL_COUNT).any(|channel| {
                    first.error.unobservable[channel][band]
                        || second.error.unobservable[channel][band]
                })
            {
                continue;
            }
            let nu = std::array::from_fn(|channel| {
                (1.0 - amount) * first.error.nu[channel][band]
                    + amount * second.error.nu[channel][band]
            });
            let Ok(reconstruction) = solve_regularized(&d_recon, &covariance, band, &nu) else {
                continue;
            };
            if reconstruction.matrix.len() != active_count {
                continue;
            }
            let data = &hop.bands[band];
            for (compact, &object) in active.iter().enumerate() {
                source_power[band][compact] += data.source_power[object];
                for (target, &value) in r_sy[band][compact]
                    .iter_mut()
                    .zip(&data.source_decoded[object])
                {
                    *target += value;
                }
            }
            for (row, source) in r_yy[band].iter_mut().zip(&data.decoded_core) {
                for (target, &value) in row.iter_mut().zip(source) {
                    *target += value;
                }
            }
            for (compact, row) in m0_ry[band].iter_mut().take(active_count).enumerate() {
                for (channel, target) in row.iter_mut().enumerate() {
                    let mut value = Complex::new(0.0, 0.0);
                    for shared in 0..CORE_CHANNEL_COUNT {
                        value += reconstruction.matrix[compact][shared]
                            * data.decoded_core[shared][channel];
                    }
                    *target += value;
                }
            }
            m0_hops[index][band] = reconstruction.matrix;
        }
    }

    let mut correction = TileCorrection::default();
    for band in 0..PARAM_BAND_COUNT {
        if source_power[band].iter().sum::<f64>() <= 0.0 {
            continue;
        }
        let r_yy_matrix = matrix_from_rows(&r_yy[band]);
        let Some(factors) = HermitianFactors::new(&r_yy_matrix) else {
            continue;
        };
        let r_sy_matrix = DMatrix::from_fn(active_count, CORE_CHANNEL_COUNT, |row, col| {
            r_sy[band][row][col]
        });
        let m0_ry_matrix = DMatrix::from_fn(active_count, CORE_CHANNEL_COUNT, |row, col| {
            m0_ry[band][row][col]
        });
        let m_star = &r_sy_matrix * &factors.pinv;
        let m0_bar = &m0_ry_matrix * &factors.pinv;
        let m0_bar_rows: Vec<ComplexRow> = (0..active_count)
            .map(|row| std::array::from_fn(|col| m0_bar[(row, col)]))
            .collect();
        let difference = m_star - &m0_bar;
        let mut delta = vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; active_count];
        let mut band_components: Vec<CorrectionComponent> = Vec::new();
        let mut before = aggregate_error(
            &m0_bar_rows,
            &delta,
            &source_power[band],
            &r_sy[band],
            &r_yy[band],
        );
        for _ in 0..rank {
            if !before.is_finite() {
                break;
            }
            let residual = &difference
                - DMatrix::from_fn(active_count, CORE_CHANNEL_COUNT, |row, col| delta[row][col]);
            let weighted = &residual * &factors.sqrt;
            let svd = weighted.svd(true, true);
            let singular = svd.singular_values;
            if singular.is_empty() || singular[0] <= 0.0 {
                break;
            }
            let Some(u) = svd.u else { break };
            let u_column = u.column(0);
            let weight: Vec<f64> = (0..active_count)
                .map(|i| {
                    if source_power[band][i] > 0.0 {
                        u_column[(i, 0)].norm_sqr()
                    } else {
                        0.0
                    }
                })
                .collect();
            let selected = select_support(&weight, support);
            if selected.is_empty() {
                break;
            }
            let umax = selected
                .iter()
                .map(|&compact| u_column[(compact, 0)].norm())
                .fold(0.0f64, f64::max);
            if !umax.is_finite() || umax <= 0.0 {
                break;
            }
            // Quantize the left vector first, then solve for the right row that
            // minimizes the residual with that fixed support and support scale.
            // With an invertible R_yy the optimal row is u^H D / (u^H u),
            // independent of the metric weighting.
            let mut left_values = vec![Complex::new(0.0, 0.0); active_count];
            let mut paired: Vec<(u16, Complex<f64>)> = selected
                .iter()
                .map(|&compact| {
                    let value = quantize_complex(u_column[(compact, 0)] / umax);
                    left_values[compact] = value * umax;
                    (active[compact] as u16, value)
                })
                .collect();
            paired.sort_unstable_by_key(|(object, _)| *object);
            let support_indices: Vec<u16> = paired.iter().map(|(object, _)| *object).collect();
            let left: Vec<Complex<f64>> = paired.into_iter().map(|(_, value)| value).collect();
            let denominator = left_values
                .iter()
                .map(|value| value.norm_sqr())
                .sum::<f64>();
            if !denominator.is_finite() || denominator <= 0.0 {
                break;
            }
            let right_opt: [Complex<f64>; CORE_CHANNEL_COUNT] = std::array::from_fn(|channel| {
                let mut value = Complex::new(0.0, 0.0);
                for compact in 0..active_count {
                    value += left_values[compact].conj() * residual[(compact, channel)];
                }
                value / denominator
            });
            let rmax = right_opt
                .iter()
                .map(|value| value.norm())
                .fold(0.0f64, f64::max);
            if !rmax.is_finite() || rmax <= 0.0 {
                break;
            }
            let right = std::array::from_fn(|channel| quantize_complex(right_opt[channel] / rmax));
            let candidate = CorrectionComponent {
                support: support_indices,
                left,
                right,
                left_scale: umax,
                right_scale: rmax,
            };
            let mut with_candidate = delta.clone();
            apply_component_compact(&candidate, &compact_of, &mut with_candidate);
            let after = aggregate_error(
                &m0_bar_rows,
                &with_candidate,
                &source_power[band],
                &r_sy[band],
                &r_yy[band],
            );
            // Approximate entropy-coded cost: fixed right/scale/header bytes
            // plus support-id and left-nibble symbols per supported object.
            let component_bytes = 16.0 + 1.5 * candidate.support.len() as f64;
            let reduction = before - after;
            let threshold = 1e-12 * before.abs().max(1.0);
            if reduction.partial_cmp(&threshold) != Some(std::cmp::Ordering::Greater)
                || reduction <= options.rate_weight * component_bytes
            {
                break;
            }
            delta = with_candidate;
            before = after;
            band_components.push(candidate);
        }
        correction.bands[band] = BandCorrectionSet {
            components: band_components,
        };
    }

    if correction.is_empty() {
        return Ok(WindowFit::default());
    }

    // Deterministic evaluation on the actual measured moments. The encoder
    // keeps a correction only when it reduces the sample error.
    let mut signal = 0.0;
    let mut baseline_error = 0.0;
    let mut corrected_error = 0.0;
    let mut kept_components = 0usize;
    let mut used_bands = 0usize;
    let mut per_band_baseline = [0.0f64; PARAM_BAND_COUNT];
    let mut per_band_corrected = [0.0f64; PARAM_BAND_COUNT];
    for (index, hop) in hops.iter().enumerate() {
        for band in 0..PARAM_BAND_COUNT {
            let m0 = &m0_hops[index][band];
            if m0.is_empty() {
                continue;
            }
            let data = &hop.bands[band];
            let mut delta = vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; active_count];
            if let Some(set) = correction.band(band) {
                apply_compact(set, &compact_of, &mut delta);
            }
            for (compact, &object) in active.iter().enumerate() {
                let power = data.source_power[object];
                if power == 0.0 && delta[compact].iter().all(|v| v.norm_sqr() == 0.0) {
                    continue;
                }
                signal += power;
                let baseline = power
                    + row_error(
                        &m0[compact],
                        &data.source_decoded[object],
                        &data.decoded_core,
                    );
                let mut corrected_row = m0[compact];
                for (value, correction) in corrected_row.iter_mut().zip(&delta[compact]) {
                    *value += *correction;
                }
                let corrected = power
                    + row_error(
                        &corrected_row,
                        &data.source_decoded[object],
                        &data.decoded_core,
                    );
                baseline_error += baseline;
                corrected_error += corrected;
                per_band_baseline[band] += baseline;
                per_band_corrected[band] += corrected;
            }
        }
    }

    for band in 0..PARAM_BAND_COUNT {
        if per_band_corrected[band] >= per_band_baseline[band] {
            correction.bands[band] = BandCorrectionSet::default();
            corrected_error -= per_band_corrected[band] - per_band_baseline[band];
        }
    }
    for band in &correction.bands {
        if !band.is_empty() {
            used_bands += 1;
            kept_components += band.components.len();
        }
    }
    if kept_components == 0 {
        correction
            .bands
            .iter_mut()
            .for_each(|band| *band = BandCorrectionSet::default());
        return Ok(WindowFit::default());
    }

    Ok(WindowFit {
        correction,
        signal,
        baseline_error,
        corrected_error: corrected_error.max(0.0),
        components: kept_components,
        bands: used_bands,
    })
}

fn matrix_from_rows(rows: &[[Complex<f64>; CORE_CHANNEL_COUNT]]) -> DMatrix<Complex<f64>> {
    DMatrix::from_fn(CORE_CHANNEL_COUNT, CORE_CHANNEL_COUNT, |row, col| {
        rows[row][col]
    })
}

fn select_support(weight: &[f64], limit: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..weight.len()).filter(|&i| weight[i] > 0.0).collect();
    indices.sort_unstable_by(|&a, &b| weight[b].total_cmp(&weight[a]).then(a.cmp(&b)));
    indices.truncate(limit);
    indices
}

fn apply_compact(
    set: &BandCorrectionSet,
    compact_of: &[usize],
    delta: &mut [[Complex<f64>; CORE_CHANNEL_COUNT]],
) {
    for component in &set.components {
        for (&object, &left) in component.support.iter().zip(&component.left) {
            let Some(&compact) = compact_of.get(usize::from(object)) else {
                continue;
            };
            if compact == usize::MAX || compact >= delta.len() {
                continue;
            }
            let left = left * component.left_scale;
            for (target, &right) in delta[compact].iter_mut().zip(&component.right) {
                *target += left * (right * component.right_scale);
            }
        }
    }
}

fn row_error(
    row: &ComplexRow,
    cross: &ComplexRow,
    r_yy: &[[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
) -> f64 {
    // tr(Rss) is supplied by the caller through the power sum. Here we only
    // need the two terms that depend on M: -2 Re M R_sy^H + M Ryy M^H.
    let mut predictive = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
    for shared in 0..CORE_CHANNEL_COUNT {
        let value = row[shared];
        for channel in 0..CORE_CHANNEL_COUNT {
            predictive[channel] += value * r_yy[shared][channel];
        }
    }
    let mut error = 0.0;
    for channel in 0..CORE_CHANNEL_COUNT {
        error += (predictive[channel] * row[channel].conj()).re
            - 2.0 * (row[channel] * cross[channel].conj()).re;
    }
    error
}

fn aggregate_error(
    m0_bar: &[ComplexRow],
    delta: &[ComplexRow],
    source_power: &[f64],
    r_sy: &[ComplexRow],
    r_yy: &[[Complex<f64>; CORE_CHANNEL_COUNT]; CORE_CHANNEL_COUNT],
) -> f64 {
    let mut error = 0.0;
    for compact in 0..m0_bar.len() {
        if source_power[compact] <= 0.0 {
            continue;
        }
        let mut row = m0_bar[compact];
        for (value, correction) in row.iter_mut().zip(&delta[compact]) {
            *value += *correction;
        }
        error += source_power[compact] + row_error(&row, &r_sy[compact], r_yy);
    }
    error
}

fn apply_component_compact(
    component: &CorrectionComponent,
    compact_of: &[usize],
    delta: &mut [[Complex<f64>; CORE_CHANNEL_COUNT]],
) {
    for (&object, &left) in component.support.iter().zip(&component.left) {
        let Some(&compact) = compact_of.get(usize::from(object)) else {
            continue;
        };
        if compact == usize::MAX || compact >= delta.len() {
            continue;
        }
        let left = left * component.left_scale;
        for (value, right) in delta[compact].iter_mut().zip(&component.right) {
            *value += left * (right * component.right_scale);
        }
    }
}

struct HermitianFactors {
    pinv: DMatrix<Complex<f64>>,
    sqrt: DMatrix<Complex<f64>>,
}

impl HermitianFactors {
    fn new(matrix: &DMatrix<Complex<f64>>) -> Option<Self> {
        let svd = matrix.clone().svd(true, true);
        let singular = svd.singular_values;
        let u = svd.u?;
        let v_t = svd.v_t?;
        let max = singular.iter().copied().fold(0.0f64, f64::max);
        if !max.is_finite() || max <= 0.0 {
            return None;
        }
        let threshold = max * 1e-12;
        let complex_diagonal = |values: Vec<f64>| {
            DMatrix::from_diagonal(&DVector::<Complex<f64>>::from_iterator(
                values.len(),
                values.into_iter().map(|value| Complex::new(value, 0.0)),
            ))
        };
        let inverse: Vec<f64> = singular
            .iter()
            .map(|&value| if value > threshold { 1.0 / value } else { 0.0 })
            .collect();
        let root: Vec<f64> = singular
            .iter()
            .map(|&value| value.max(0.0).sqrt())
            .collect();
        let pinv = v_t.adjoint() * complex_diagonal(inverse) * u.adjoint();
        let sqrt = &u * complex_diagonal(root) * &v_t;
        Some(Self { pinv, sqrt })
    }
}

/// Validate that a fitted correction can be attached to a tile.
pub fn validate_fit(fit: &CorrectionFit, tile: &ParameterTile) -> Result<()> {
    for correction in &fit.corrections {
        if correction.bands.len() != PARAM_BAND_COUNT {
            bail!("correction band count does not match the profile");
        }
        for component in correction.bands.iter().flat_map(|band| &band.components) {
            for &object in &component.support {
                if usize::from(object) >= tile.anchors[0].active.len() {
                    bail!("correction support exceeds the object count");
                }
            }
        }
    }
    Ok(())
}
