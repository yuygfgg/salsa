// SPDX-License-Identifier: MPL-2.0
//! Centroid selection, gain assignment and cluster metadata aggregation.

use std::collections::BTreeSet;

use rayon::prelude::*;

use crate::loudness::Loudness;
use crate::matrix;
use crate::settings::AssignMode;
use crate::spatial::width_distance_scale;

#[derive(Debug, Clone)]
pub struct ClusterFrameMetadata {
    pub positions: Vec<[f64; 3]>,
    pub sizes: Vec<f64>,
    pub importance: Vec<f64>,
    pub dialog: Vec<f64>,
    pub music: Vec<f64>,
    pub zones: Vec<String>,
    pub active: Vec<bool>,
}

#[allow(clippy::too_many_arguments)]
pub fn select_centroids(
    loudness: &Loudness,
    excitation: &[f64],
    f_cluster: &[f64],
    active: &[bool],
    content_prior: &[f64],
    n_smooth: &[f64],
    target: usize,
    fixed_indices: &[usize],
    sources: &[usize],
) -> Vec<usize> {
    let n_sources = content_prior.len();
    if target >= n_sources {
        return (0..n_sources).collect();
    }
    let mut selected = Vec::with_capacity(target);
    let mut remaining = excitation.to_vec();
    let bands = remaining.len() / n_sources;

    for &index in fixed_indices {
        if selected.len() >= target {
            break;
        }
        selected.push(index);
        for source in 0..n_sources {
            let weight = f_cluster[source * n_sources + index];
            for band in 0..bands {
                remaining[source * bands + band] *= weight;
            }
        }
    }

    let mut candidates: Vec<usize> = (0..n_sources)
        .filter(|index| active[*index] && !selected.contains(index))
        .collect();
    while selected.len() < target {
        let mut scores = loudness.cluster_scores(&remaining, f_cluster, &candidates, sources);
        for candidate in &candidates {
            scores[*candidate] *= content_prior[*candidate].max(1e-9);
        }
        let mut best = candidates.first().copied().unwrap_or(0);
        let mut best_score = f64::NEG_INFINITY;
        for candidate in &candidates {
            if scores[*candidate] > best_score {
                best_score = scores[*candidate];
                best = *candidate;
            }
        }

        if !best_score.is_finite() || best_score <= 1e-15 {
            let active_candidates: Vec<usize> = candidates.clone();
            let pool = if active_candidates.is_empty() {
                (0..n_sources)
                    .filter(|index| !selected.contains(index))
                    .collect::<Vec<_>>()
            } else {
                active_candidates
            };
            if pool.is_empty() {
                break;
            }
            best = *pool
                .iter()
                .max_by(|a, b| {
                    (n_smooth[**a] * content_prior[**a])
                        .partial_cmp(&(n_smooth[**b] * content_prior[**b]))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .expect("non-empty pool");
        }

        selected.push(best);
        candidates.retain(|candidate| *candidate != best);
        for source in 0..n_sources {
            let weight = f_cluster[source * n_sources + best];
            for band in 0..bands {
                remaining[source * bands + band] *= weight;
            }
        }
    }
    selected
}

fn simplex_weights(
    centroids: &[[f64; 3]],
    point: &[f64; 3],
    max_residual: Option<f64>,
) -> Option<Vec<f64>> {
    let count = centroids.len();
    if count == 1 {
        return Some(vec![1.0]);
    }
    if count == 2 {
        let direction = [
            centroids[1][0] - centroids[0][0],
            centroids[1][1] - centroids[0][1],
            centroids[1][2] - centroids[0][2],
        ];
        let denominator = matrix::dot(&direction, &direction);
        if denominator <= 1e-18 {
            return None;
        }
        let from_start = [
            point[0] - centroids[0][0],
            point[1] - centroids[0][1],
            point[2] - centroids[0][2],
        ];
        let fraction = matrix::dot(&from_start, &direction) / denominator;
        let fraction = fraction.clamp(0.0, 1.0);
        let projected = [
            centroids[0][0] + fraction * direction[0],
            centroids[0][1] + fraction * direction[1],
            centroids[0][2] + fraction * direction[2],
        ];
        let residual = (0..3)
            .map(|index| (projected[index] - point[index]).powi(2))
            .sum::<f64>()
            .sqrt();
        if max_residual.is_some_and(|limit| residual > limit) {
            return None;
        }
        return Some(vec![1.0 - fraction, fraction]);
    }

    let mut a = vec![0.0; 4 * count];
    let mut b = vec![0.0; 4];
    for (index, centroid) in centroids.iter().enumerate() {
        a[index] = centroid[0];
        a[count + index] = centroid[1];
        a[2 * count + index] = centroid[2];
        a[3 * count + index] = 1.0;
    }
    b[0] = point[0];
    b[1] = point[1];
    b[2] = point[2];
    b[3] = 1.0;
    let mut weights = matrix::lstsq(&a, 4, count, &b)?;
    if weights.iter().any(|value| *value < -1e-6) {
        return None;
    }
    let sum: f64 = weights.iter().sum();
    if (sum - 1.0).abs() > 1e-4 {
        return None;
    }
    let mut residual = 0.0;
    for row in 0..4 {
        let mut value = 0.0;
        for column in 0..count {
            value += a[row * count + column] * weights[column];
        }
        residual += (value - b[row]).powi(2);
    }
    let residual = residual.sqrt();
    if max_residual.is_some_and(|limit| residual > limit) {
        return None;
    }
    for value in &mut weights {
        *value = value.max(0.0);
    }
    let sum: f64 = weights.iter().sum::<f64>().max(1e-12);
    for value in &mut weights {
        *value /= sum;
    }
    Some(weights)
}

fn panning_gains(point: &[f64; 3], centroids: &[[f64; 3]], distance: &[f64]) -> Option<Vec<f64>> {
    let mut order: Vec<usize> = (0..centroids.len()).collect();
    order.sort_by(|a, b| {
        distance[*a]
            .partial_cmp(&distance[*b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for count in 2..=4 {
        if centroids.len() < count {
            continue;
        }
        let selected = &order[..count];
        let subset: Vec<[f64; 3]> = selected.iter().map(|index| centroids[*index]).collect();
        if let Some(weights) = simplex_weights(&subset, point, Some(0.15)) {
            let mut gains = vec![0.0; centroids.len()];
            for (slot, index) in selected.iter().enumerate() {
                gains[*index] = weights[slot].sqrt();
            }
            return Some(gains);
        }
    }
    None
}

pub fn assign_gains(
    positions: &[[f64; 3]],
    sizes: &[f64],
    active: &[bool],
    selected: &[usize],
    mode: AssignMode,
    width_weight: f64,
) -> Vec<f64> {
    let n_obj = positions.len();
    let n_cluster = selected.len();
    let mut gains = vec![0.0; n_obj * n_cluster];
    if n_cluster == 0 {
        return gains;
    }
    let centroids: Vec<[f64; 3]> = selected.iter().map(|index| positions[*index]).collect();
    let width = width_distance_scale(sizes, width_weight);
    let mut distance = vec![vec![0.0; n_cluster]; n_obj];
    for (index, position) in positions.iter().enumerate() {
        for (cluster, centroid) in centroids.iter().enumerate() {
            let dx = position[0] - centroid[0];
            let dy = position[1] - centroid[1];
            let dz = position[2] - centroid[2];
            distance[index][cluster] = (dx * dx + dy * dy + dz * dz).sqrt() * width[index];
        }
    }
    let mut selected_index = vec![usize::MAX; n_obj];
    for (cluster, index) in selected.iter().enumerate() {
        selected_index[*index] = cluster;
    }

    gains
        .par_chunks_mut(n_cluster)
        .enumerate()
        .for_each(|(index, row)| {
            if !active[index] {
                return;
            }
            if selected_index[index] != usize::MAX {
                row[selected_index[index]] = 1.0;
                return;
            }

            let mut order: Vec<usize> = (0..n_cluster).collect();
            order.sort_by(|a, b| {
                distance[index][*a]
                    .partial_cmp(&distance[index][*b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            match mode {
                AssignMode::Nearest | AssignMode::Audio => {
                    row[order[0]] = 1.0;
                }
                AssignMode::Pan2 => {
                    let selected = &order[..2.min(n_cluster)];
                    let subset: Vec<[f64; 3]> =
                        selected.iter().map(|cluster| centroids[*cluster]).collect();
                    if let Some(weights) = simplex_weights(&subset, &positions[index], None) {
                        for (slot, cluster) in selected.iter().enumerate() {
                            row[*cluster] = weights[slot].sqrt();
                        }
                    }
                }
                AssignMode::Pan3 => {
                    let selected = &order[..3.min(n_cluster)];
                    let subset: Vec<[f64; 3]> =
                        selected.iter().map(|cluster| centroids[*cluster]).collect();
                    if let Some(weights) = simplex_weights(&subset, &positions[index], Some(0.15)) {
                        for (slot, cluster) in selected.iter().enumerate() {
                            row[*cluster] = weights[slot].sqrt();
                        }
                    }
                }
                AssignMode::Auto => {
                    if let Some(panning) =
                        panning_gains(&positions[index], &centroids, &distance[index])
                    {
                        row.copy_from_slice(&panning);
                    }
                }
            }
            if row.iter().all(|value| *value == 0.0) {
                row[order[0]] = 1.0;
            }
        });
    gains
}

#[allow(clippy::too_many_arguments)]
pub fn cluster_metadata(
    gains: &[f64],
    positions: &[[f64; 3]],
    sizes: &[f64],
    n_smooth: &[f64],
    dialog: &[f64],
    music: &[f64],
    zones: &[String],
    centroids: &[[f64; 3]],
    source_power: &[f64],
    audible: &[bool],
) -> ClusterFrameMetadata {
    let n_obj = positions.len();
    let n_cluster = centroids.len();
    let mut out_positions = vec![[0.0; 3]; n_cluster];
    let mut out_sizes = vec![0.0; n_cluster];
    let mut out_importance = vec![0.0; n_cluster];
    let mut out_dialog = vec![-1.0; n_cluster];
    let mut out_music = vec![-1.0; n_cluster];
    let mut out_active = vec![false; n_cluster];
    let mut out_zones = vec!["all".to_string(); n_cluster];

    for cluster in 0..n_cluster {
        let mut importance_weights = vec![0.0; n_obj];
        let mut position_weights = vec![0.0; n_obj];
        let mut total = 0.0;
        for index in 0..n_obj {
            let gain = gains[index * n_cluster + cluster];
            let gain2 = gain * gain;
            importance_weights[index] = gain2 * n_smooth[index].max(1e-12);
            position_weights[index] = gain2 * source_power[index].max(0.0);
            total += position_weights[index];
        }
        let mut rendered = vec![false; n_obj];
        for index in 0..n_obj {
            rendered[index] = gains[index * n_cluster + cluster] > 0.01 && audible[index];
        }
        out_active[cluster] = rendered.iter().any(|value| *value);
        if total > 1e-15 {
            let mut position = [0.0; 3];
            let mut size = 0.0;
            for index in 0..n_obj {
                position[0] += position_weights[index] * positions[index][0];
                position[1] += position_weights[index] * positions[index][1];
                position[2] += position_weights[index] * positions[index][2];
                size += position_weights[index] * sizes[index];
            }
            out_positions[cluster] = [
                position[0] / total,
                position[1] / total,
                position[2] / total,
            ];
            out_sizes[cluster] = size / total;
        } else {
            out_positions[cluster] = centroids[cluster];
            out_sizes[cluster] = 0.0;
        }
        out_importance[cluster] = (0..n_obj)
            .map(|index| {
                let gain = gains[index * n_cluster + cluster];
                gain * gain * n_smooth[index]
            })
            .sum();

        for (source, destination) in [(dialog, &mut out_dialog), (music, &mut out_music)] {
            let mut weight_sum = 0.0;
            let mut value_sum = 0.0;
            for index in 0..n_obj {
                if rendered[index] && source[index] >= 0.0 {
                    weight_sum += importance_weights[index];
                    value_sum += importance_weights[index] * source[index];
                }
            }
            if weight_sum > 0.0 {
                destination[cluster] = value_sum / weight_sum.max(1e-15);
            }
        }

        let cluster_zones: BTreeSet<&str> = (0..n_obj)
            .filter(|index| rendered[*index])
            .map(|index| zones[index].as_str())
            .collect();
        out_zones[cluster] = if cluster_zones.len() == 1 {
            cluster_zones.iter().next().expect("one zone").to_string()
        } else {
            "all".to_string()
        };
    }

    ClusterFrameMetadata {
        positions: out_positions,
        sizes: out_sizes,
        importance: out_importance,
        dialog: out_dialog,
        music: out_music,
        zones: out_zones,
        active: out_active,
    }
}
