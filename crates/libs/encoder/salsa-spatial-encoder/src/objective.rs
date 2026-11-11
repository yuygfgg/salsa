//! Render-weighted distortion and its derivatives on the decoder hop grid.

use crate::measurement::TileMeasurements;
use anyhow::Result;
use num_complex::Complex;
use objectaudio_ir::adm_to_q15_f64;
use objectaudio_render::{
    BLOCK_SAMPLES, SpeakerControls, SpeakerMetadata, SpeakerRenderer, layout_by_name,
};
use rayon::prelude::*;
use salsa_spatial::params::ParameterAnchor;
use salsa_spatial::{CorrelationPair, PARAM_BAND_COUNT, RECONSTRUCTION_EPSILON};
use salsa_spatial_dsp::reconstruct::solve_regularized_with_inverse;
use salsa_spatial_dsp::{
    BandCovariance, core_gain_column, covariance_from_band_powers, solve_regularized,
};
use std::sync::Arc;

type Matrix = Vec<Vec<f64>>;
type ComplexRow = [Complex<f64>; 7];

struct BandData {
    r_sy: Vec<ComplexRow>,
    r_yy: [[Complex<f64>; 7]; 7],
    /// Reference-render cross spectra for the compact active set.
    r_render_y: [Vec<ComplexRow>; 3],
}

struct SpatialMatrices {
    a_dec: [Matrix; 3],
    d_recon: Vec<[f64; 7]>,
}

struct HopData {
    sample: u64,
    spatial: Arc<SpatialMatrices>,
    bands: Vec<BandData>,
}

pub struct RenderContext {
    object_count: usize,
    hops: Vec<HopData>,
    pub active_objects: Vec<usize>,
    compact_of: Vec<usize>,
    reference_energy: [f64; 3],
    object_reference_energy: f64,
    floor: f64,
}

struct HopEvaluation {
    target_error: [f64; 3],
    object_error: f64,
    gradient: Vec<f64>,
    pair_scores: Vec<f64>,
}

fn core_matrix(positions: &[[f64; 3]], active: &[usize]) -> Matrix {
    let mut matrix = vec![vec![0.0; active.len()]; 7];
    for (column, &object) in active.iter().enumerate() {
        let gains = core_gain_column(positions[object]);
        for channel in 0..7 {
            matrix[channel][column] = gains[channel];
        }
    }
    matrix
}

fn layout_matrix(
    renderer: &mut SpeakerRenderer,
    positions: &[[f64; 3]],
    active: &[usize],
) -> Matrix {
    let channels = renderer.layout().channel_count();
    let mut matrix = vec![vec![0.0; active.len()]; channels];
    let controls = SpeakerControls::default_for(renderer.layout());
    let input: Vec<f32> = (0..BLOCK_SAMPLES).flat_map(|_| [0.0, 1.0]).collect();
    let mut output = vec![0.0; BLOCK_SAMPLES * channels];
    for (column, &object) in active.iter().enumerate() {
        let position = [adm_to_q15_f64(positions[object])];
        let metadata = SpeakerMetadata::new(0, 0, &position);
        renderer
            .process(&input, BLOCK_SAMPLES, &[metadata], &controls, &mut output)
            .expect("valid point-object render");
        for channel in 0..channels {
            matrix[channel][column] = output[channel];
        }
    }
    matrix
}

/// Select the compact active columns of a full-object downmix matrix.
fn compact_columns(columns: &[[f64; 7]], active: &[usize]) -> Vec<[f64; 7]> {
    active.iter().map(|&object| columns[object]).collect()
}

