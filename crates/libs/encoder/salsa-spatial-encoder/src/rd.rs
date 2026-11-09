//! Per-tile parameter construction and closed-loop `D + beta*R` search.
//!
//! Distortion is evaluated against the original unclustered scene and the
//! actual locally decoded core.  Three speaker targets are used with equal
//! weight (the defined core panner, 7.1.4 and 9.1.6) plus a 0.1-weighted object
//! waveform term.  Candidate error uses the per-band covariance statistics
//! from the shared analysis accumulator.

use std::f64::consts::PI;

use anyhow::Result;
use rayon::prelude::*;
use salsa_spatial::params::{CorrelationCoefficient, ParameterAnchor, SceneParameterEncoder};
use salsa_spatial::{
    CORRECTION_SCALES_DB, CoreErrorEnvelope, CorrelationPair, DCT_COEFF_COUNT, PARAM_BAND_COUNT,
    PowerModel, TrajectoryKnot,
};
use salsa_spatial_dsp::BandStatistics;

use crate::measurement::TileMeasurements;

pub struct CandidateEval {
    pub distortion: f64,
    pub bits: f64,
    pub cost: f64,
}

#[allow(dead_code)]
pub struct SearchResult {
    pub tile: salsa_spatial::params::ParameterTile,
    pub distortion: f64,
    pub bits: f64,
    pub cost: f64,
    pub scale: f64,
}

use crate::objective::{
    RenderContext, evaluate_distortion, evaluate_pair_scores, evaluate_power_gradient,
    power_parameters,
};

