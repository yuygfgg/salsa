// SPDX-License-Identifier: MPL-2.0
//! Layer-aware panning used by the audio-reconstruction objective.

use crate::matrix;

pub const FLOOR_SPEAKERS: [[f64; 3]; 7] = [
    [-1.0, 1.0, 0.0],  // L
    [1.0, 1.0, 0.0],   // R
    [0.0, 1.0, 0.0],   // C
    [-1.0, 0.0, 0.0],  // Ls
    [1.0, 0.0, 0.0],   // Rs
    [-1.0, -1.0, 0.0], // Lrs
    [1.0, -1.0, 0.0],  // Rrs
];

pub const TOP_SPEAKERS: [[f64; 3]; 4] = [
    [-1.0, 1.0, 1.0],  // Ltf
    [1.0, 1.0, 1.0],   // Rtf
    [-1.0, -1.0, 1.0], // Ltr
    [1.0, -1.0, 1.0],  // Rtr
];

pub const VIRTUAL_SPEAKERS: usize = FLOOR_SPEAKERS.len() + TOP_SPEAKERS.len();

const TOP_BLEND_START: f64 = 0.15;
const TOP_BLEND_END: f64 = 0.65;

fn inverse_distance_gains(point: &[f64; 3], speakers: &[[f64; 3]], nearest: usize) -> Vec<f64> {
    let mut order: Vec<(f64, usize)> = speakers
        .iter()
        .enumerate()
        .map(|(index, speaker)| {
            let dx = speaker[0] - point[0];
            let dy = speaker[1] - point[1];
            let dz = speaker[2] - point[2];
            ((dx * dx + dy * dy + dz * dz).sqrt(), index)
        })
        .collect();
    order.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let count = nearest.min(speakers.len());
    let mut weights = vec![0.0; count];
    for (slot, (distance, _)) in order.iter().take(count).enumerate() {
        let d = *distance + 0.05;
        weights[slot] = 1.0 / (d * d);
    }
    let weight_norm = matrix::norm(&weights).max(1e-30);
    let mut gains = vec![0.0; speakers.len()];
    for (slot, (_, index)) in order.iter().take(count).enumerate() {
        gains[*index] = weights[slot] / weight_norm;
    }
    gains
}

pub fn panning_vector(position: &[f64; 3]) -> Vec<f64> {
    let blend = (position[2] - TOP_BLEND_START) / (TOP_BLEND_END - TOP_BLEND_START);
    let elevation = blend.clamp(0.0, 1.0);

    let mut gains = vec![0.0; VIRTUAL_SPEAKERS];
    if elevation < 1.0 {
        let floor = inverse_distance_gains(position, &FLOOR_SPEAKERS, 4);
        let scale = (1.0 - elevation).sqrt();
        for (index, value) in floor.iter().enumerate() {
            gains[index] = scale * value;
        }
    }
    if elevation > 0.0 {
        let top = inverse_distance_gains(position, &TOP_SPEAKERS, 4);
        let scale = elevation.sqrt();
        for (index, value) in top.iter().enumerate() {
            gains[FLOOR_SPEAKERS.len() + index] = scale * value;
        }
    }

    let norm = matrix::norm(&gains);
    if norm <= 1e-30 {
        return panning_vector(&[position[0], position[1], 0.0]);
    }
    gains.iter().map(|value| value / norm).collect()
}

/// Panning gains, shape `(virtual speakers, positions)`.
pub fn panning_matrix(positions: &[[f64; 3]]) -> Vec<f64> {
    let mut out = vec![0.0; VIRTUAL_SPEAKERS * positions.len()];
    for (column, position) in positions.iter().enumerate() {
        let gains = panning_vector(position);
        for (row, value) in gains.iter().enumerate() {
            out[row * positions.len() + column] = *value;
        }
    }
    out
}