impl RenderContext {
    pub fn new(measurements: &TileMeasurements) -> Self {
        let object_count = measurements.stats.object_count();
        let active_objects = measurements.active_objects.clone();
        let mut compact_of = vec![usize::MAX; object_count];
        for (compact, &object) in active_objects.iter().enumerate() {
            compact_of[object] = compact;
        }
        let mut renderer_714 = SpeakerRenderer::new(layout_by_name("7.1.4").unwrap(), 1);
        let mut renderer_916 = SpeakerRenderer::new(layout_by_name("9.1.6").unwrap(), 1);
        let mut reference_energy = [0.0; 3];
        let mut object_reference_energy = 0.0;
        let hop_count = measurements.hop_count;
        let mut reference_matrices: Vec<Arc<[Matrix; 3]>> = Vec::with_capacity(hop_count);
        let mut spatials: Vec<Arc<SpatialMatrices>> = Vec::with_capacity(hop_count);

        for (index, hop) in measurements.stats.per_hop().iter().enumerate() {
            let reference_positions = &measurements.source_positions[index];
            let decoded_positions = &measurements.recon_positions[index];
            if index == 0 || *reference_positions != measurements.source_positions[index - 1] {
                let matrices = [
                    core_matrix(reference_positions, &active_objects),
                    layout_matrix(&mut renderer_714, reference_positions, &active_objects),
                    layout_matrix(&mut renderer_916, reference_positions, &active_objects),
                ];
                reference_matrices.push(Arc::new(matrices));
            } else {
                reference_matrices.push(Arc::clone(&reference_matrices[index - 1]));
            }
            let spatial = if index > 0
                && *decoded_positions == measurements.recon_positions[index - 1]
                && measurements.downmix_columns[index] == measurements.downmix_columns[index - 1]
            {
                Arc::clone(&spatials[index - 1])
            } else {
                Arc::new(SpatialMatrices {
                    a_dec: [
                        core_matrix(decoded_positions, &active_objects),
                        layout_matrix(&mut renderer_714, decoded_positions, &active_objects),
                        layout_matrix(&mut renderer_916, decoded_positions, &active_objects),
                    ],
                    d_recon: compact_columns(&measurements.downmix_columns[index], &active_objects),
                })
            };
            spatials.push(Arc::clone(&spatial));
            for band in &hop.bands {
                let cross = &measurements.source_cross_hops[index][band.band];
                for target in 0..3 {
                    reference_energy[target] +=
                        rendered_power(&reference_matrices[index][target], cross);
                }
                object_reference_energy += band.source_power.iter().sum::<f64>();
            }
        }

        let total = reference_energy.iter().sum::<f64>() + object_reference_energy;
        let floor = 1e-10 * total.max(1e-30) + 1e-30;
        let mut hops: Vec<HopData> = Vec::with_capacity(hop_count);
        for (index, hop) in measurements.stats.per_hop().iter().enumerate() {
            let spatial = &spatials[index];
            let reference = &reference_matrices[index];
            let bands = hop
                .bands
                .iter()
                .map(|band| {
                    let r_sy: Vec<ComplexRow> = active_objects
                        .iter()
                        .map(|&object| band.source_decoded[object])
                        .collect();
                    let r_render_y = std::array::from_fn(|target| {
                        reference[target]
                            .iter()
                            .map(|row| {
                                let mut result = [Complex::new(0.0, 0.0); 7];
                                for (i, cross) in r_sy.iter().enumerate() {
                                    let gain = row[i];
                                    if gain == 0.0 {
                                        continue;
                                    }
                                    for (result, cross) in result.iter_mut().zip(cross) {
                                        *result += gain * cross;
                                    }
                                }
                                result
                            })
                            .collect()
                    });
                    BandData {
                        r_sy,
                        r_yy: band.decoded_core,
                        r_render_y,
                    }
                })
                .collect();
            hops.push(HopData {
                sample: hop.sample,
                spatial: Arc::clone(spatial),
                bands,
            });
        }
        Self {
            object_count,
            hops,
            active_objects,
            compact_of,
            reference_energy,
            object_reference_energy,
            floor,
        }
    }
}

/// Rendered power of the compact source cross covariance through one target
/// matrix. Columns correspond to `active_objects`, so this is a plain `A C A^T`
/// trace with no full-object indexing.
fn rendered_power(matrix: &Matrix, cross: &[Complex<f64>]) -> f64 {
    let n = matrix.first().map_or(0, Vec::len);
    let mut power = 0.0;
    for row in matrix {
        for i in 0..n {
            let left = row[i];
            if left == 0.0 {
                continue;
            }
            for j in 0..n {
                power += left * cross[i * n + j].re * row[j];
            }
        }
    }
    power.max(0.0)
}

pub fn power_parameters(tile: &salsa_spatial::params::ParameterTile) -> Vec<f64> {
    tile.anchors
        .iter()
        .flat_map(|anchor| {
            anchor
                .ell_half_db
                .iter()
                .flat_map(|bands| bands.iter().map(|&x| f64::from(x)))
        })
        .collect()
}

