// SPDX-License-Identifier: MPL-2.0
//! Spatial unmasking matrices used by the clustering criterion.

pub fn width_distance_scale(sizes: &[f64], width_weight: f64) -> Vec<f64> {
    sizes
        .iter()
        .map(|size| 1.0 - width_weight * size.clamp(0.0, 1.0))
        .collect()
}

pub fn unmasking_matrix(positions: &[[f64; 3]], fmax: f64, d0: f64) -> Vec<f64> {
    let n = positions.len();
    let mut out = vec![0.0; n * n];
    let scale = d0.max(1e-9);
    for (i, row) in out.chunks_mut(n).enumerate() {
        for j in 0..n {
            if i == j {
                continue;
            }
            let dx = positions[i][0] - positions[j][0];
            let dy = positions[i][1] - positions[j][1];
            let dz = positions[i][2] - positions[j][2];
            let distance = (dx * dx + dy * dy + dz * dz).sqrt();
            row[j] = fmax * (1.0 - (-distance / scale).exp());
        }
    }
    out
}

pub fn clustering_unmasking(
    positions: &[[f64; 3]],
    sizes: &[f64],
    width_weight: f64,
    fmax: f64,
    d0: f64,
) -> Vec<f64> {
    let n = positions.len();
    let width = width_distance_scale(sizes, width_weight);
    let mut out = vec![0.0; n * n];
    let scale = d0.max(1e-9);
    for (i, row) in out.chunks_mut(n).enumerate() {
        for j in 0..n {
            if i == j {
                continue;
            }
            let dx = positions[i][0] - positions[j][0];
            let dy = positions[i][1] - positions[j][1];
            let dz = positions[i][2] - positions[j][2];
            let distance = (dx * dx + dy * dy + dz * dz).sqrt() * width[i];
            row[j] = fmax * (1.0 - (-distance / scale).exp());
        }
    }
    out
}
