// SPDX-License-Identifier: MPL-2.0
//! Audio-reconstruction-driven source-to-cluster assignment.

use crate::panning::panning_matrix;
use crate::reconstruction::{GainSolveRequest, solve_gains, source_covariance};
use crate::settings::{AudioSourceWeighting, SILENCE_POWER};

/// Inputs for [`audio_reconstruction_gains`].
pub struct AudioAssignRequest<'a> {
    pub block: &'a [f64],
    pub frames: usize,
    pub positions: &'a [[f64; 3]],
    pub sizes: &'a [f64],
    pub active: &'a [bool],
    pub selected: &'a [usize],
    pub centroids: &'a [[f64; 3]],
    pub width_weight: f64,
    pub spatial_weight: f64,
    pub iterations: usize,
    pub anchor_indices: Option<&'a [usize]>,
    pub source_weighting: AudioSourceWeighting,
    pub importance: Option<&'a [f64]>,
}

pub fn audio_reconstruction_gains(request: AudioAssignRequest<'_>) -> Vec<f64> {
    let AudioAssignRequest {
        block,
        frames,
        positions,
        sizes,
        active,
        selected,
        centroids,
        width_weight,
        spatial_weight,
        iterations,
        anchor_indices,
        source_weighting,
        importance,
    } = request;
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
    solve_gains(GainSolveRequest {
        covariance: &source_cov,
        source_pan: &source_pan,
        cluster_pan: &cluster_pan,
        positions,
        centroids,
        active: &audible,
        selected,
        fixed_indices: anchor_indices.unwrap_or(&empty),
        sizes,
        width_weight,
        spatial_weight,
        iterations,
    })
}