pub(crate) fn candidate_powers(
    anchor: &ParameterAnchor,
    values: &[f64],
    active: &[usize],
) -> Vec<[f64; PARAM_BAND_COUNT]> {
    let mut powers = vec![[0.0; PARAM_BAND_COUNT]; active.len()];
    for band in 0..PARAM_BAND_COUNT {
        let mut max_log = f64::NEG_INFINITY;
        for &object in active {
            if anchor.active[object] {
                max_log = max_log.max(values[object * PARAM_BAND_COUNT + band]);
            }
        }
        if !max_log.is_finite() {
            continue;
        }
        let mut sum = 0.0;
        for (compact, &object) in active.iter().enumerate() {
            if anchor.active[object] {
                let relative = values[object * PARAM_BAND_COUNT + band] - max_log;
                let power = (relative * std::f64::consts::LN_10 / 20.0).exp();
                powers[compact][band] = power;
                sum += power;
            }
        }
        for row in &mut powers {
            row[band] /= sum;
        }
    }
    powers
}

/// Remap full-object correlation pairs onto the compact active set. Pairs that
/// involve an object with no energy in this tile are dropped.
pub(crate) fn compact_pairs(
    pairs: &[CorrelationPair],
    compact_of: &[usize],
) -> Vec<CorrelationPair> {
    pairs
        .iter()
        .filter_map(|pair| {
            let a = *compact_of.get(usize::from(pair.a))?;
            let b = *compact_of.get(usize::from(pair.b))?;
            if a == usize::MAX || b == usize::MAX {
                return None;
            }
            Some(CorrelationPair {
                a: a as u16,
                b: b as u16,
                rho: pair.rho,
            })
        })
        .collect()
}

pub fn evaluate_distortion(
    tile: &salsa_spatial::params::ParameterTile,
    context: &RenderContext,
) -> Result<f64> {
    Ok(evaluate_parameters(tile, context, &power_parameters(tile), false)?.0)
}

pub fn evaluate_power_gradient(
    tile: &salsa_spatial::params::ParameterTile,
    context: &RenderContext,
    values: &[f64],
) -> Result<(f64, Vec<f64>)> {
    evaluate_parameters(tile, context, values, true)
}

/// First-order distortion changes for adding each quantized correlation pair.
/// These scores only rank proposals; selection uses full distortion and rate.
pub fn evaluate_pair_scores(
    tile: &salsa_spatial::params::ParameterTile,
    context: &RenderContext,
    candidates: &[CorrelationPair],
) -> Result<Vec<f64>> {
    let mut scores = vec![0.0; candidates.len()];
    evaluate_parameters_and_pairs(
        tile,
        context,
        &power_parameters(tile),
        false,
        candidates,
        &mut scores,
    )?;
    Ok(scores)
}

fn evaluate_parameters(
    tile: &salsa_spatial::params::ParameterTile,
    context: &RenderContext,
    values: &[f64],
    derivatives: bool,
) -> Result<(f64, Vec<f64>)> {
    evaluate_parameters_and_pairs(tile, context, values, derivatives, &[], &mut [])
}

