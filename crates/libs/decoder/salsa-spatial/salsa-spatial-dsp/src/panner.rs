//! Deterministic 5.1.2 core panner.

use std::f64::consts::PI;

use salsa_spatial::TrajectoryKnot;

use salsa_spatial::HOP_SAMPLES;

pub const CORE_CHANNELS: usize = 7;

#[inline]
fn pair(a: f64) -> [f64; 2] {
    let angle = PI * a / 2.0;
    [angle.cos(), angle.sin()]
}

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Evaluate the shared core panner as separate ground and height rows.
///
/// The ground rows are `L, R, C, Ls, Rs`; the height rows are `H_L, H_R`.
pub fn core_gain_parts(position: [f64; 3]) -> ([f64; 5], [f64; 2]) {
    let [x, y, z] = position;
    let u = ((x + 1.0) / 2.0).clamp(0.0, 1.0);
    let v = (1.0 - y).clamp(0.0, 1.0);
    let h = z.clamp(0.0, 1.0);

    let front = if u <= 0.5 {
        let p = pair(2.0 * u);
        [p[0], 0.0, p[1]]
    } else {
        let p = pair(2.0 * u - 1.0);
        [0.0, p[1], p[0]]
    };
    let surround = pair(u);
    let height = pair(u);

    let floor_scale = (PI * v / 2.0).cos();
    let surround_scale = (PI * v / 2.0).sin();
    let floor = [
        floor_scale * front[0],
        floor_scale * front[1],
        floor_scale * front[2],
        surround_scale * surround[0],
        surround_scale * surround[1],
    ];

    let floor_out = (PI * h / 2.0).cos();
    let height_out = (PI * h / 2.0).sin();
    (
        [
            floor_out * floor[0],
            floor_out * floor[1],
            floor_out * floor[2],
            floor_out * floor[3],
            floor_out * floor[4],
        ],
        [height_out * height[0], height_out * height[1]],
    )
}

/// Evaluate the shared core panner column for one ADM position.
///
/// Channel order is `L, R, C, Ls, Rs, H_L, H_R`. The returned column has unit
/// squared norm for every input.
pub fn core_gain_column(position: [f64; 3]) -> [f64; 7] {
    let (ground, height) = core_gain_parts(position);
    [
        ground[0], ground[1], ground[2], ground[3], ground[4], height[0], height[1],
    ]
}

/// Apply the base fold to an interpolated panner column, producing the
/// reconstruction observation column `[D_base; D_aux]`.
pub fn fold_observation_column(column: [f64; 7], fold: salsa_spatial::BaseFold) -> [f64; 7] {
    let ground = [column[0], column[1], column[2], column[3], column[4]];
    let height = [column[5], column[6]];
    let base = fold.apply(ground, height);
    [
        base[0], base[1], base[2], base[3], base[4], height[0], height[1],
    ]
}

/// Stateless helper around [`core_gain_column`] and
/// [`interpolated_core_columns`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CorePanner;

impl CorePanner {
    pub const fn new() -> Self {
        Self
    }

    pub fn gain_column(&self, position: [f64; 3]) -> [f64; 7] {
        core_gain_column(position)
    }

    pub fn columns(
        &self,
        knots: &[TrajectoryKnot],
        start_sample: u64,
        frames: usize,
    ) -> Vec<[f64; 7]> {
        interpolated_core_columns(knots, start_sample, frames)
    }
}

fn position_at(knots: &[TrajectoryKnot], sample: i64) -> [f64; 3] {
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
    [
        lerp(a.position[0], b.position[0], t),
        lerp(a.position[1], b.position[1], t),
        lerp(a.position[2], b.position[2], t),
    ]
}

/// Evaluate the panner on the global 5 ms control grid and interpolate the
/// gains for every output sample.
///
/// A control point is inserted at every authored knot inside the evaluated
/// range. Knots before the range select the constant/linear segment that is
/// active at the start. The returned vector always has exactly `frames`
/// columns and index 0 corresponds to `start_sample`.
pub fn interpolated_core_columns(
    knots: &[TrajectoryKnot],
    start_sample: u64,
    frames: usize,
) -> Vec<[f64; 7]> {
    if frames == 0 {
        return Vec::new();
    }

    let start = start_sample as i64;
    let end = start + frames as i64;
    let hop = HOP_SAMPLES as i64;
    let low = start.div_euclid(hop) * hop;
    let high = (end + hop - 1).div_euclid(hop) * hop;

    let mut grid: Vec<i64> = Vec::new();
    let mut sample = low;
    while sample <= high {
        grid.push(sample);
        sample += hop;
    }
    for knot in knots {
        let knot_sample = knot.sample as i64;
        if knot_sample >= low && knot_sample <= high {
            grid.push(knot_sample);
        }
    }
    grid.sort_unstable();
    grid.dedup();

    let gains: Vec<[f64; 7]> = grid
        .iter()
        .map(|&sample| core_gain_column(position_at(knots, sample)))
        .collect();

    let mut output = Vec::with_capacity(frames);
    let mut segment = 0usize;
    for index in 0..frames {
        let t = start + index as i64;
        while segment + 1 < grid.len() && grid[segment + 1] <= t {
            segment += 1;
        }
        if segment + 1 >= grid.len() {
            output.push(gains[segment]);
            continue;
        }
        let a = grid[segment];
        let b = grid[segment + 1];
        if b <= a {
            output.push(gains[segment]);
            continue;
        }
        let alpha = (t - a) as f64 / (b - a) as f64;
        let mut column = [0.0; 7];
        for channel in 0..7 {
            column[channel] = lerp(gains[segment][channel], gains[segment + 1][channel], alpha);
        }
        output.push(column);
    }
    output
}

