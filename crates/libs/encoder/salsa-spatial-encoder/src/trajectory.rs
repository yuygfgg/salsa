//! 12-bit position quantization and piecewise-linear trajectory fitting.
//!
//! The quantizer mirrors `salsa-spatial::params`: `x`/`y` use `[-1, 1]` and
//! `z` uses `[0, 1]`, each with 4095 steps.  A fitted trajectory is simplified
//! only while the core panner and validation-layout gain vectors stay within
//! 0.005 L2 of the unsimplified quantized path.

use salsa_spatial::TrajectoryKnot;
use salsa_spatial_dsp::core_gain_column;

pub const POSITION_STEPS: f64 = 4095.0;
/// Largest allowed knot spacing in a moving segment: 100 ms.
pub const MAX_KNOT_GAP: u64 = 4_800;
/// Trajectory validation tolerance from the design document.
pub const VALIDATION_TOLERANCE: f64 = 0.005;

const VALIDATION_LAYOUTS: [&str; 2] = ["7.1.4", "9.1.6"];

pub fn position_limits(axis: usize) -> (f64, f64) {
    if axis < 2 { (-1.0, 1.0) } else { (0.0, 1.0) }
}

pub fn quantize_position(axis: usize, value: f64) -> u16 {
    let (low, high) = position_limits(axis);
    let normalized = ((value - low) / (high - low)).clamp(0.0, 1.0);
    (normalized * POSITION_STEPS).round() as u16
}

pub fn dequantize_position(axis: usize, code: u16) -> f64 {
    let (low, high) = position_limits(axis);
    low + (f64::from(code) / POSITION_STEPS) * (high - low)
}