fn evaluate_parameters_and_pairs(
    tile: &salsa_spatial::params::ParameterTile,
    context: &RenderContext,
    values: &[f64],
    derivatives: bool,
    pair_candidates: &[CorrelationPair],
    pair_scores: &mut [f64],
) -> Result<(f64, Vec<f64>)> {
    let needs_derivatives = derivatives || !pair_candidates.is_empty();
    let count = context.object_count;
    let stride = count * PARAM_BAND_COUNT;
    if count == 0 {
        return Ok((0.0, Vec::new()));
    }
    if tile.anchors.is_empty() {
        return Ok((1.1, vec![0.0; values.len()]));
    }
    let active = &context.active_objects;
    let active_count = active.len();
    let pairs = compact_pairs(&tile.pairs, &context.compact_of);
    let covariances: Vec<_> = tile
        .anchors
        .iter()
        .zip(values.chunks(stride))
        .map(|(anchor, values)| {
            covariance_from_band_powers(&candidate_powers(anchor, values, active), &pairs)
        })
        .collect();
    let pair_compact: Vec<Option<CorrelationPair>> = pair_candidates
        .iter()
        .map(|pair| {
            let a = *context.compact_of.get(usize::from(pair.a))?;
            let b = *context.compact_of.get(usize::from(pair.b))?;
            if a == usize::MAX || b == usize::MAX {
                return None;
            }
            Some(CorrelationPair {
                a: a as u16,
                b: b as u16,
                rho: pair.rho,
            })
        })
        .collect();
    let object_weight = 0.1 / context.object_reference_energy.max(context.floor);
    let target_weights = context
        .reference_energy
        .map(|energy| 1.0 / (3.0 * energy.max(context.floor)));
    let gradient_len = if derivatives { values.len() } else { 0 };
    let evaluated: Vec<Result<HopEvaluation>> = context
        .hops
        .par_iter()
        .map(|hop| {
            let mut target_error = [0.0; 3];
            let mut object_error = 0.0;
            let mut gradient = vec![0.0; gradient_len];
            let mut scores = vec![0.0; pair_candidates.len()];
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
                hop.sample.saturating_sub(first.sample) as f64
                    / (second.sample - first.sample) as f64
            };
            let covariance = covariances[left].interpolate(&covariances[right], amount);
            let d_recon = &hop.spatial.d_recon;
            let mut live: Vec<usize> = Vec::with_capacity(active_count);
            // TODO: retain separate DC/Nyquist statistics for the decoder's real
            // endpoint matrices. This cost currently uses the complex band proxy.
            for band in 0..PARAM_BAND_COUNT {
                if covariance.trace(band) <= salsa_spatial_dsp::reconstruct::SILENCE_EPSILON
                    || (0..7).any(|ch| {
                        first.error.unobservable[ch][band] || second.error.unobservable[ch][band]
                    })
                {
                    continue;
                }
                live.clear();
                live.extend(
                    (0..active_count).filter(|&object| covariance.diagonal(band, object) > 0.0),
                );
                let data = &hop.bands[band];
                let nu = std::array::from_fn(|ch| {
                    (1.0 - amount) * first.error.nu[ch][band] + amount * second.error.nu[ch][band]
                });
                let (matrix, inverse) = if needs_derivatives {
                    let (reconstruction, inverse) =
                        solve_regularized_with_inverse(d_recon, &covariance, band, &nu)?;
                    (reconstruction.matrix, Some(inverse))
                } else {
                    (
                        solve_regularized(d_recon, &covariance, band, &nu)?.matrix,
                        None,
                    )
                };

                let mut g = if needs_derivatives {
                    vec![[Complex::new(0.0, 0.0); 7]; active_count]
                } else {
                    Vec::new()
                };
                let reference_cross = &data.r_render_y;
                for target in 0..3 {
                    let a = &hop.spatial.a_dec[target];
                    for (channel, row) in a.iter().enumerate() {
                        let mut rendered = [Complex::new(0.0, 0.0); 7];
                        for &i in &live {
                            let gain = row[i];
                            if gain == 0.0 {
                                continue;
                            }
                            for k in 0..7 {
                                rendered[k] += gain * matrix[i][k];
                            }
                        }
                        let mut residual = [Complex::new(0.0, 0.0); 7];
                        let mut predictive = [Complex::new(0.0, 0.0); 7];
                        for (l, &value) in rendered.iter().enumerate() {
                            for (k, target) in predictive.iter_mut().enumerate() {
                                *target += value * data.r_yy[l][k];
                            }
                        }
                        for k in 0..7 {
                            let mr = predictive[k];
                            let cross = reference_cross[target][channel][k];
                            target_error[target] +=
                                (mr * rendered[k].conj() - 2.0 * rendered[k] * cross.conj()).re;
                            residual[k] = mr - cross;
                        }
                        if needs_derivatives {
                            let scale = target_weights[target];
                            for &i in &live {
                                let weight = row[i] * scale;
                                if weight == 0.0 {
                                    continue;
                                }
                                for k in 0..7 {
                                    g[i][k] += weight * residual[k];
                                }
                            }
                        }
                    }
                }
                for &i in &live {
                    let mut predictive = [Complex::new(0.0, 0.0); 7];
                    for (l, &value) in matrix[i].iter().enumerate() {
                        for (k, target) in predictive.iter_mut().enumerate() {
                            *target += value * data.r_yy[l][k];
                        }
                    }
                    for k in 0..7 {
                        let mr = predictive[k];
                        let cross = data.r_sy[i][k];
                        object_error +=
                            (mr * matrix[i][k].conj() - 2.0 * matrix[i][k] * cross.conj()).re;
                        if needs_derivatives {
                            g[i][k] += object_weight * (mr - cross);
                        }
                    }
                }
                if needs_derivatives {
                    let inverse = inverse.expect("derivative path factored the system");
                    let differential =
                        CovarianceDifferential::new(d_recon, &matrix, &g, &inverse, &nu);
                    if derivatives {
                        for (anchor, weight) in [(left, 1.0 - amount), (right, amount)] {
                            if weight == 0.0 {
                                continue;
                            }
                            let derivative =
                                differential.log_power_gradient(&covariances[anchor], band);
                            for (compact, &object) in active.iter().enumerate() {
                                gradient[anchor * stride + object * PARAM_BAND_COUNT + band] +=
                                    weight * derivative[compact];
                            }
                        }
                    }
                    for ((pair, compact), score) in pair_candidates
                        .iter()
                        .zip(pair_compact.iter())
                        .zip(scores.iter_mut())
                    {
                        let Some(rho) = pair.rho[band] else {
                            continue;
                        };
                        let Some(compact) = compact else {
                            continue;
                        };
                        let a = usize::from(compact.a);
                        let b = usize::from(compact.b);
                        let scale = (1.0 - amount)
                            * (covariances[left].diagonal(band, a)
                                * covariances[left].diagonal(band, b))
                            .sqrt()
                            + amount
                                * (covariances[right].diagonal(band, a)
                                    * covariances[right].diagonal(band, b))
                                .sqrt();
                        let cross = Complex::from_polar(
                            scale * f64::from(rho.magnitude) / 16.0,
                            f64::from(rho.phase) * std::f64::consts::TAU / 32.0,
                        );
                        *score += 2.0
                            * (differential.entry(b, a) * cross
                                + differential.entry(a, b) * cross.conj())
                            .re;
                    }
                }
            }
            Ok(HopEvaluation {
                target_error,
                object_error,
                gradient,
                pair_scores: scores,
            })
        })
        .collect();

    // Keep the reduction in hop order so worker scheduling cannot change RD
    // decisions through floating-point summation order.
    let mut target_error = context.reference_energy;
    let mut object_error = context.object_reference_energy;
    let mut gradient = vec![0.0; gradient_len];
    for evaluation in evaluated {
        let evaluation = evaluation?;
        for (total, error) in target_error.iter_mut().zip(evaluation.target_error) {
            *total += error;
        }
        object_error += evaluation.object_error;
        for (total, value) in gradient.iter_mut().zip(evaluation.gradient) {
            *total += value;
        }
        for (total, score) in pair_scores.iter_mut().zip(evaluation.pair_scores) {
            *total += score;
        }
    }
    let distortion = target_error
        .iter()
        .zip(target_weights)
        .map(|(error, weight)| error.max(0.0) * weight)
        .sum::<f64>()
        + object_error.max(0.0) * object_weight;
    Ok((distortion, gradient))
}

