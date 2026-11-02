// SPDX-License-Identifier: MPL-2.0
//! Audio-reconstruction-driven source-to-cluster assignment.

use crate::panning::panning_matrix;
use crate::reconstruction::{solve_gains, source_covariance};
use crate::settings::{AudioSourceWeighting, SILENCE_POWER};

#[allow(clippy::too_many_arguments)]
pub fn audio_reconstruction_gains(
    block: &[f64],
    frames: usize,
    positions: &[[f64; 3]],
    sizes: &[f64],
    active: &[bool],
    selected: &[usize],
    centroids: &[[f64; 3]],
    width_weight: f64,
    spatial_weight: f64,
    iterations: usize,
    anchor_indices: Option<&[usize]>,
    source_weighting: AudioSourceWeighting,
    importance: Option<&[f64]>,
) -> Vec<f64> {
    let n_sources = positions.len();
    let n_clusters = selected.len();
    if n_clusters == 0 {
        return Vec::new();
    }

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
    let audible: Vec<bool> = (0..n_sources)
        .map(|source| active[source] && power[source] > SILENCE_POWER)
        .collect();
    if !audible.iter().any(|value| *value) {
        return vec![0.0; n_sources * n_clusters];
    }

    let source_cov = source_covariance(
        block,
        frames,
        n_sources,
        &audible,
        importance,
        source_weighting,
    );
    let source_pan = panning_matrix(positions);
    let cluster_pan = panning_matrix(centroids);
    let empty: [usize; 0] = [];
    solve_gains(
        &source_cov,
        &source_pan,
        &cluster_pan,
        positions,
        centroids,
        &audible,
        selected,
        anchor_indices.unwrap_or(&empty),
        sizes,
        width_weight,
        spatial_weight,
        iterations,
    )
}