pub fn evaluate_candidate(
    tile: &salsa_spatial::params::ParameterTile,
    context: &RenderContext,
    encoder: &SceneParameterEncoder,
    beta: f64,
) -> Result<CandidateEval> {
    let distortion = evaluate_distortion(tile, context)?;
    let bits = encoder.estimate_tile_bits(tile)?;
    Ok(CandidateEval {
        distortion,
        bits,
        cost: distortion + beta * bits,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EllMode {
    Measured,
    HoldPrevious,
}

fn quantize_ell(half_db: i32, scale: f64) -> i32 {
    let step = (2.0 * scale).round().max(1.0) as i32;
    let value = ((f64::from(half_db) / f64::from(step)).round() as i32) * step;
    value.clamp(-180, 0)
}

fn measured_ell(
    bands: &[BandStatistics],
    active: &[bool],
    scale: f64,
) -> Vec<[i32; PARAM_BAND_COUNT]> {
    let object_count = active.len();
    let mut ell = vec![[0i32; PARAM_BAND_COUNT]; object_count];
    for band in 0..PARAM_BAND_COUNT {
        let statistics = &bands[band];
        let mut max_power: f64 = 0.0;
        for object in 0..object_count {
            if active[object] {
                max_power = max_power.max(statistics.source_power[object]);
            }
        }
        for object in 0..object_count {
            if !active[object] {
                continue;
            }
            let value = if max_power > 1e-30 {
                let power = statistics.source_power[object];
                if power > 0.0 {
                    (10.0 * (power / max_power).log10() / 0.5).round() as i32
                } else {
                    -180
                }
            } else {
                -180
            };
            ell[object][band] = quantize_ell(value, scale);
        }
    }
    ell
}

fn envelope_from_bands(bands: &[BandStatistics]) -> CoreErrorEnvelope {
    let mut envelope = CoreErrorEnvelope::default();
    for band in 0..PARAM_BAND_COUNT {
        for channel in 0..7 {
            envelope.nu[channel][band] = bands[band].nu[channel];
            envelope.unobservable[channel][band] = bands[band].unobservable[channel];
        }
    }
    envelope.quantized()
}

fn transient_hops(measurements: &TileMeasurements) -> Vec<usize> {
    let hops = measurements.stats.per_hop();
    let mut transients = Vec::new();
    for index in 1..hops.len() {
        let previous: f64 = hops[index - 1]
            .bands
            .iter()
            .map(|band| band.source_power.iter().sum::<f64>())
            .sum();
        let current: f64 = hops[index]
            .bands
            .iter()
            .map(|band| band.source_power.iter().sum::<f64>())
            .sum();
        if previous <= 1e-30 {
            continue;
        }
        let ratio = (current / previous).max(1e-30);
        if ratio > 4.0 || ratio < 0.25 {
            if index % 4 != 0 {
                transients.push(index);
            }
        }
    }
    transients.truncate(4);
    transients
}

#[allow(clippy::too_many_arguments)]
fn build_anchors(
    measurements: &TileMeasurements,
    start_sample: u64,
    scale: f64,
    transient: bool,
    mode: EllMode,
    previous_ell: &[[i32; PARAM_BAND_COUNT]],
) -> Vec<ParameterAnchor> {
    let hop_count = measurements.hop_count;
    let object_count = previous_ell.len();
    let mut anchors: Vec<ParameterAnchor> = Vec::new();
    let mut hops: Vec<usize> = (0..hop_count).step_by(4).collect();
    if !hops.contains(&(hop_count - 1)) {
        hops.push(hop_count - 1);
    }
    if transient {
        for hop in transient_hops(measurements) {
            if !hops.contains(&hop) {
                hops.push(hop);
            }
        }
    }
    hops.sort_unstable();

    for hop in hops {
        let frame = &measurements.stats.per_hop()[hop];
        let active = measurements.source_active_hops[hop].clone();
        let mut ell = measured_ell(&frame.bands, &active, scale);
        match mode {
            EllMode::Measured => {}
            EllMode::HoldPrevious => {
                for object in 0..object_count {
                    if active[object] {
                        ell[object] = previous_ell[object];
                    }
                }
            }
        }
        let error = envelope_from_bands(&frame.bands);
        anchors.push(ParameterAnchor {
            sample: start_sample + (hop * salsa_spatial::HOP_SAMPLES) as u64,
            active,
            ell_half_db: ell.clone(),
            error,
        });
    }
    anchors
}

fn fit_base(
    anchors: &[ParameterAnchor],
    object_count: usize,
    scale: f64,
) -> Vec<[i32; PARAM_BAND_COUNT]> {
    let mut base = vec![[0i32; PARAM_BAND_COUNT]; object_count];
    let step = (4.0 * scale).round().max(1.0) as i32;
    for object in 0..object_count {
        for band in 0..PARAM_BAND_COUNT {
            let mut sum = 0.0;
            let mut count = 0usize;
            for anchor in anchors {
                if anchor.active[object] {
                    sum += f64::from(anchor.ell_half_db[object][band]);
                    count += 1;
                }
            }
            if count > 0 {
                let average = sum / count as f64;
                let quantized = ((average / f64::from(step)).round() as i32) * step;
                base[object][band] = quantized.clamp(-180, 0);
            }
        }
    }
    base
}

fn measured_pairs(measurements: &TileMeasurements) -> Vec<(CorrelationPair, f64)> {
    let active = &measurements.active_objects;
    let count = active.len();
    let mut candidates = Vec::new();
    for (ai, &a) in active.iter().enumerate() {
        for (bi, &b) in active.iter().enumerate().skip(ai + 1) {
            let mut rho = [None; PARAM_BAND_COUNT];
            let mut score = 0.0;
            let mut bands = 0;
            for band in 0..PARAM_BAND_COUNT {
                let cross = &measurements.source_cross[band];
                let denominator = (cross[ai * count + ai].re * cross[bi * count + bi].re).sqrt();
                if denominator <= 1e-30 {
                    continue;
                }
                let value = cross[ai * count + bi] / denominator;
                score += value.norm();
                bands += 1;
                let magnitude = (value.norm() * 16.0).round().clamp(0.0, 15.0) as u8;
                if magnitude != 0 {
                    let phase = ((value.arg() / (2.0 * PI / 32.0)).round().rem_euclid(32.0)) as u8;
                    rho[band] = Some(CorrelationCoefficient { magnitude, phase });
                }
            }
            if rho.iter().any(Option::is_some) {
                candidates.push((
                    CorrelationPair {
                        a: a as u16,
                        b: b as u16,
                        rho,
                    },
                    score / bands as f64,
                ));
            }
        }
    }
    candidates
}

fn greedy_pairs(
    candidates: &[(CorrelationPair, f64)],
    object_count: usize,
) -> Vec<CorrelationPair> {
    let mut candidates = candidates.to_vec();
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut used = vec![false; object_count];
    let mut pairs = Vec::new();
    for (pair, score) in candidates {
        if pairs.len() == 8 || score < 0.05 {
            break;
        }
        if used[usize::from(pair.a)] || used[usize::from(pair.b)] {
            continue;
        }
        used[usize::from(pair.a)] = true;
        used[usize::from(pair.b)] = true;
        pairs.push(pair);
    }
    pairs
}

fn overlap(a: &CorrelationPair, b: &CorrelationPair) -> bool {
    a.a == b.a || a.a == b.b || a.b == b.a || a.b == b.b
}

fn refine_pairs(
    mut best: Best,
    candidates: &[(CorrelationPair, f64)],
    context: &RenderContext,
    encoder: &SceneParameterEncoder,
    beta: f64,
) -> Result<Best> {
    let candidates: Vec<_> = candidates
        .iter()
        .map(|(pair, _)| *pair)
        .filter(|pair| {
            best.tile.anchors.iter().any(|anchor| {
                anchor.active[usize::from(pair.a)] && anchor.active[usize::from(pair.b)]
            })
        })
        .collect();
    if candidates.is_empty() {
        return Ok(best);
    }
    for _ in 0..8 {
        let scores = evaluate_pair_scores(&best.tile, context, &candidates)?;
        let current_scores: Vec<_> = best
            .tile
            .pairs
            .iter()
            .map(|pair| {
                candidates
                    .iter()
                    .position(|candidate| candidate == pair)
                    .map_or(0.0, |index| scores[index])
            })
            .collect();
        let mut proposals = Vec::new();
        for (pair, &score) in candidates.iter().zip(&scores) {
            if best.tile.pairs.contains(pair) {
                continue;
            }
            let mut remaining = Vec::new();
            let mut change = score;
            for (old, &old_score) in best.tile.pairs.iter().zip(&current_scores) {
                if overlap(pair, old) {
                    change -= old_score;
                } else {
                    remaining.push((*old, old_score));
                }
            }
            if remaining.len() == 8 {
                let remove = remaining
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.1.total_cmp(&b.1))
                    .unwrap()
                    .0;
                change -= remaining.remove(remove).1;
            }
            let mut pairs: Vec<_> = remaining.into_iter().map(|(pair, _)| pair).collect();
            pairs.push(*pair);
            pairs.sort_by_key(|pair| (pair.a, pair.b));
            proposals.push((change, pairs));
        }
        proposals.sort_by(|a, b| a.0.total_cmp(&b.0));
        proposals.truncate(12);
        for index in 0..best.tile.pairs.len() {
            let mut pairs = best.tile.pairs.clone();
            pairs.remove(index);
            proposals.push((0.0, pairs));
        }
        let specs = proposals
            .into_iter()
            .enumerate()
            .map(|(group, (_, pairs))| {
                let mut tile = best.tile.clone();
                tile.pairs = pairs;
                (tile, best.scale, group)
            })
            .collect();
        let pending = distortion_pending(specs, context)?;
        let Some(candidate) = finish_candidates(pending, encoder, beta)? else {
            break;
        };
        if candidate.eval.cost >= best.eval.cost - 1e-12 {
            break;
        }
        best = candidate;
    }
    Ok(best)
}

fn make_tile(
    start_sample: u64,
    frames: usize,
    base: &[[i32; PARAM_BAND_COUNT]],
    anchors: &[ParameterAnchor],
    pairs: &[CorrelationPair],
    trajectories: &[Vec<TrajectoryKnot>],
) -> salsa_spatial::params::ParameterTile {
    salsa_spatial::params::ParameterTile {
        start_sample,
        frames,
        base: PowerModel {
            base_half_db: base.to_vec(),
            fast_half_db: vec![[0i32; DCT_COEFF_COUNT]; base.len()],
        },
        anchors: anchors.to_vec(),
        pairs: pairs.to_vec(),
        trajectories: trajectories.to_vec(),
        correction: Vec::new(),
    }
}

struct Best {
    tile: salsa_spatial::params::ParameterTile,
    eval: CandidateEval,
    scale: f64,
}

struct Pending {
    tile: salsa_spatial::params::ParameterTile,
    distortion: f64,
    scale: f64,
}

fn distortion_pending(
    specs: Vec<(salsa_spatial::params::ParameterTile, f64, usize)>,
    context: &RenderContext,
) -> Result<Vec<Pending>> {
    let mut representatives: Vec<(usize, salsa_spatial::params::ParameterTile)> = Vec::new();
    let mut group_index: Vec<Option<usize>> = Vec::new();
    for (tile, _, group) in &specs {
        if *group >= group_index.len() {
            group_index.resize(*group + 1, None);
        }
        if group_index[*group].is_none() {
            group_index[*group] = Some(representatives.len());
            representatives.push((*group, tile.clone()));
        }
    }
    let evaluated: Vec<Result<(usize, f64)>> = representatives
        .into_par_iter()
        .map(|(group, tile)| {
            let distortion = evaluate_distortion(&tile, context)?;
            Ok((group, distortion))
        })
        .collect();
    let mut distortions: Vec<Option<f64>> = vec![None; group_index.len()];
    for entry in evaluated {
        let (group, distortion) = entry?;
        distortions[group] = Some(distortion);
    }
    let mut pending = Vec::with_capacity(specs.len());
    for (tile, scale, group) in specs {
        let distortion = distortions[group].expect("every distortion group has a representative");
        pending.push(Pending {
            tile,
            distortion,
            scale,
        });
    }
    Ok(pending)
}

fn finish_candidates(
    pending: Vec<Pending>,
    encoder: &SceneParameterEncoder,
    beta: f64,
) -> Result<Option<Best>> {
    if pending.is_empty() {
        return Ok(None);
    }
    let mut pending = pending;
    pending.sort_by(|a, b| a.distortion.total_cmp(&b.distortion));
    let evaluated: Vec<Result<Best>> = pending
        .into_par_iter()
        .map(|pending| {
            let bits = encoder.estimate_tile_bits(&pending.tile)?;
            Ok(Best {
                tile: pending.tile,
                eval: CandidateEval {
                    distortion: pending.distortion,
                    bits,
                    cost: pending.distortion + beta * bits,
                },
                scale: pending.scale,
            })
        })
        .collect();
    let mut best: Option<Best> = None;
    for candidate in evaluated {
        let candidate = candidate?;
        if best
            .as_ref()
            .is_none_or(|current| candidate.eval.cost < current.eval.cost - 1e-12)
        {
            best = Some(candidate);
        }
    }
    Ok(best)
}

fn deactivate_object(tile: &mut salsa_spatial::params::ParameterTile, object: usize) {
    for anchor in &mut tile.anchors {
        anchor.active[object] = false;
        anchor.ell_half_db[object] = [0i32; PARAM_BAND_COUNT];
    }
}

fn toggle_object(
    tile: &mut salsa_spatial::params::ParameterTile,
    object: usize,
    unpruned_anchors: &[ParameterAnchor],
) {
    if tile.anchors.iter().any(|anchor| anchor.active[object]) {
        deactivate_object(tile, object);
        return;
    }
    for (anchor, unpruned) in tile.anchors.iter_mut().zip(unpruned_anchors) {
        anchor.active[object] = unpruned.active[object];
        anchor.ell_half_db[object] = unpruned.ell_half_db[object];
    }
}

fn evaluate_tiles(
    tiles: Vec<salsa_spatial::params::ParameterTile>,
    context: &RenderContext,
    encoder: &SceneParameterEncoder,
    beta: f64,
) -> Result<Vec<(usize, CandidateEval)>> {
    let evaluated: Vec<Result<(usize, CandidateEval)>> = tiles
        .into_par_iter()
        .enumerate()
        .map(|(index, tile)| {
            let distortion = evaluate_distortion(&tile, context)?;
            let bits = encoder.estimate_tile_bits(&tile)?;
            Ok((
                index,
                CandidateEval {
                    distortion,
                    bits,
                    cost: distortion + beta * bits,
                },
            ))
        })
        .collect();
    let mut output = Vec::with_capacity(evaluated.len());
    for entry in evaluated {
        output.push(entry?);
    }
    Ok(output)
}

fn refine_powers(
    best: Best,
    context: &RenderContext,
    encoder: &SceneParameterEncoder,
    beta: f64,
) -> Result<Best> {
    let object_count = best.tile.base.base_half_db.len();
    let stride = object_count * PARAM_BAND_COUNT;
    if stride == 0 {
        return Ok(best);
    }
    let points = crate::optimize::minimize(power_parameters(&best.tile), 16, 16.0, |values| {
        evaluate_power_gradient(&best.tile, context, values)
    })?;
    let mut specs = Vec::new();
    for point in points {
        for &scale in &CORRECTION_SCALES_DB {
            let mut tile = best.tile.clone();
            for (anchor, values) in tile.anchors.iter_mut().zip(point.chunks(stride)) {
                for band in 0..PARAM_BAND_COUNT {
                    let maximum = anchor
                        .active
                        .iter()
                        .enumerate()
                        .filter(|(_, active)| **active)
                        .map(|(object, _)| values[object * PARAM_BAND_COUNT + band])
                        .fold(f64::NEG_INFINITY, f64::max);
                    for object in 0..object_count {
                        if anchor.active[object] {
                            let relative = values[object * PARAM_BAND_COUNT + band] - maximum;
                            anchor.ell_half_db[object][band] =
                                quantize_ell(relative.round().clamp(-180.0, 0.0) as i32, scale);
                        }
                    }
                }
            }
            let group = specs.len();
            specs.push((tile.clone(), scale, group));
            tile.base.base_half_db = fit_base(&tile.anchors, object_count, scale);
            specs.push((tile, scale, group));
        }
    }
    let pending = distortion_pending(specs, context)?;
    if let Some(candidate) = finish_candidates(pending, encoder, beta)?
        && candidate.eval.cost < best.eval.cost - 1e-12
    {
        return Ok(candidate);
    }
    Ok(best)
}

#[allow(clippy::too_many_arguments)]
pub fn search_tile(
    encoder: &SceneParameterEncoder,
    measurements: &TileMeasurements,
    start_sample: u64,
    frames: usize,
    trajectories: &[Vec<TrajectoryKnot>],
    previous_base: &[[i32; PARAM_BAND_COUNT]],
    previous_ell: &[[i32; PARAM_BAND_COUNT]],
    beta: f64,
) -> Result<SearchResult> {
    let started = std::time::Instant::now();
    let object_count = previous_ell.len();
    let context = RenderContext::new(measurements);
    let pair_candidates = measured_pairs(measurements);
    let greedy = greedy_pairs(&pair_candidates, object_count);
    let empty: Vec<CorrelationPair> = Vec::new();
    let transients = transient_hops(measurements);
    let transient_options = if transients.is_empty() {
        vec![false]
    } else {
        vec![false, true]
    };
    let measured_group_count = CORRECTION_SCALES_DB.len() * transient_options.len() * 2;
    let mut specs: Vec<(salsa_spatial::params::ParameterTile, f64, usize)> = Vec::new();
    let mut group = 0usize;

    for &scale in &CORRECTION_SCALES_DB {
        let measured = build_anchors(
            measurements,
            start_sample,
            scale,
            false,
            EllMode::Measured,
            previous_ell,
        );
        let replaced_base = fit_base(&measured, object_count, scale);
        let base_candidates = [(previous_base.to_vec(), false), (replaced_base, true)];
        for transient in &transient_options {
            let anchors = if *transient {
                build_anchors(
                    measurements,
                    start_sample,
                    scale,
                    true,
                    EllMode::Measured,
                    previous_ell,
                )
            } else {
                measured.clone()
            };
            for pairs in [&empty, &greedy] {
                for (base, _) in &base_candidates {
                    specs.push((
                        make_tile(start_sample, frames, base, &anchors, pairs, trajectories),
                        scale,
                        group,
                    ));
                }
                group += 1;
            }
        }
    }

    // Hold-previous anchors are independent of the quantizer scale. Evaluate
    // each pair choice once and reuse its distortion for all four scales.
    let held = build_anchors(
        measurements,
        start_sample,
        1.0,
        false,
        EllMode::HoldPrevious,
        previous_ell,
    );
    for (pair_index, pairs) in [&empty, &greedy].into_iter().enumerate() {
        let held_group = measured_group_count + pair_index;
        for &scale in &CORRECTION_SCALES_DB {
            specs.push((
                make_tile(
                    start_sample,
                    frames,
                    previous_base,
                    &held,
                    pairs,
                    trajectories,
                ),
                scale,
                held_group,
            ));
        }
    }

    let pending = distortion_pending(specs, &context)?;
    let mut best = match finish_candidates(pending, encoder, beta)? {
        Some(best) => best,
        None => {
            let anchors = build_anchors(
                measurements,
                start_sample,
                1.0,
                false,
                EllMode::Measured,
                previous_ell,
            );
            let base = fit_base(&anchors, object_count, 1.0);
            let tile = make_tile(start_sample, frames, &base, &anchors, &empty, trajectories);
            let eval = evaluate_candidate(&tile, &context, encoder, beta)?;
            Best {
                tile,
                eval,
                scale: 1.0,
            }
        }
    };
    let initial_time = started.elapsed();

    best = refine_powers(best, &context, encoder, beta)?;
    let first_power_time = started.elapsed();

    // Keep this tile's fitted powers and source activity for reactivation.
    // Pruning clears powers; previous-tile values are not valid replacements.
    let unpruned_anchors = best.tile.anchors.clone();

    // Quiet source objects are often authored as enabled even when their
    // contribution in this tile is negligible. Deactivating one object at a
    // time would need dozens of sweeps, so first evaluate all currently active
    // objects in parallel and apply the jointly beneficial deactivations.
    for _ in 0..4 {
        let eligible: Vec<usize> = (0..object_count)
            .filter(|&object| best.tile.anchors.iter().all(|anchor| anchor.active[object]))
            .collect();
        if eligible.is_empty() {
            break;
        }
        let mut tiles = Vec::with_capacity(eligible.len());
        for &object in &eligible {
            let mut tile = best.tile.clone();
            deactivate_object(&mut tile, object);
            tiles.push(tile);
        }
        let evaluated = evaluate_tiles(tiles, &context, encoder, beta)?;
        let mut beneficial = Vec::new();
        let mut best_individual: Option<(usize, CandidateEval)> = None;
        for (index, eval) in evaluated {
            if eval.cost < best.eval.cost - 1e-12 {
                beneficial.push(eligible[index]);
            }
            if best_individual
                .as_ref()
                .is_none_or(|(_, current)| eval.cost < current.cost)
            {
                best_individual = Some((index, eval));
            }
        }
        if beneficial.is_empty() {
            break;
        }
        let mut combined = best.tile.clone();
        for object in &beneficial {
            deactivate_object(&mut combined, *object);
        }
        let combined_eval = evaluate_candidate(&combined, &context, encoder, beta)?;
        if combined_eval.cost < best.eval.cost - 1e-12 {
            best = Best {
                tile: combined,
                eval: combined_eval,
                scale: best.scale,
            };
        } else if let Some((index, eval)) = best_individual {
            if eval.cost < best.eval.cost - 1e-12 {
                let mut tile = best.tile.clone();
                deactivate_object(&mut tile, eligible[index]);
                best = Best {
                    tile,
                    eval,
                    scale: best.scale,
                };
            } else {
                break;
            }
        } else {
            break;
        }
    }

    let pruning_time = started.elapsed();
    // Two parallel coordinate sweeps. Each sweep evaluates every active object
    // toggle from the same committed state, plus the base and pair alternates,
    // then advances to the best exact-rate candidate.
    for _ in 0..2 {
        let mut sweep_specs: Vec<(salsa_spatial::params::ParameterTile, f64, usize)> = Vec::new();
        let mut sweep_group = 0usize;
        for &object in &context.active_objects {
            let mut tile = best.tile.clone();
            toggle_object(&mut tile, object, &unpruned_anchors);
            sweep_specs.push((tile, best.scale, sweep_group));
            sweep_group += 1;
        }

        let alternate_base = if best.tile.base.base_half_db.as_slice() == previous_base {
            fit_base(&best.tile.anchors, object_count, best.scale)
        } else {
            previous_base.to_vec()
        };
        let mut tile = best.tile.clone();
        tile.base.base_half_db = alternate_base;
        sweep_specs.push((tile, best.scale, sweep_group));
        sweep_group += 1;

        let alternate_pairs = if best.tile.pairs.is_empty() {
            greedy.clone()
        } else {
            Vec::new()
        };
        let mut tile = best.tile.clone();
        tile.pairs = alternate_pairs;
        sweep_specs.push((tile, best.scale, sweep_group));

        let pending = distortion_pending(sweep_specs, &context)?;
        if let Some(candidate) = finish_candidates(pending, encoder, beta)?
            && candidate.eval.cost < best.eval.cost - 1e-12
        {
            best = candidate;
        }
    }

    let sweep_time = started.elapsed();
    best = refine_pairs(best, &pair_candidates, &context, encoder, beta)?;
    let pair_time = started.elapsed();
    best = refine_powers(best, &context, encoder, beta)?;
    log::debug!(
        "RD {start_sample}: initial {:.1}, powers {:.1}, pruning {:.1}, sweeps {:.1}, pairs {:.1}, final powers {:.1} ms",
        initial_time.as_secs_f64() * 1000.0,
        (first_power_time - initial_time).as_secs_f64() * 1000.0,
        (pruning_time - first_power_time).as_secs_f64() * 1000.0,
        (sweep_time - pruning_time).as_secs_f64() * 1000.0,
        (pair_time - sweep_time).as_secs_f64() * 1000.0,
        (started.elapsed() - pair_time).as_secs_f64() * 1000.0,
    );

    Ok(SearchResult {
        tile: best.tile,
        distortion: best.eval.distortion,
        bits: best.eval.bits,
        cost: best.eval.cost,
        scale: best.scale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex;

    fn sequential_objects() -> (TileMeasurements, Vec<Vec<TrajectoryKnot>>) {
        use crate::{TileSource, measurement::MeasurementContext};
        let frames = salsa_spatial::TILE_SAMPLES;
        let objects: Vec<Vec<f32>> = (0..2)
            .map(|object| {
                (0..frames)
                    .map(|i| {
                        if (i < 1920) == (object == 0) {
                            (i as f32 * 0.11).sin() * 0.2
                        } else {
                            0.0
                        }
                    })
                    .collect()
            })
            .collect();
        let positions = [[-1.0, 1.0, 0.0], [1.0, 1.0, 0.0]];
        let source = TileSource {
            start_sample: 0,
            frames,
            object_count: 2,
            object_pcm: objects.clone(),
            lfe_pcm: vec![0.0; frames],
            positions: (0..frames).flat_map(|_| positions).collect(),
            active: vec![true; 2 * frames],
            events: Vec::new(),
        };
        let core = std::array::from_fn(|ch| {
            if ch < 2 {
                objects[ch].clone()
            } else {
                vec![0.0; frames]
            }
        });
        let trajectories: Vec<_> = positions
            .iter()
            .map(|position| {
                vec![TrajectoryKnot {
                    sample: 0,
                    position: position.map(f64::from),
                }]
            })
            .collect();
        let measurements = MeasurementContext::new(
            1.0,
            salsa_spatial_dsp::CorePanner::new(),
            salsa_spatial::BaseFold::none(),
        )
        .measure_tile(&source, &core, &core, &trajectories, None);
        (measurements, trajectories)
    }

    #[test]
    fn rd_scores_early_anchors_even_when_the_middle_anchor_is_unchanged() {
        let (measurements, trajectories) = sequential_objects();
        let frames = salsa_spatial::TILE_SAMPLES;
        let previous = vec![[0; PARAM_BAND_COUNT]; 2];
        let anchors = build_anchors(&measurements, 0, 0.5, false, EllMode::Measured, &previous);
        assert_eq!(anchors.first().unwrap().sample, 0);
        assert_eq!(anchors.last().unwrap().sample, 4560);
        for anchor in &anchors {
            let hop = anchor.sample as usize / salsa_spatial::HOP_SAMPLES;
            assert_eq!(
                anchor.ell_half_db,
                measured_ell(
                    &measurements.stats.per_hop()[hop].bands,
                    &anchor.active,
                    0.5
                )
            );
        }
        let good = make_tile(0, frames, &previous, &anchors, &[], &trajectories);
        let mut bad = good.clone();
        bad.anchors[0].active[0] = false;
        bad.anchors[1].active[0] = false;
        let context = RenderContext::new(&measurements);
        let good_error = evaluate_distortion(&good, &context).unwrap();
        let bad_error = evaluate_distortion(&bad, &context).unwrap();
        assert!(
            bad_error > good_error + 0.01,
            "early mute was ignored: {bad_error} vs {good_error}"
        );
    }

    #[test]
    fn object_toggle_restores_fitted_powers_and_source_activity() {
        let anchors: Vec<_> = [true, false, true]
            .into_iter()
            .enumerate()
            .map(|(index, active)| ParameterAnchor {
                sample: (index * salsa_spatial::PARAM_INTERVAL_SAMPLES) as u64,
                active: vec![active, true],
                ell_half_db: vec![
                    if index == 2 {
                        std::array::from_fn(|band| -2 * band as i32)
                    } else {
                        [0; PARAM_BAND_COUNT]
                    },
                    [-20; PARAM_BAND_COUNT],
                ],
                error: CoreErrorEnvelope::default(),
            })
            .collect();
        let base = fit_base(&anchors, 2, 0.5);
        let original = make_tile(0, salsa_spatial::TILE_SAMPLES, &base, &anchors, &[], &[]);
        let mut candidate = original.clone();

        toggle_object(&mut candidate, 0, &anchors);
        for (anchor, original) in candidate.anchors.iter().zip(&original.anchors) {
            assert!(!anchor.active[0]);
            assert_eq!(anchor.ell_half_db[0], [0; PARAM_BAND_COUNT]);
            assert_eq!(anchor.active[1], original.active[1]);
            assert_eq!(anchor.ell_half_db[1], original.ell_half_db[1]);
        }

        toggle_object(&mut candidate, 0, &anchors);
        assert_eq!(candidate, original);
    }

    #[test]
    fn reactivation_candidate_recovers_the_current_tile_rd_cost() {
        let (measurements, trajectories) = sequential_objects();
        let previous = vec![[0; PARAM_BAND_COUNT]; 2];
        let anchors = build_anchors(&measurements, 0, 0.5, false, EllMode::Measured, &previous);
        let original = make_tile(
            0,
            salsa_spatial::TILE_SAMPLES,
            &previous,
            &anchors,
            &[],
            &trajectories,
        );
        let context = RenderContext::new(&measurements);
        let encoder = SceneParameterEncoder::new(2);
        let beta = 1e-6;
        let original_eval = evaluate_candidate(&original, &context, &encoder, beta).unwrap();
        let mut muted = original.clone();
        deactivate_object(&mut muted, 0);
        let muted_eval = evaluate_candidate(&muted, &context, &encoder, beta).unwrap();
        let mut restored = muted.clone();
        toggle_object(&mut restored, 0, &anchors);

        let pending =
            distortion_pending(vec![(muted, 0.5, 0), (restored, 0.5, 1)], &context).unwrap();
        let best = finish_candidates(pending, &encoder, beta).unwrap().unwrap();
        assert_eq!(best.tile, original);
        assert_eq!(best.eval.distortion, original_eval.distortion);
        assert_eq!(best.eval.bits, original_eval.bits);
        assert!(best.eval.cost < muted_eval.cost);
    }

    fn band() -> BandStatistics {
        BandStatistics {
            band: 0,
            source_power: vec![0.0; 3],
            source_decoded: vec![[Complex::new(0.0, 0.0); 7]; 3],
            core_power: [0.0; 7],
            decoded_core: [[Complex::new(0.0, 0.0); 7]; 7],
            error_power: [0.0; 7],
            nu: [0.0; 7],
            unobservable: [false; 7],
        }
    }

    #[test]
    fn measured_anchors_are_relative_inactive_and_half_db_quantized() {
        let mut bands: Vec<BandStatistics> = (0..PARAM_BAND_COUNT).map(|_| band()).collect();
        bands[0].source_power = vec![1.0, 0.01, 1.0];
        let active = [true, true, false];
        let ell = measured_ell(&bands, &active, 0.5);
        assert_eq!(ell[0][0], 0);
        assert_eq!(ell[1][0], -40);
        assert_eq!(ell[2][0], 0);
        for band in 1..PARAM_BAND_COUNT {
            assert_eq!(ell[0][band], -180);
            assert_eq!(ell[1][band], -180);
        }
    }

    #[test]
    fn base_fit_averages_active_anchors_and_pays_the_scale_step() {
        let mut anchors = Vec::new();
        for _ in 0..2 {
            let mut ell = vec![[0i32; PARAM_BAND_COUNT]; 2];
            ell[0][0] = -10;
            ell[1][0] = -20;
            anchors.push(ParameterAnchor {
                sample: 0,
                active: vec![true, true],
                ell_half_db: ell,
                error: CoreErrorEnvelope::default(),
            });
        }
        let base = fit_base(&anchors, 2, 0.5);
        assert_eq!(base[0][0], -10);
        assert_eq!(base[1][0], -20);
    }

    #[test]
    fn error_envelope_uses_the_transmitted_grid_and_silence_mask() {
        let mut bands: Vec<BandStatistics> = (0..PARAM_BAND_COUNT).map(|_| band()).collect();
        bands[3].nu[2] = 0.25;
        bands[3].unobservable[5] = true;
        let envelope = envelope_from_bands(&bands);
        assert!((envelope.nu[2][3] - 10f64.powf(-0.6)).abs() < 1e-12);
        assert!(envelope.unobservable[5][3]);
    }
}