/// The differential is dJ = 2 Re tr(H dC). Only diagonal and pair
/// entries of H are needed, so no dense object-by-object matrix is formed.
///
/// The system is `K = K0 + epsilon diag(K0)` with `K0 = Z + trace(Z) diag(nu)`.
/// Both the trace-scaled error envelope and the diagonal numerical floor
/// depend on the covariance, so their contributions are part of the
/// differential.
struct CovarianceDifferential<'a> {
    d: &'a [[f64; 7]],
    t: Vec<ComplexRow>,
    v: Vec<ComplexRow>,
    trace_term: f64,
    p_diag: [Complex<f64>; 7],
}

impl<'a> CovarianceDifferential<'a> {
    fn new(
        d: &'a [[f64; 7]],
        m: &[ComplexRow],
        g: &[ComplexRow],
        inverse: &[ComplexRow; 7],
        nu: &[f64; 7],
    ) -> Self {
        let count = d.len();
        let mut gm = [[Complex::new(0.0, 0.0); 7]; 7];
        for object in 0..count {
            for k in 0..7 {
                for l in 0..7 {
                    gm[k][l] += g[object][k].conj() * m[object][l];
                }
            }
        }
        let mut p = [[Complex::new(0.0, 0.0); 7]; 7];
        for k in 0..7 {
            for l in 0..7 {
                p[k][l] = (0..7).map(|m| inverse[k][m] * gm[m][l]).sum();
            }
        }
        let p_diag = std::array::from_fn(|k| p[k][k]);
        let trace_term = (0..7).map(|k| (p_diag[k] * nu[k]).re).sum::<f64>();
        let t = d
            .iter()
            .map(|column| std::array::from_fn(|k| (0..7).map(|l| column[l] * inverse[l][k]).sum()))
            .collect();
        let v = d
            .iter()
            .zip(g)
            .map(|(column, grad)| {
                std::array::from_fn(|k| {
                    grad[k].conj() - (0..7).map(|l| gm[k][l] * column[l]).sum::<Complex<f64>>()
                })
            })
            .collect();
        Self {
            d,
            t,
            v,
            trace_term,
            p_diag,
        }
    }

