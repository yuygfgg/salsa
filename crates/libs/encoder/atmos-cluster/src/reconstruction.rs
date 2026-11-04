// SPDX-License-Identifier: MPL-2.0
//! Audio-reconstruction-driven centroid selection and gain solving.

#![allow(clippy::needless_range_loop)]

use rayon::prelude::*;

use crate::clustering::assign_gains;
use crate::matrix;
use crate::panning::{VIRTUAL_SPEAKERS, panning_matrix};
use crate::settings::{AssignMode, AudioSourceWeighting};

pub fn source_covariance(
    block: &[f64],
    frames: usize,
    n_sources: usize,
    active: &[bool],
    importance: Option<&[f64]>,
    weighting: AudioSourceWeighting,
) -> Vec<f64> {
    let mut power = vec![0.0f64; n_sources];
    if frames > 0 {
        for frame in 0..frames {
            let row = &block[frame * n_sources..(frame + 1) * n_sources];
            for (source, value) in row.iter().enumerate() {
                power[source] += value * value;
            }
        }
        for value in &mut power {
            *value /= frames as f64;
        }
    }

    let importance_scale = importance.map_or(0.0, |values| {
        values
            .iter()
            .zip(active)
            .filter(|(_, active)| **active)
            .map(|(value, _)| *value)
            .fold(0.0f64, f64::max)
    });
    let mut weights: Vec<f64> = (0..n_sources)
        .map(|source| {
            if !active[source] {
                return 0.0;
            }
            match weighting {
                AudioSourceWeighting::None => 1.0,
                AudioSourceWeighting::Power => 1.0 / (power[source] + 1e-8),
                AudioSourceWeighting::Importance => {
                    let normalised = importance
                        .filter(|_| importance_scale > 0.0)
                        .map_or(0.0, |values| values[source].max(0.0) / importance_scale);
                    (0.05 + 0.95 * normalised) / (power[source] + 1e-8)
                }
            }
        })
        .collect();

    let active_count = active.iter().filter(|value| **value).count();
    if active_count > 0 {
        let mean_weight: f64 = weights
            .iter()
            .zip(active)
            .filter(|(_, active)| **active)
            .map(|(weight, _)| *weight)
            .sum::<f64>()
            / active_count as f64;
        if mean_weight > 0.0 {
            for value in &mut weights {
                *value /= mean_weight;
            }
        }
    }
    for value in &mut weights {
        *value = value.clamp(0.0, 1e6);
    }

    debug_assert_eq!(block.len(), frames * n_sources);
    let sqrt_weights: Vec<f64> = weights.iter().map(|value| value.sqrt()).collect();
    let threads = rayon::current_num_threads().max(1);
    let rows_per_task = frames.div_ceil(threads).max(1);
    let mut covariance = (0..frames)
        .step_by(rows_per_task)
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|start_frame| {
            let end_frame = (start_frame + rows_per_task).min(frames);
            let mut covariance = vec![0.0f64; n_sources * n_sources];
            let mut scaled = vec![0.0f64; n_sources];
            for frame in start_frame..end_frame {
                let row = &block[frame * n_sources..(frame + 1) * n_sources];
                for (index, (value, weight)) in row.iter().zip(&sqrt_weights).enumerate() {
                    scaled[index] = value * weight;
                }
                for i in 0..n_sources {
                    let left = scaled[i];
                    if left == 0.0 {
                        continue;
                    }
                    for j in 0..n_sources {
                        covariance[i * n_sources + j] += left * scaled[j];
                    }
                }
            }
            covariance
        })
        .reduce(
            || vec![0.0f64; n_sources * n_sources],
            |mut left, right| {
                for (value, other) in left.iter_mut().zip(right) {
                    *value += other;
                }
                left
            },
        );
    let trace: f64 = (0..n_sources)
        .map(|index| covariance[index * n_sources + index])
        .sum();
    if trace > 0.0 {
        for value in &mut covariance {
            *value /= trace;
        }
    }
    for index in 0..n_sources {
        if active[index] {
            covariance[index * n_sources + index] += 1e-9;
        }
    }
    covariance
}