/// Quantize an ADM position exactly as the parameter syntax does.
pub fn quantize_adm(position: [f32; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| {
        dequantize_position(axis, quantize_position(axis, f64::from(position[axis])))
    })
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn lerp_position(a: [f64; 3], b: [f64; 3], t: f64) -> [f64; 3] {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

/// Position at a sample for a piecewise-linear knot list.  This mirrors the
/// shared panner's private evaluator so core generation and validation use the
/// same convention.
pub fn position_at(knots: &[TrajectoryKnot], sample: i64) -> [f64; 3] {
    if knots.is_empty() {
        return [0.0; 3];
    }
    let sample = sample.max(0) as u64;
    let after = knots.partition_point(|knot| knot.sample <= sample);
    if after == 0 {
        return knots[0].position;
    }
    if after == knots.len() {
        return knots[knots.len() - 1].position;
    }
    let a = knots[after - 1];
    let b = knots[after];
    if sample == a.sample || b.sample <= a.sample {
        return a.position;
    }
    let t = (sample - a.sample) as f64 / (b.sample - a.sample) as f64;
    lerp_position(a.position, b.position, t)
}

fn unit(position: [f32; 3]) -> [f64; 3] {
    let mut vector = [
        f64::from(position[0]),
        f64::from(position[1]),
        f64::from(position[2]).clamp(0.0, 1.0),
    ];
    let norm = (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt();
    if norm <= 1e-12 {
        return [0.0, 1.0, 0.0];
    }
    for value in &mut vector {
        *value /= norm;
    }
    vector
}

fn validation_weights(name: &str, position: [f64; 3]) -> Vec<f64> {
    let layout = objectaudio_ir::STANDARD_LAYOUTS
        .iter()
        .find(|layout| layout.name == name)
        .expect("validation layout exists");
    let direction = unit([position[0] as f32, position[1] as f32, position[2] as f32]);
    let mut weights = vec![0.0f64; layout.channels.len()];
    for (index, speaker) in layout.channels.iter().enumerate() {
        if matches!(
            speaker,
            objectaudio_ir::SpeakerPosition::Lfe | objectaudio_ir::SpeakerPosition::Lfe2
        ) {
            continue;
        }
        let speaker_direction = unit(speaker.adm_position());
        let dot = (direction[0] * speaker_direction[0]
            + direction[1] * speaker_direction[1]
            + direction[2] * speaker_direction[2])
            .max(0.0);
        weights[index] = dot.powi(4);
    }
    let norm = weights
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if norm > 1e-15 {
        for value in &mut weights {
            *value /= norm;
        }
    }
    weights
}

fn l2(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f64>()
        .sqrt()
}

fn segment_valid(a: TrajectoryKnot, b: TrajectoryKnot, dense: &[(u64, [f64; 3])]) -> bool {
    if b.sample <= a.sample {
        return true;
    }
    for &(sample, position) in dense {
        if sample <= a.sample || sample >= b.sample {
            continue;
        }
        let t = (sample - a.sample) as f64 / (b.sample - a.sample) as f64;
        let interpolated = lerp_position(a.position, b.position, t);
        let core_a = core_gain_column(interpolated);
        let core_b = core_gain_column(position);
        let core_error = core_a
            .iter()
            .zip(core_b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f64>()
            .sqrt();
        if core_error > VALIDATION_TOLERANCE {
            return false;
        }
        for name in VALIDATION_LAYOUTS {
            let error = l2(
                &validation_weights(name, interpolated),
                &validation_weights(name, position),
            );
            if error > VALIDATION_TOLERANCE {
                return false;
            }
        }
    }
    true
}

/// Build dense quantized control points for a tile.
pub fn dense_points(
    start: u64,
    frames: usize,
    object_count: usize,
    object: usize,
    positions: &[[f32; 3]],
    control_samples: &[u64],
) -> Vec<(u64, [f64; 3])> {
    if frames == 0 || object_count == 0 {
        return Vec::new();
    }
    let end = start + frames as u64;
    let mut samples = Vec::new();
    let mut sample = start;
    while sample <= end {
        samples.push(sample);
        sample += salsa_spatial::HOP_SAMPLES as u64;
    }
    for &sample in control_samples {
        if (start..=end).contains(&sample) {
            samples.push(sample);
            if sample > start {
                samples.push(sample - 1);
            }
        }
    }
    samples.sort_unstable();
    samples.dedup();
    samples
        .into_iter()
        .map(|sample| {
            let index = ((sample - start) as usize).min(frames - 1);
            let position = quantize_adm(positions[index * object_count + object]);
            (sample, position)
        })
        .collect()
}

/// Fit a tile trajectory.  Returns `(knots, dense, static)`.
///
/// The dense set is retained for diagnostics and for deciding whether a
/// metadata update is needed.  Steps are represented by the dense samples at
/// `event` and `event - 1`, so simplification never moves an authored offset.
pub fn fit_trajectory(
    start: u64,
    frames: usize,
    object_count: usize,
    object: usize,
    positions: &[[f32; 3]],
    control_samples: &[u64],
) -> (Vec<TrajectoryKnot>, Vec<(u64, [f64; 3])>, bool) {
    let dense = dense_points(
        start,
        frames,
        object_count,
        object,
        positions,
        control_samples,
    );
    if dense.is_empty() {
        return (Vec::new(), dense, true);
    }
    let static_path = dense.windows(2).all(|pair| pair[0].1 == pair[1].1);
    if static_path {
        let position = dense[0].1;
        return (
            vec![TrajectoryKnot {
                sample: start,
                position,
            }],
            dense,
            true,
        );
    }

    let mut knots: Vec<TrajectoryKnot> = dense
        .iter()
        .map(|&(sample, position)| TrajectoryKnot { sample, position })
        .collect();

    let mut changed = true;
    while changed {
        changed = false;
        let mut index = 1usize;
        while index + 1 < knots.len() {
            let a = knots[index - 1];
            let b = knots[index + 1];
            if b.sample.saturating_sub(a.sample) > MAX_KNOT_GAP {
                index += 1;
                continue;
            }
            if segment_valid(a, b, &dense) {
                knots.remove(index);
                changed = true;
            } else {
                index += 1;
            }
        }
    }

    (knots, dense, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_positions(count: usize, position: [f32; 3]) -> Vec<[f32; 3]> {
        vec![position; count]
    }

    #[test]
    fn quantization_matches_the_parameter_grid() {
        assert_eq!(quantize_position(0, -1.0), 0);
        assert_eq!(quantize_position(0, 1.0), 4095);
        assert_eq!(quantize_position(2, 0.0), 0);
        assert_eq!(quantize_position(2, 1.0), 4095);
        assert_eq!(dequantize_position(0, 0), -1.0);
        assert!((dequantize_position(0, 4095) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn static_positions_need_one_knot() {
        let positions = flat_positions(4800, [0.25, 0.5, 0.0]);
        let (knots, _, is_static) = fit_trajectory(0, 4800, 1, 0, &positions, &[]);
        assert!(is_static);
        assert_eq!(knots.len(), 1);
        assert_eq!(knots[0].sample, 0);
    }

    #[test]
    fn moving_trajectory_keeps_gaps_below_100_ms() {
        let mut positions = Vec::new();
        for index in 0..4800 {
            let x = -1.0 + 2.0 * index as f32 / 4799.0;
            positions.push([x, 1.0, 0.0]);
        }
        let (knots, _, is_static) = fit_trajectory(0, 4800, 1, 0, &positions, &[]);
        assert!(!is_static);
        assert!(knots.len() >= 2);
        for pair in knots.windows(2) {
            assert!(pair[1].sample - pair[0].sample <= MAX_KNOT_GAP);
        }
    }

    #[test]
    fn authored_step_keeps_its_exact_offset() {
        let mut positions = Vec::new();
        for index in 0..4800 {
            let x = if index < 2400 { -1.0 } else { 1.0 };
            positions.push([x, 1.0, 0.0]);
        }
        let (knots, _, _) = fit_trajectory(0, 4800, 1, 0, &positions, &[2400]);
        let has_step = knots
            .iter()
            .any(|knot| knot.sample == 2399 || knot.sample == 2400);
        assert!(has_step, "step samples missing from {knots:?}");
    }

    #[test]
    fn position_at_holds_the_last_knot() {
        let knots = vec![TrajectoryKnot {
            sample: 10,
            position: [0.0, 1.0, 0.0],
        }];
        assert_eq!(position_at(&knots, 0), [0.0, 1.0, 0.0]);
        assert_eq!(position_at(&knots, 100), [0.0, 1.0, 0.0]);
    }
}