    fn entry(&self, i: usize, j: usize) -> Complex<f64> {
        let base = (0..7)
            .map(|k| self.t[i][k] * self.v[j][k])
            .sum::<Complex<f64>>();
        let mut floor = Complex::new(0.0, 0.0);
        let mut dd = 0.0;
        for k in 0..7 {
            let product = self.d[i][k] * self.d[j][k];
            floor += self.p_diag[k] * product;
            dd += product;
        }
        base - Complex::new((1.0 + RECONSTRUCTION_EPSILON) * self.trace_term * dd, 0.0)
            - Complex::new(RECONSTRUCTION_EPSILON, 0.0) * floor
    }

    fn log_power_gradient(&self, covariance: &BandCovariance, band: usize) -> Vec<f64> {
        let mut gradient: Vec<_> = (0..self.d.len())
            .map(|i| 2.0 * covariance.diagonal(band, i) * self.entry(i, i).re)
            .collect();
        for pair in covariance.pairs() {
            let cross = pair.coefficient[band];
            let value =
                (self.entry(pair.b, pair.a) * cross + self.entry(pair.a, pair.b) * cross.conj()).re;
            gradient[pair.a] += value;
            gradient[pair.b] += value;
        }
        let total: f64 = gradient.iter().sum();
        for (i, value) in gradient.iter_mut().enumerate() {
            *value =
                (*value - covariance.diagonal(band, i) * total) * std::f64::consts::LN_10 / 20.0;
        }
        gradient
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TileSource, downmix::build_core, measurement::MeasurementContext};
    use salsa_spatial::params::{CorrelationCoefficient, ParameterTile};
    use salsa_spatial::{CoreErrorEnvelope, CorrelationPair, TILE_SAMPLES, TrajectoryKnot};

    #[test]
    fn point_columns_match_the_multi_object_renderer() {
        let positions = [[-0.7, -0.4, 0.3], [0.2, 0.8, 0.0], [0.8, 0.3, 0.9]];
        let values = [0.25f32, -0.5, 0.75];
        for name in ["7.1.4", "9.1.6"] {
            let layout = layout_by_name(name).unwrap();
            let mut one = SpeakerRenderer::new(layout, 1);
            let matrix = layout_matrix(&mut one, &positions, &[0, 1, 2]);
            let mut renderer = SpeakerRenderer::new(layout, positions.len());
            let input: Vec<_> = (0..BLOCK_SAMPLES)
                .flat_map(|_| [0.0, values[0], values[1], values[2]])
                .collect();
            let mut output = vec![0.0; BLOCK_SAMPLES * layout.channel_count()];
            let q15 = positions.map(adm_to_q15_f64);
            renderer
                .process(
                    &input,
                    BLOCK_SAMPLES,
                    &[SpeakerMetadata::new(0, 0, &q15)],
                    &SpeakerControls::default_for(layout),
                    &mut output,
                )
                .unwrap();
            for (channel, row) in matrix.iter().enumerate() {
                let expected = row
                    .iter()
                    .zip(values)
                    .map(|(a, b)| a * f64::from(b))
                    .sum::<f64>();
                assert!((output[channel] - expected).abs() < 1e-12);
            }
        }
    }

    fn problem(positions: &[[f64; 3]], decoded_gain: f32) -> (ParameterTile, RenderContext) {
        let count = positions.len();
        let frames = TILE_SAMPLES;
        let trajectories: Vec<_> = positions
            .iter()
            .map(|&position| {
                vec![TrajectoryKnot {
                    sample: 0,
                    position,
                }]
            })
            .collect();
        let source = TileSource {
            start_sample: 0,
            frames,
            object_count: count,
            object_pcm: (0..count)
                .map(|object| {
                    (0..frames)
                        .map(|i| {
                            (0.131 * i as f32 + object as f32 * 0.6).sin() * 0.2
                                + (i as f32 * (0.051 + object as f32 * 0.012)).cos() * 0.13
                        })
                        .collect()
                })
                .collect(),
            lfe_pcm: vec![0.0; frames],
            positions: (0..frames)
                .flat_map(|_| positions.iter().map(|p| p.map(|x| x as f32)))
                .collect(),
            active: vec![true; frames * count],
            events: Vec::new(),
        };
        let (core, _) = build_core(&source, &trajectories, 1.0);
        let decoded = core
            .each_ref()
            .map(|channel| channel.iter().map(|x| x * decoded_gain).collect());
        let measured = MeasurementContext::new(
            1.0,
            salsa_spatial_dsp::CorePanner::new(),
            salsa_spatial::BaseFold::none(),
        )
        .measure_tile(&source, &core, &decoded, &trajectories, None);
        let anchors = [0, 1920, 4560]
            .into_iter()
            .enumerate()
            .map(|(index, sample)| ParameterAnchor {
                sample,
                active: vec![true; count],
                ell_half_db: (0..count)
                    .map(|object| {
                        std::array::from_fn(|band| {
                            -((object * 7 + band % 3 + index * object) as i32)
                        })
                    })
                    .collect(),
                error: CoreErrorEnvelope {
                    nu: [[f64::from(1.0 - decoded_gain).powi(2); PARAM_BAND_COUNT]; 7],
                    unobservable: [[false; PARAM_BAND_COUNT]; 7],
                },
            })
            .collect();
        let tile = ParameterTile {
            start_sample: 0,
            frames,
            anchors,
            pairs: Vec::new(),
            trajectories,
            correction: Vec::new(),
        };
        (tile, RenderContext::new(&measured))
    }