/// Interpolate the shared panner and apply the complete-fallback fold.
///
/// The returned seven rows are `[b_FL, b_FR, b_FC, b_SL, b_SR, H_L, H_R]`,
/// where `b = g + A h`.
pub fn interpolated_observation_columns(
    knots: &[TrajectoryKnot],
    start_sample: u64,
    frames: usize,
    fold: salsa_spatial::BaseFold,
) -> Vec<[f64; 7]> {
    interpolated_core_columns(knots, start_sample, frames)
        .into_iter()
        .map(|column| fold_observation_column(column, fold))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(sample: u64, position: [f64; 3]) -> TrajectoryKnot {
        TrajectoryKnot { sample, position }
    }

    fn norm_squared(column: &[f64; 7]) -> f64 {
        column.iter().map(|value| value * value).sum()
    }

    fn assert_close(a: [f64; 7], b: [f64; 7], tolerance: f64) {
        for (x, y) in a.into_iter().zip(b) {
            assert!((x - y).abs() <= tolerance, "{a:?} != {b:?}");
        }
    }

    #[test]
    fn canonical_floor_and_height_routing() {
        assert_close(
            core_gain_column([0.0, 1.0, 0.0]),
            [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            1e-12,
        );
        assert_close(
            core_gain_column([-1.0, 1.0, 0.0]),
            [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            1e-12,
        );
        assert_close(
            core_gain_column([1.0, 1.0, 0.0]),
            [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            1e-12,
        );
        assert_close(
            core_gain_column([-1.0, 0.0, 0.0]),
            [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            1e-12,
        );
        assert_close(
            core_gain_column([1.0, 0.0, 0.0]),
            [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            1e-12,
        );
        assert_close(
            core_gain_column([-1.0, 1.0, 1.0]),
            [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            1e-12,
        );
        assert_close(
            core_gain_column([1.0, 1.0, 1.0]),
            [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            1e-12,
        );
    }

    #[test]
    fn every_column_has_unit_squared_norm() {
        let mut value = 0.125_f64;
        for _ in 0..200 {
            let position = [value.sin(), (value * 1.7).cos(), (value * 2.3).sin()];
            let column = core_gain_column(position);
            assert!((norm_squared(&column) - 1.0).abs() < 1e-12, "{column:?}");
            value = (value * 1.618_033_988_75 + 0.1).fract();
        }
    }

    #[test]
    fn panner_is_continuous() {
        let a = core_gain_column([-0.2, 0.4, 0.1]);
        let b = core_gain_column([-0.2 + 1e-5, 0.4, 0.1]);
        for channel in 0..7 {
            assert!((a[channel] - b[channel]).abs() < 1e-5);
        }
    }

    #[test]
    fn static_trajectory_is_constant() {
        let knots = [position(0, [0.25, 0.75, 0.25])];
        let columns = interpolated_core_columns(&knots, 123, 700);
        assert_eq!(columns.len(), 700);
        let expected = core_gain_column([0.25, 0.75, 0.25]);
        for column in columns {
            assert_close(column, expected, 1e-12);
        }
    }

    #[test]
    fn knot_before_start_selects_the_active_segment() {
        let knots = [
            position(0, [-1.0, 1.0, 0.0]),
            position(100, [1.0, 1.0, 0.0]),
        ];
        let columns = interpolated_core_columns(&knots, 500, 20);
        for column in columns {
            assert_close(column, core_gain_column([1.0, 1.0, 0.0]), 1e-12);
        }
    }

    #[test]
    fn interpolation_is_between_control_grid_columns() {
        let knots = [
            position(0, [-1.0, 1.0, 0.0]),
            position(240, [1.0, 1.0, 0.0]),
        ];
        let columns = interpolated_core_columns(&knots, 0, 241);
        let first = core_gain_column([-1.0, 1.0, 0.0]);
        let last = core_gain_column([1.0, 1.0, 0.0]);
        assert_close(columns[0], first, 1e-12);
        assert_close(columns[240], last, 1e-12);
        let expected_mid: [f64; 7] = std::array::from_fn(|i| 0.5 * (first[i] + last[i]));
        assert_close(columns[120], expected_mid, 1e-12);
    }

    #[test]
    fn authored_steps_become_grid_points() {
        let p0 = [-1.0, 1.0, 0.0];
        let p1 = [-0.5, 1.0, 0.0];
        let p2 = [0.0, 1.0, 0.0];
        let knots = [position(0, p0), position(100, p1), position(240, p2)];
        let columns = interpolated_core_columns(&knots, 0, 241);
        assert_close(columns[100], core_gain_column(p1), 1e-12);
        assert_close(columns[240], core_gain_column(p2), 1e-12);
        let expected = [
            0.5 * (core_gain_column(p0)[0] + core_gain_column(p1)[0]),
            0.5 * (core_gain_column(p0)[1] + core_gain_column(p1)[1]),
            0.5 * (core_gain_column(p0)[2] + core_gain_column(p1)[2]),
            0.5 * (core_gain_column(p0)[3] + core_gain_column(p1)[3]),
            0.5 * (core_gain_column(p0)[4] + core_gain_column(p1)[4]),
            0.5 * (core_gain_column(p0)[5] + core_gain_column(p1)[5]),
            0.5 * (core_gain_column(p0)[6] + core_gain_column(p1)[6]),
        ];
        assert_close(columns[50], expected, 1e-12);
    }

    #[test]
    fn end_of_tile_returns_exact_length() {
        let knots = [
            position(0, [0.0, 1.0, 0.0]),
            position(1000, [1.0, 0.0, 0.5]),
        ];
        let columns = interpolated_core_columns(&knots, 900, 333);
        assert_eq!(columns.len(), 333);
        assert_close(columns[100], core_gain_column([1.0, 0.0, 0.5]), 1e-12);
    }
}