fn project(gains: &mut [f64], free: &[bool], anchors: &[f64], n_sources: usize, n_clusters: usize) {
    for source in 0..n_sources {
        let row = &mut gains[source * n_clusters..(source + 1) * n_clusters];
        if !free[source] {
            row.copy_from_slice(&anchors[source * n_clusters..(source + 1) * n_clusters]);
            continue;
        }
        let largest = row
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(index, _)| index)
            .expect("non-empty gain row");
        for value in row.iter_mut() {
            *value = value.max(0.0);
        }
        let norm = matrix::norm(row);
        if norm > 0.0 {
            for value in row.iter_mut() {
                *value /= norm;
            }
        } else {
            // The nearest nonnegative unit vector for an all-negative row
            // is the axis with the largest original component.
            row[largest] = 1.0;
        }
    }
}

struct GainObjective<'a> {
    covariance: &'a [f64],
    cluster_cov: &'a [f64],
    source_to_cluster: &'a [f64],
    spatial_penalty: &'a [f64],
    n_sources: usize,
    n_clusters: usize,
}

impl GainObjective<'_> {
    fn evaluate(&self, gains: &[f64]) -> (f64, Vec<f64>) {
        let temp = matrix::mat_mul(
            self.covariance,
            self.n_sources,
            self.n_sources,
            gains,
            self.n_clusters,
        );
        let covered = matrix::mat_mul(
            &temp,
            self.n_sources,
            self.n_clusters,
            self.cluster_cov,
            self.n_clusters,
        );
        let mut value = 0.0;
        let mut gradient = vec![0.0; gains.len()];
        for index in 0..gains.len() {
            let quadratic = covered[index] + self.spatial_penalty[index] * gains[index];
            value += (quadratic - 2.0 * self.source_to_cluster[index]) * gains[index];
            gradient[index] = 2.0 * (quadratic - self.source_to_cluster[index]);
        }
        (value, gradient)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn solve_gains(
    covariance: &[f64],
    source_pan: &[f64],
    cluster_pan: &[f64],
    positions: &[[f64; 3]],
    centroids: &[[f64; 3]],
    active: &[bool],
    selected: &[usize],
    fixed_indices: &[usize],
    sizes: &[f64],
    width_weight: f64,
    spatial_weight: f64,
    iterations: usize,
) -> Vec<f64> {
    let n_sources = positions.len();
    let n_clusters = selected.len();
    if n_clusters == 0 {
        return Vec::new();
    }
    let mut gains = assign_gains(
        positions,
        sizes,
        active,
        selected,
        AssignMode::Auto,
        width_weight,
    );

    let mut anchors = vec![0.0f64; n_sources * n_clusters];
    for (cluster, source) in selected.iter().enumerate() {
        if active[*source] && fixed_indices.contains(source) {
            anchors[*source * n_clusters + cluster] = 1.0;
        }
    }
    for source in 0..n_sources {
        if !active[source] {
            gains[source * n_clusters..(source + 1) * n_clusters].fill(0.0);
        }
    }
    for (cluster, source) in selected.iter().enumerate() {
        if active[*source] {
            gains[*source * n_clusters..(*source + 1) * n_clusters].fill(0.0);
            gains[*source * n_clusters + cluster] = 1.0;
        }
    }
    let anchored_rows: Vec<usize> = fixed_indices
        .iter()
        .copied()
        .filter(|index| selected.contains(index))
        .collect();
    for source in &anchored_rows {
        gains[*source * n_clusters..(*source + 1) * n_clusters]
            .copy_from_slice(&anchors[*source * n_clusters..(*source + 1) * n_clusters]);
    }

    let mut free = active.to_vec();
    for source in &anchored_rows {
        free[*source] = false;
    }
    if !free.iter().any(|value| *value) {
        return gains;
    }

    // `source_pan^T * cluster_pan`
    let mut pan_cross = vec![0.0f64; n_sources * n_clusters];
    for source in 0..n_sources {
        for cluster in 0..n_clusters {
            let mut sum = 0.0;
            for speaker in 0..VIRTUAL_SPEAKERS {
                sum += source_pan[speaker * n_sources + source]
                    * cluster_pan[speaker * n_clusters + cluster];
            }
            pan_cross[source * n_clusters + cluster] = sum;
        }
    }
    let source_to_cluster =
        matrix::mat_mul(covariance, n_sources, n_sources, &pan_cross, n_clusters);

    let mut cluster_cov = vec![0.0f64; n_clusters * n_clusters];
    for i in 0..n_clusters {
        for j in 0..n_clusters {
            let mut sum = 0.0;
            for speaker in 0..VIRTUAL_SPEAKERS {
                sum +=
                    cluster_pan[speaker * n_clusters + i] * cluster_pan[speaker * n_clusters + j];
            }
            cluster_cov[i * n_clusters + j] = sum;
        }
    }

    let mut distance = vec![0.0f64; n_sources * n_clusters];
    let mut distance_sum = 0.0;
    let mut distance_count = 0usize;
    for source in 0..n_sources {
        for cluster in 0..n_clusters {
            let dx = positions[source][0] - centroids[cluster][0];
            let dy = positions[source][1] - centroids[cluster][1];
            let dz = positions[source][2] - centroids[cluster][2];
            let value = dx * dx + dy * dy + dz * dz;
            distance[source * n_clusters + cluster] = value;
            if active[source] {
                distance_sum += value;
                distance_count += 1;
            }
        }
    }
    let scale = if distance_count > 0 {
        distance_sum / distance_count as f64
    } else {
        1.0
    }
    .max(1e-6);
    for value in &mut distance {
        *value /= scale;
    }

    let spatial_penalty: Vec<f64> = distance
        .iter()
        .map(|distance| spatial_weight * distance * distance)
        .collect();
    let objective = GainObjective {
        covariance,
        cluster_cov: &cluster_cov,
        source_to_cluster: &source_to_cluster,
        spatial_penalty: &spatial_penalty,
        n_sources,
        n_clusters,
    };

    project(&mut gains, &free, &anchors, n_sources, n_clusters);
    let (mut current, mut gradient) = objective.evaluate(&gains);
    for _ in 0..iterations {
        for source in 0..n_sources {
            if !free[source] {
                for index in source * n_clusters..(source + 1) * n_clusters {
                    gradient[index] = 0.0;
                }
            }
        }

        let mut step = 1.0;
        let mut accepted = false;
        for _ in 0..12 {
            let mut trial: Vec<f64> = gains
                .iter()
                .zip(&gradient)
                .map(|(gain, grad)| gain - step * grad)
                .collect();
            project(&mut trial, &free, &anchors, n_sources, n_clusters);
            let (value, trial_gradient) = objective.evaluate(&trial);
            if value < current - 1e-12 {
                gains = trial;
                current = value;
                gradient = trial_gradient;
                accepted = true;
                break;
            }
            step *= 0.5;
        }
        if !accepted {
            break;
        }
    }
    for source in &anchored_rows {
        gains[*source * n_clusters..(*source + 1) * n_clusters]
            .copy_from_slice(&anchors[*source * n_clusters..(*source + 1) * n_clusters]);
    }
    gains
}

#[allow(clippy::too_many_arguments)]
pub fn select_centroids_reconstruction(
    positions: &[[f64; 3]],
    sizes: &[f64],
    active: &[bool],
    block: &[f64],
    frames: usize,
    importance: &[f64],
    content_prior: &[f64],
    target: usize,
    fixed_indices: &[usize],
    width_weight: f64,
    spatial_weight: f64,
    iterations: usize,
    source_weighting: AudioSourceWeighting,
    prev_selected: &[usize],
    stability: f64,
    min_centroid_distance: f64,
) -> Vec<usize> {
    let n_sources = positions.len();
    if target >= n_sources {
        return (0..n_sources).collect();
    }
    let mut selected: Vec<usize> = Vec::with_capacity(target);
    for &index in fixed_indices {
        if selected.len() >= target {
            break;
        }
        selected.push(index);
    }
    let fixed_set: Vec<bool> = {
        let mut set = vec![false; n_sources];
        for index in fixed_indices {
            set[*index] = true;
        }
        set
    };

    let mut near_fixed = vec![false; n_sources];
    if min_centroid_distance > 0.0 && !fixed_indices.is_empty() {
        for source in 0..n_sources {
            let mut minimum = f64::INFINITY;
            for fixed in fixed_indices {
                let dx = positions[source][0] - positions[*fixed][0];
                let dy = positions[source][1] - positions[*fixed][1];
                let dz = positions[source][2] - positions[*fixed][2];
                minimum = minimum.min((dx * dx + dy * dy + dz * dz).sqrt());
            }
            near_fixed[source] = minimum < min_centroid_distance;
        }
    }

    for &index in prev_selected {
        if fixed_set[index] || near_fixed[index] || selected.contains(&index) {
            continue;
        }
        if selected.len() >= target {
            break;
        }
        selected.push(index);
    }

    let combined_importance: Vec<f64> = (0..n_sources)
        .map(|index| importance[index].max(0.0) * content_prior[index].max(1e-9))
        .collect();
    let covariance = source_covariance(
        block,
        frames,
        n_sources,
        active,
        Some(&combined_importance),
        source_weighting,
    );
    let source_pan = panning_matrix(positions);
    let diagonal: Vec<f64> = (0..n_sources)
        .map(|index| covariance[index * n_sources + index])
        .collect();

    let mut prev_mask = vec![false; n_sources];
    for index in prev_selected {
        prev_mask[*index] = true;
    }
    for index in 0..n_sources {
        prev_mask[index] &= !near_fixed[index];
    }
    let mut candidate_mask = vec![false; n_sources];
    for index in 0..n_sources {
        candidate_mask[index] =
            (active[index] || prev_mask[index]) && !near_fixed[index] && diagonal[index] > 1e-12;
    }
    for index in &selected {
        candidate_mask[*index] = false;
    }

    while selected.len() < target {
        if min_centroid_distance > 0.0 && !selected.is_empty() {
            for source in 0..n_sources {
                let mut minimum = f64::INFINITY;
                for index in &selected {
                    let dx = positions[source][0] - positions[*index][0];
                    let dy = positions[source][1] - positions[*index][1];
                    let dz = positions[source][2] - positions[*index][2];
                    minimum = minimum.min((dx * dx + dy * dy + dz * dz).sqrt());
                }
                if minimum < min_centroid_distance {
                    candidate_mask[source] = false;
                }
            }
        }

        let residual = if !selected.is_empty() {
            let mut cluster_pan = vec![0.0f64; VIRTUAL_SPEAKERS * selected.len()];
            for (cluster, source) in selected.iter().enumerate() {
                for speaker in 0..VIRTUAL_SPEAKERS {
                    cluster_pan[speaker * selected.len() + cluster] =
                        source_pan[speaker * n_sources + source];
                }
            }
            let centroids: Vec<[f64; 3]> = selected.iter().map(|index| positions[*index]).collect();
            let gains = solve_gains(
                &covariance,
                &source_pan,
                &cluster_pan,
                positions,
                &centroids,
                active,
                &selected,
                fixed_indices,
                sizes,
                width_weight,
                spatial_weight,
                iterations,
            );
            let source_term = matrix::mat_mul(
                &source_pan,
                VIRTUAL_SPEAKERS,
                n_sources,
                &covariance,
                n_sources,
            );
            let gains_t = matrix::transpose(&gains, n_sources, selected.len());
            let covariance_term =
                matrix::mat_mul(&gains_t, selected.len(), n_sources, &covariance, n_sources);
            let cluster_term = matrix::mat_mul(
                &cluster_pan,
                VIRTUAL_SPEAKERS,
                selected.len(),
                &covariance_term,
                n_sources,
            );
            let mut residual = vec![0.0f64; VIRTUAL_SPEAKERS * n_sources];
            for index in 0..residual.len() {
                residual[index] = source_term[index] - cluster_term[index];
            }
            residual
        } else {
            matrix::mat_mul(
                &source_pan,
                VIRTUAL_SPEAKERS,
                n_sources,
                &covariance,
                n_sources,
            )
        };

        let mut delta = vec![0.0f64; n_sources];
        for candidate in 0..n_sources {
            let mut sum = 0.0;
            for speaker in 0..VIRTUAL_SPEAKERS {
                sum += residual[speaker * n_sources + candidate]
                    * source_pan[speaker * n_sources + candidate];
            }
            delta[candidate] = 2.0 * sum - diagonal[candidate];
        }
        if stability > 0.0 && prev_mask.iter().zip(&candidate_mask).any(|(p, c)| *p && *c) {
            let values: Vec<f64> = diagonal
                .iter()
                .zip(&candidate_mask)
                .filter(|(_, candidate)| **candidate)
                .map(|(value, _)| *value)
                .collect();
            let floor = if values.is_empty() {
                1.0
            } else {
                0.05 * values.iter().sum::<f64>() / values.len() as f64
            };
            for candidate in 0..n_sources {
                if prev_mask[candidate] && candidate_mask[candidate] {
                    delta[candidate] += stability * diagonal[candidate].max(floor);
                }
            }
        }
        for candidate in 0..n_sources {
            if !candidate_mask[candidate] {
                delta[candidate] = f64::NEG_INFINITY;
            }
        }

        let mut best: Option<usize> = None;
        let mut best_delta = f64::NEG_INFINITY;
        for (candidate, value) in delta.iter().enumerate() {
            if *value > best_delta {
                best_delta = *value;
                best = Some(candidate);
            }
        }
        if best_delta <= 1e-12 {
            best = None;
            let mut best_energy = f64::NEG_INFINITY;
            for candidate in 0..n_sources {
                if !candidate_mask[candidate] {
                    continue;
                }
                let mut energy = 0.0;
                for speaker in 0..VIRTUAL_SPEAKERS {
                    let value = residual[speaker * n_sources + candidate];
                    energy += value * value;
                }
                if energy > best_energy {
                    best_energy = energy;
                    best = Some(candidate);
                }
            }
        }
        let best = if let Some(best) = best {
            best
        } else {
            let pool: Vec<usize> = (0..n_sources)
                .filter(|index| !selected.contains(index) && !near_fixed[*index])
                .collect();
            let pool = if pool.is_empty() {
                (0..n_sources)
                    .filter(|index| !selected.contains(index))
                    .collect()
            } else {
                pool
            };
            if pool.is_empty() {
                break;
            }
            *pool
                .iter()
                .max_by(|a, b| {
                    combined_importance[**a]
                        .partial_cmp(&combined_importance[**b])
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .expect("non-empty pool")
        };
        selected.push(best);
        candidate_mask[best] = false;
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_keeps_active_rows_on_the_unit_sphere() {
        let mut gains = [-0.8, -0.2, 0.0, 0.0, 3.0, 4.0, 0.5, 0.5];
        project(&mut gains, &[true, true, true, false], &[0.0; 8], 4, 2);
        assert_eq!(&gains[..2], &[0.0, 1.0]);
        assert_eq!(&gains[4..6], &[0.6, 0.8]);
        assert_eq!(&gains[6..], &[0.0, 0.0]);
        for row in gains[..6].as_chunks::<2>().0 {
            assert!((matrix::norm(row) - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn inactive_sources_do_not_change_covariance_or_gains() {
        let positions = [[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let selected = [0, 1];
        let active = [true, false, true];
        let source_pan = panning_matrix(&positions);
        let cluster_pan = panning_matrix(&positions[..2]);
        let frames = 128;
        let baseline: Vec<f64> = (0..frames)
            .flat_map(|frame| [1.0, 0.0, if frame % 2 == 0 { 0.5 } else { -0.5 }])
            .collect();
        let mut contaminated = baseline.clone();
        for row in contaminated.as_chunks_mut::<3>().0 {
            row[1] = 4.0 * row[0] + row[2];
        }
        for weighting in [
            AudioSourceWeighting::None,
            AudioSourceWeighting::Power,
            AudioSourceWeighting::Importance,
        ] {
            let covariance = source_covariance(
                &baseline,
                frames,
                3,
                &active,
                Some(&[1.0, 0.0, 0.5]),
                weighting,
            );
            let changed = source_covariance(
                &contaminated,
                frames,
                3,
                &active,
                Some(&[1.0, 1e6, 0.5]),
                weighting,
            );
            assert_eq!(covariance, changed, "{weighting:?}");
            for source in 0..3 {
                assert_eq!(covariance[3 + source], 0.0);
                assert_eq!(covariance[source * 3 + 1], 0.0);
            }
            let solve = |covariance: &[f64]| {
                solve_gains(
                    covariance,
                    &source_pan,
                    &cluster_pan,
                    &positions,
                    &positions[..2],
                    &active,
                    &selected,
                    &[1],
                    &[0.0; 3],
                    0.5,
                    0.0,
                    100,
                )
            };
            let gains = solve(&covariance);
            assert_eq!(gains, solve(&changed));
            assert_eq!(&gains[2..4], &[0.0, 0.0]);
            assert!((gains[0] - 1.0).abs() < 1e-6);
            assert!(gains[1].abs() < 1e-6);
            assert_eq!(
                source_covariance(&contaminated, frames, 3, &[false; 3], None, weighting),
                vec![0.0; 9]
            );
        }
    }

    #[test]
    fn objective_gradient_matches_central_differences() {
        let covariance = [0.6, 0.2, 0.2, 0.4];
        let cluster_cov = [1.0, 0.3, 0.3, 1.0];
        let source_to_cluster = [0.5, 0.1, 0.2, 0.4];
        let distance = [0.2, 1.7, 0.8, 2.3];
        let gains = [0.3, 0.8, 0.6, 0.1];
        for weight in [0.0, 0.02, 0.5] {
            let penalty = distance.map(|value| weight * value * value);
            let objective = GainObjective {
                covariance: &covariance,
                cluster_cov: &cluster_cov,
                source_to_cluster: &source_to_cluster,
                spatial_penalty: &penalty,
                n_sources: 2,
                n_clusters: 2,
            };
            let (_, gradient) = objective.evaluate(&gains);
            for index in 0..gains.len() {
                let mut lower = gains;
                let mut upper = gains;
                lower[index] -= 1e-6;
                upper[index] += 1e-6;
                let numerical =
                    (objective.evaluate(&upper).0 - objective.evaluate(&lower).0) / 2e-6;
                assert!((gradient[index] - numerical).abs() < 1e-8);
            }
        }
    }

    #[test]
    fn spatial_regularization_reaches_the_minimum_on_the_gain_circle() {
        // One free source between two fixed clusters leaves a one-angle problem.
        let positions = [[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.35, 0.7, 0.0]];
        let source_pan = panning_matrix(&positions);
        let cluster_pan = panning_matrix(&positions[..2]);
        let covariance = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let weight = 0.2;
        let gains = solve_gains(
            &covariance,
            &source_pan,
            &cluster_pan,
            &positions,
            &positions[..2],
            &[true; 3],
            &[0, 1],
            &[0, 1],
            &[0.0; 3],
            0.5,
            weight,
            500,
        );
        let distances: Vec<f64> = positions
            .iter()
            .flat_map(|position| {
                positions[..2].iter().map(move |centroid| {
                    position
                        .iter()
                        .zip(centroid)
                        .map(|(a, b)| (a - b).powi(2))
                        .sum::<f64>()
                })
            })
            .collect();
        let scale = distances.iter().sum::<f64>() / distances.len() as f64;
        let objective = |left: f64, right: f64| {
            let residual = (0..VIRTUAL_SPEAKERS)
                .map(|speaker| {
                    let error = cluster_pan[speaker * 2] * left
                        + cluster_pan[speaker * 2 + 1] * right
                        - source_pan[speaker * 3 + 2];
                    error * error
                })
                .sum::<f64>();
            residual
                + weight
                    * ((distances[4] / scale * left).powi(2)
                        + (distances[5] / scale * right).powi(2))
        };
        let minimum = (0..=10_000)
            .map(|step| {
                let angle = std::f64::consts::FRAC_PI_2 * step as f64 / 10_000.0;
                objective(angle.cos(), angle.sin())
            })
            .fold(f64::INFINITY, f64::min);
        assert!((gains[4].hypot(gains[5]) - 1.0).abs() < 1e-12);
        assert!(objective(gains[4], gains[5]) <= minimum + 1e-8);
    }
}