    #[test]
    fn lossy_core_score_matches_direct_waveform_error() {
        let (tile, context) = problem(&[[-1.0, 1.0, 0.0]], 0.5);
        // The new solver removes the signal-level global loading; only the
        // dimensionless numerical floor remains, so the unit downmix gain is
        // 0.5 / (1 + 0.25) to within the floor.
        let gain: f64 = 0.5 / 1.25;
        let expected = 1.1 * (1.0 - gain).powi(2);
        let actual = evaluate_distortion(&tile, &context).unwrap();
        assert!((actual - expected).abs() < 1e-10, "{actual} != {expected}");
    }

    #[test]
    fn distortion_gradients_and_pair_scores_do_not_depend_on_worker_count() {
        let (mut tile, context) = problem(
            &[[-0.7, -0.1, 0.4], [-0.7, -0.8, 0.4], [0.6, 0.4, 0.1]],
            0.81,
        );
        let pair = CorrelationPair {
            a: 0,
            b: 1,
            rho: [Some(CorrelationCoefficient {
                magnitude: 10,
                phase: 5,
            }); PARAM_BAND_COUNT],
        };
        tile.pairs.push(pair);
        let evaluate = || {
            (
                evaluate_power_gradient(&tile, &context, &power_parameters(&tile)).unwrap(),
                evaluate_pair_scores(&tile, &context, &[pair]).unwrap(),
            )
        };
        let serial = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let parallel = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        assert_eq!(serial.install(evaluate), parallel.install(evaluate));
    }

    #[test]
    fn power_gradient_matches_finite_differences_with_complex_pairs() {
        let (mut tile, context) = problem(
            &[[-0.7, -0.1, 0.4], [-0.7, -0.8, 0.4], [0.6, 0.4, 0.1]],
            0.81,
        );
        tile.pairs.push(CorrelationPair {
            a: 0,
            b: 1,
            rho: [Some(CorrelationCoefficient {
                magnitude: 10,
                phase: 5,
            }); PARAM_BAND_COUNT],
        });
        let values = power_parameters(&tile);
        let (value, gradient) = evaluate_power_gradient(&tile, &context, &values).unwrap();
        assert!((value - evaluate_distortion(&tile, &context).unwrap()).abs() < 1e-12);
        let mut largest: f64 = 0.0;
        for anchor in 0..3 {
            for object in 0..3 {
                for band in [2, 6, 9, 16] {
                    let index = (anchor * 3 + object) * PARAM_BAND_COUNT + band;
                    let mut plus = values.clone();
                    let mut minus = values.clone();
                    plus[index] += 1e-3;
                    minus[index] -= 1e-3;
                    let a = evaluate_parameters(&tile, &context, &plus, false)
                        .unwrap()
                        .0;
                    let b = evaluate_parameters(&tile, &context, &minus, false)
                        .unwrap()
                        .0;
                    let numeric = (a - b) / 2e-3;
                    largest = largest.max(numeric.abs());
                    assert!(
                        (numeric - gradient[index]).abs() < 1e-7,
                        "anchor {anchor}, object {object}, band {band}: {} != {numeric}",
                        gradient[index]
                    );
                }
            }
        }
        assert!(largest > 1e-5);
        let points = crate::optimize::minimize(values, 16, 16.0, |point| {
            evaluate_power_gradient(&tile, &context, point)
        })
        .unwrap();
        let improved = evaluate_parameters(&tile, &context, points.last().unwrap(), false)
            .unwrap()
            .0;
        assert!(improved < value * 0.9, "{improved} did not improve {value}");
    }

    #[test]
    fn complex_pair_differential_matches_finite_differences() {
        use salsa_spatial_dsp::CorrelatedPair;
        let d = [
            [0.8, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0],
            [0.3, 0.95, 0.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.7, 0.0, 0.0, 0.7, 0.0],
        ];
        let nu = [0.03; 7];
        let covariance = |cross| {
            BandCovariance::new(
                3,
                vec![vec![0.3, 0.4, 0.3]; PARAM_BAND_COUNT],
                vec![CorrelatedPair {
                    a: 0,
                    b: 1,
                    coefficient: [cross; PARAM_BAND_COUNT],
                }],
            )
        };
        let cross = Complex::new(0.08, 0.04);
        let target: Vec<ComplexRow> = (0..3)
            .map(|i| std::array::from_fn(|k| Complex::new(0.1 * (i + k) as f64, 0.02 * k as f64)))
            .collect();
        let c = covariance(cross);
        let m = solve_regularized(&d, &c, 0, &nu).unwrap().matrix;
        let g: Vec<ComplexRow> = m
            .iter()
            .zip(&target)
            .map(|(row, target)| std::array::from_fn(|k| row[k] - target[k]))
            .collect();
        let (_, inverse) = solve_regularized_with_inverse(&d, &c, 0, &nu).unwrap();
        let derivative = CovarianceDifferential::new(&d, &m, &g, &inverse, &nu);
        let loss = |cross| {
            let m = solve_regularized(&d, &covariance(cross), 0, &nu)
                .unwrap()
                .matrix;
            m.iter()
                .zip(&target)
                .flat_map(|(row, target)| row.iter().zip(target).map(|(m, t)| (*m - t).norm_sqr()))
                .sum::<f64>()
        };
        for direction in [Complex::new(1.0, 0.0), Complex::new(0.0, 1.0)] {
            let expected = 2.0
                * (derivative.entry(1, 0) * direction + derivative.entry(0, 1) * direction.conj())
                    .re;
            let numeric = (loss(cross + 1e-6 * direction) - loss(cross - 1e-6 * direction)) / 2e-6;
            assert!((numeric - expected).abs() < 1e-6, "{numeric} != {expected}");
        }
    }

    #[test]
    fn spatial_matrices_follow_each_hop() {
        let (_, fixed) = problem(&[[-0.7, -0.1, 0.4]], 1.0);
        assert!(Arc::ptr_eq(&fixed.hops[0].spatial, &fixed.hops[19].spatial));
        // A moving source must generate distinct matrices at the two ends.
        let frames = TILE_SAMPLES;
        let positions: Vec<[f32; 3]> = (0..frames)
            .map(|i| [-0.8 + 1.6 * i as f32 / frames as f32, 0.8, 0.3])
            .collect();
        let source = TileSource {
            start_sample: 0,
            frames,
            object_count: 1,
            object_pcm: vec![vec![0.1; frames]],
            lfe_pcm: vec![0.0; frames],
            positions,
            active: vec![true; frames],
            events: Vec::new(),
        };
        let trajectories = vec![vec![
            TrajectoryKnot {
                sample: 0,
                position: [-0.8, 0.8, 0.3],
            },
            TrajectoryKnot {
                sample: frames as u64,
                position: [0.8, 0.8, 0.3],
            },
        ]];
        let (core, _) = build_core(&source, &trajectories, 1.0);
        let measured = MeasurementContext::new(
            1.0,
            salsa_spatial_dsp::CorePanner::new(),
            salsa_spatial::BaseFold::none(),
        )
        .measure_tile(&source, &core, &core, &trajectories, None);
        let moving = RenderContext::new(&measured);
        assert_ne!(
            moving.hops[0].spatial.d_recon,
            moving.hops[19].spatial.d_recon
        );
        assert_ne!(
            moving.hops[0].spatial.a_dec[1],
            moving.hops[19].spatial.a_dec[1]
        );
    }
    #[test]
    fn rendered_power_includes_correlated_sources() {
        let matrix = vec![vec![1.0, 1.0]];
        let cross = [1.0, -1.0, -1.0, 1.0].map(|value| Complex::new(value, 0.0));
        assert_eq!(rendered_power(&matrix, &cross), 0.0);
        let cross = [Complex::new(1.0, 0.0); 4];
        assert_eq!(rendered_power(&matrix, &cross), 4.0);
    }
}
