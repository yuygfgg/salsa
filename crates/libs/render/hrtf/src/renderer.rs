//! Rosella `.personalized_headphone` binaural renderer.
//!
//! Direct HRTF path, early reflections and the recursive table-A room model,
//! driven by the shared scene timeline. Input is planar object audio
//! (channel 0 = LFE) in 1536-sample frames; output is interleaved stereo.

use std::sync::Arc;

use num_complex::Complex64;
use rustfft::{Fft as RustFft, FftPlanner};

use objectaudio_ir::{SceneTimeline, SpatialPayload};

use crate::error::HrtfError;
use crate::filterbank::{Filterbank, FilterbankKind, HYBRID_BANDS, QMF_HOP, create};
use crate::model::{RosellaDistanceProfile, RosellaModel};

pub const OUTPUT_CHANNELS: usize = 2;
pub const DIRECTION_TERMS: usize = 36;
pub const BLOCK_SAMPLES: usize = 512;
pub const FRAME_SAMPLES: usize = 1536;
const ROOM_BANDS: usize = 64;
const ROOM_INPUT_GAIN: f64 = 0.70710677;
const CENTRE_EQUAL: f64 = 0.9998489618301392;
const CENTRE_ALTERNATE: f64 = 0.7070000171661377;

const SPECIAL_LFE_LOW_BITS: [u32; 16] = [
    0x402695EA, 0x3FE75979, 0x3F28CAAA, 0xBCE1FB2E, 0xBDD8AF65, 0xBD8F426E, 0x3D996821, 0xBC16B3A0,
    0x3B64BAF1, 0xBC81ECFD, 0xBA3D892F, 0x3AF6A9F0, 0xB9DD1C5F, 0x380A193F, 0x38052059, 0x351BCB34,
];

/// Distance profile selected by the `--binaural-mode` equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RosellaProfile {
    Near,
    Far,
    #[default]
    Mid,
}

impl RosellaProfile {
    fn index(self) -> usize {
        match self {
            Self::Near => 1,
            Self::Far => 2,
            Self::Mid => 3,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "near" => Some(Self::Near),
            "far" => Some(Self::Far),
            "mid" => Some(Self::Mid),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Near => "near",
            Self::Far => "far",
            Self::Mid => "mid",
        }
    }
}

/// Renderer options. `filterbank` selects between the public and captured bank.
#[derive(Debug, Clone)]
pub struct RosellaRenderOptions {
    pub profile: RosellaProfile,
    pub filterbank: FilterbankKind,
    /// Delay spatial controls relative to PCM. Source gain stays at audio time.
    pub object_delay_samples: i64,
    pub tail_seconds: f64,
    pub output_gain: f64,
    pub chunk_frames: usize,
    pub room_impulse_slots: usize,
    /// Drop the filterbank's startup latency for offline rendering. The
    /// GStreamer element disables this and reports the latency instead.
    pub skip_latency: bool,
}

impl Default for RosellaRenderOptions {
    fn default() -> Self {
        Self {
            profile: RosellaProfile::Mid,
            filterbank: FilterbankKind::Public,
            object_delay_samples: 0,
            tail_seconds: 5.0,
            output_gain: 1.0,
            chunk_frames: 64,
            room_impulse_slots: 4096,
            skip_latency: true,
        }
    }
}

type Complex = Complex64;

fn direction_basis(x: f64, y: f64, z: f64, out: &mut [f64; DIRECTION_TERMS]) {
    let yz = y * z;
    let x2 = x * x;
    let y2 = y * y;
    let x2m02 = x2 - 0.2;
    let xy = x * y;
    out[0] = 1.0;
    out[1] = x;
    out[2] = y;
    out[3] = z;
    out[4] = x2 - (1.0 / 3.0);
    out[5] = xy;
    out[6] = x * z;
    out[7] = y2 - (1.0 / 3.0);
    out[8] = yz;
    out[9] = (x2 - 0.6) * x;
    out[10] = x2m02 * y;
    out[11] = x2m02 * z;
    out[12] = (y2 - 0.2) * x;
    out[13] = yz * x;
    out[14] = (y2 - 0.6) * y;
    out[15] = (y2 - 0.2) * z;
    out[16] = x2 * x2 - 0.2;
    out[17] = xy * x2;
    out[18] = (x * z) * x2;
    out[19] = y2 * x2 - (1.0 / 15.0);
    out[20] = yz * x2;
    out[21] = x * y2 * y;
    out[22] = x * y2 * z;
    out[23] = y2 * y2 - 0.2;
    let x4 = x2 * x2;
    let x2y2 = y2 * x2;
    let y4 = y2 * y2;
    out[24] = yz * y2;
    out[25] = (x4 - (3.0 / 7.0)) * x;
    out[26] = (x4 - (3.0 / 35.0)) * y;
    out[27] = (x4 - (3.0 / 35.0)) * z;
    out[28] = (x2y2 - (3.0 / 35.0)) * x;
    out[29] = (x2 * z) * xy;
    out[30] = (x2y2 - (3.0 / 35.0)) * y;
    out[31] = (x2y2 - (1.0 / 35.0)) * z;
    out[32] = (y4 - (3.0 / 35.0)) * x;
    out[33] = (y2 * z) * xy;
    out[34] = (y4 - (3.0 / 7.0)) * y;
    out[35] = (y4 - (3.0 / 35.0)) * z;
}

fn logical_field(padded: &[f32]) -> Vec<f64> {
    let mut out = vec![0.0f64; HYBRID_BANDS * DIRECTION_TERMS * 2];
    for band in 0..HYBRID_BANDS {
        let block = band / 4;
        let lane = band % 4;
        for term in 0..DIRECTION_TERMS {
            for component in 0..2 {
                let source = lane + 4 * (term * 2 + component + 72 * block);
                out[(band * DIRECTION_TERMS + term) * 2 + component] = if source < padded.len() {
                    f64::from(padded[source])
                } else {
                    0.0
                };
            }
        }
    }
    out
}

fn q15_position(position: &[f64; 3]) -> [i32; 3] {
    objectaudio_ir::adm_to_q15_f64(*position).map(i32::from)
}

struct ProfileGeometry {
    direction: [f64; 3],
    radius: f64,
    clamped: f64,
    alpha: f64,
}

fn profile_geometry(
    model: &RosellaModel,
    position: &[f64; 3],
    profile_index: usize,
) -> ProfileGeometry {
    let profile = &model.profiles[profile_index];
    let encoded = q15_position(position);
    let q_front = 1.0 - 2.0 * encoded[1] as f64 / 32768.0;
    let q_x = 2.0 * encoded[0] as f64 / 32768.0 - 1.0;
    let q_vertical = encoded[2] as f64 / 32768.0;
    let mut mapped_front = q_front;
    let mut mapped_lateral = -q_x;
    let mut mapped_vertical = q_vertical;
    if model.header_integer_fields[0] != 0 {
        if q_x == 0.0 && q_front == 0.0 {
            mapped_front = 0.0;
            mapped_lateral = 0.0;
            mapped_vertical = q_vertical;
        } else {
            let horizontal_max = q_x.abs().max(q_front.abs());
            let horizontal_norm = (q_x / horizontal_max) * (q_x / horizontal_max)
                + (q_front / horizontal_max) * (q_front / horizontal_max);
            let mut vertical_norm = 1.0;
            if q_vertical != 0.0 {
                let smaller = q_vertical.abs().min(horizontal_max);
                let larger = q_vertical.abs().max(horizontal_max);
                vertical_norm = 1.0 + (smaller / larger) * (smaller / larger);
            }
            let horizontal_factor = 1.0 / (horizontal_norm * vertical_norm).sqrt();
            let vertical_factor = 1.0 / vertical_norm.sqrt();
            mapped_front = q_front * horizontal_factor;
            mapped_lateral = -q_x * horizontal_factor;
            mapped_vertical = q_vertical * vertical_factor;
        }
    }
    let mut scaled = [
        mapped_front * f64::from(profile.axis_scales_internal[2]),
        mapped_lateral * f64::from(profile.axis_scales_internal[0]),
        mapped_vertical * f64::from(profile.axis_scales_internal[1]),
    ];
    let mut ray = 1.0f64;
    for axis in 0..3 {
        let value = scaled[axis];
        let lower = f64::from(profile.bounds[axis * 2]);
        let upper = f64::from(profile.bounds[axis * 2 + 1]);
        if value < lower {
            ray = ray.min(lower / value);
        } else if value > upper {
            ray = ray.min(upper / value);
        }
    }
    if ray < 1.0 {
        for value in &mut scaled {
            *value *= ray;
        }
    }
    let radius = (scaled[0] * scaled[0] + scaled[1] * scaled[1] + scaled[2] * scaled[2]).sqrt();
    let clamped = radius.max(f64::from(profile.minimum_normalized_radius));
    let alpha = radius / clamped;
    let mut direction = [1.0, 0.0, 0.0];
    if radius > 1.0e-30 {
        for axis in 0..3 {
            direction[axis] = scaled[axis] / radius;
        }
    }
    ProfileGeometry {
        direction,
        radius,
        clamped,
        alpha,
    }
}

struct EarGeometry {
    basis_minus: [f64; DIRECTION_TERMS],
    basis_plus: [f64; DIRECTION_TERMS],
    path_minus: f64,
    path_plus: f64,
}

fn ear_geometry(
    model: &RosellaModel,
    profile: &RosellaDistanceProfile,
    direction: &[f64; 3],
    clamped: f64,
    offset: f64,
    correction: f64,
) -> EarGeometry {
    let x = direction[0];
    let y = direction[1];
    let z = direction[2];
    let inverse_distance = f64::from(profile.inverse_distance_per_m);
    let ear = offset * inverse_distance / clamped;
    let y_minus = y - ear;
    let y_plus = y + ear;
    let common = x * x + z * z;
    let length_minus = (y_minus * y_minus + common).sqrt();
    let length_plus = (y_plus * y_plus + common).sqrt();
    let mut result = EarGeometry {
        basis_minus: [0.0; DIRECTION_TERMS],
        basis_plus: [0.0; DIRECTION_TERMS],
        path_minus: 0.0,
        path_plus: 0.0,
    };
    direction_basis(
        x / length_minus,
        y_minus / length_minus,
        z / length_minus,
        &mut result.basis_minus,
    );
    direction_basis(
        x / length_plus,
        y_plus / length_plus,
        z / length_plus,
        &mut result.basis_plus,
    );
    result.path_minus = length_minus * clamped;
    result.path_plus = length_plus * clamped;
    if correction != 0.0 {
        let multiplier = 2.0 * correction * inverse_distance;
        let mut left = 0.0;
        let mut right = 0.0;
        for term in 0..DIRECTION_TERMS {
            left += f64::from(model.vector_left[term]) * result.basis_minus[term];
            right += f64::from(model.vector_right[term]) * result.basis_plus[term];
        }
        result.path_minus += left.max(0.0) * multiplier;
        result.path_plus += right.max(0.0) * multiplier;
    }
    result
}

fn phase_groups(model: &RosellaModel, delay_samples: f64) -> [Complex; HYBRID_BANDS] {
    let mut out = [Complex::new(1.0, 0.0); HYBRID_BANDS];
    let mut current = Complex::new(1.0, 0.0);
    let mut step = Complex::new(1.0, 0.0);
    let mut value_index = 0usize;
    for band in 0..model.hybrid_flags.len().min(HYBRID_BANDS) {
        let flag = model.hybrid_flags[band];
        if flag != 2 {
            if flag == 1 {
                let angle = f64::from(model.hybrid_values[value_index]) * delay_samples;
                value_index += 1;
                step = Complex::new(angle.cos(), angle.sin());
            }
            current *= step;
        }
        out[band] = current;
    }
    out
}

struct DirectResult {
    gains: [[Complex; HYBRID_BANDS]; 2],
    room_send: f64,
}

fn special_lfe_direct() -> DirectResult {
    let mut low = [0.0f64; HYBRID_BANDS];
    for (index, bits) in SPECIAL_LFE_LOW_BITS.iter().enumerate() {
        low[index] = f64::from(f32::from_bits(*bits));
    }
    let mut gains = [[Complex::new(0.0, 0.0); HYBRID_BANDS]; 2];
    for ear in 0..2 {
        for band in 0..HYBRID_BANDS {
            gains[ear][band] = Complex::new(low[band], 0.0);
        }
    }
    DirectResult {
        gains,
        room_send: 0.0,
    }
}

fn direct_and_room_send(
    model: &RosellaModel,
    field_left: &[f64],
    field_right: &[f64],
    position: &[f64; 3],
    profile_index: usize,
) -> DirectResult {
    let profile = &model.profiles[profile_index];
    let geometry = profile_geometry(model, position, profile_index);

    let delay_ears = ear_geometry(
        model,
        profile,
        &geometry.direction,
        geometry.clamped,
        f64::from(model.model_scalars[1]),
        f64::from(model.model_scalars[2]),
    );
    let delay = (delay_ears.path_plus - delay_ears.path_minus).abs()
        * f64::from(profile.distance_scale_m)
        * (model.sample_rate as f64 / 343.3)
        * geometry.alpha;
    let delayed_ear = if delay_ears.path_minus > delay_ears.path_plus {
        0
    } else if delay_ears.path_plus > delay_ears.path_minus {
        1
    } else {
        -1
    };

    let weight_ears = ear_geometry(
        model,
        profile,
        &geometry.direction,
        geometry.clamped,
        f64::from(model.model_scalars[3]),
        f64::from(model.model_scalars[4]),
    );
    let weight_norm = (weight_ears.path_minus * weight_ears.path_minus
        + weight_ears.path_plus * weight_ears.path_plus)
        .sqrt();
    let weight_left = weight_ears.path_plus / weight_norm;
    let weight_right = weight_ears.path_minus / weight_norm;

    let final_offset = f64::from(model.model_scalars[0]);
    let mut basis_minus = [0.0f64; DIRECTION_TERMS];
    let mut basis_plus = [0.0f64; DIRECTION_TERMS];
    if final_offset == 0.0 {
        direction_basis(
            geometry.direction[0],
            geometry.direction[1],
            geometry.direction[2],
            &mut basis_minus,
        );
        basis_plus = basis_minus;
    } else {
        let x = geometry.direction[0];
        let y = geometry.direction[1];
        let z = geometry.direction[2];
        let ear = final_offset * f64::from(profile.inverse_distance_per_m) / geometry.clamped;
        let y_minus = y - ear;
        let y_plus = y + ear;
        let common_length = x * x + z * z;
        let length_minus = (y_minus * y_minus + common_length).sqrt();
        let length_plus = (y_plus * y_plus + common_length).sqrt();
        direction_basis(
            x / length_minus,
            y_minus / length_minus,
            z / length_minus,
            &mut basis_minus,
        );
        direction_basis(
            x / length_plus,
            y_plus / length_plus,
            z / length_plus,
            &mut basis_plus,
        );
    }

    let mut left = [Complex::new(0.0, 0.0); HYBRID_BANDS];
    let mut right = [Complex::new(0.0, 0.0); HYBRID_BANDS];
    for band in 0..HYBRID_BANDS {
        let mut left_real = 0.0;
        let mut left_imag = 0.0;
        let mut right_real = 0.0;
        let mut right_imag = 0.0;
        for term in 0..DIRECTION_TERMS {
            let index = (band * DIRECTION_TERMS + term) * 2;
            left_real += field_left[index] * basis_minus[term];
            left_imag += field_left[index + 1] * basis_minus[term];
            right_real += field_right[index] * basis_plus[term];
            right_imag += field_right[index + 1] * basis_plus[term];
        }
        left[band] = Complex::new(left_real, left_imag);
        right[band] = Complex::new(right_real, right_imag);
    }
    if delayed_ear >= 0 {
        let phase = phase_groups(model, delay);
        let target = if delayed_ear == 0 {
            &mut left
        } else {
            &mut right
        };
        for band in 0..HYBRID_BANDS {
            target[band] *= phase[band];
        }
    }

    let effective_radius = geometry.radius
        * f64::from(model.header_float_scalars[0])
        * f64::from(profile.distance_scale_m);
    let mut common = 1.0;
    let mut room_send = 0.0;
    if profile_index == 2 || profile_index == 3 {
        common = 1.0
            / (1.0
                + f64::from(model.header_float_scalars[1]) * effective_radius * effective_radius)
                .sqrt();
        room_send = effective_radius * common;
    }

    let mut left_term0 = [Complex::new(0.0, 0.0); HYBRID_BANDS];
    let mut right_term0 = [Complex::new(0.0, 0.0); HYBRID_BANDS];
    for band in 0..HYBRID_BANDS {
        let index = band * DIRECTION_TERMS * 2;
        left_term0[band] = Complex::new(field_left[index], field_left[index + 1]);
        right_term0[band] = Complex::new(field_right[index], field_right[index + 1]);
    }
    let weights_are_default_equal = model.model_scalars[3] == 0.0 && model.model_scalars[4] == 0.0;
    let centre_left;
    let centre_right;
    let mut right_direction_weight = weight_right;
    if weights_are_default_equal {
        centre_left = weight_left * (1.0 - geometry.alpha) * CENTRE_EQUAL;
        centre_right = centre_left;
        right_direction_weight = weight_left;
    } else {
        centre_left = (1.0 - geometry.alpha) * CENTRE_ALTERNATE;
        centre_right = centre_left;
    }
    let mut gains = [[Complex::new(0.0, 0.0); HYBRID_BANDS]; 2];
    for band in 0..HYBRID_BANDS {
        gains[0][band] =
            (left[band] * (weight_left * geometry.alpha) + left_term0[band] * centre_left) * common;
        gains[1][band] = (right[band] * (right_direction_weight * geometry.alpha)
            + right_term0[band] * centre_right)
            * common;
    }
    DirectResult { gains, room_send }
}

struct RoomState {
    bands: usize,
    delays: [i32; 4],
    capacity: i32,
    matrix: [f64; 16],
    feedback_real: Vec<f64>,
    feedback_imag: Vec<f64>,
    output_tap: Vec<f64>,
    left_real: Vec<f64>,
    left_imag: Vec<f64>,
    right_real: Vec<f64>,
    right_imag: Vec<f64>,
    allpass_gain: Vec<f64>,
    allpass_delay: Vec<i32>,
    allpass_real: Vec<Vec<f64>>,
    allpass_imag: Vec<Vec<f64>>,
    allpass_position: Vec<i32>,
    memory_real: Vec<f64>,
    memory_imag: Vec<f64>,
    position: i32,
    extra_matrices: Vec<[f64; 16]>,
    extra_indices: Vec<i32>,
    extra_fields: Vec<f32>,
    table_a_scalar: f32,
    table_a_integer: i32,
}

impl RoomState {
    fn new(model: &RosellaModel) -> Self {
        let bands = 64.min(model.table_a_dimension);
        let delays = model.table_a_four_integers;
        let capacity = *delays.iter().max().unwrap();
        let mut matrix = [0.0f64; 16];
        for row in 0..4 {
            for column in 0..4 {
                matrix[row + 4 * column] = f64::from(model.table_a_vector16[row + 4 * column]);
            }
        }
        let f8 = &model.table_a_filter_8x64_padded;
        let f4 = &model.table_a_filter_4x64_padded;
        let f16 = &model.table_a_filter_16x64_padded;
        let mut feedback_real = vec![0.0f64; bands * 4];
        let mut feedback_imag = vec![0.0f64; bands * 4];
        let mut output_tap = vec![0.0f64; bands * 4];
        let mut left_real = vec![0.0f64; bands * 4];
        let mut left_imag = vec![0.0f64; bands * 4];
        let mut right_real = vec![0.0f64; bands * 4];
        let mut right_imag = vec![0.0f64; bands * 4];
        for band in 0..bands {
            let group = band / 4;
            let lane = band % 4;
            for tap in 0..4 {
                let target = band * 4 + tap;
                let g8 = ((group * 4 + tap) * 2) * 4 + lane;
                feedback_real[target] = f64::from(f8[g8]);
                feedback_imag[target] = f64::from(f8[g8 + 4]);
                output_tap[target] = f64::from(f4[(group * 4 + tap) * 4 + lane]);
                let g16 = ((group * 4 + tap) * 4) * 4;
                left_real[target] = f64::from(f16[g16 + lane]);
                left_imag[target] = f64::from(f16[g16 + 4 + lane]);
                right_real[target] = f64::from(f16[g16 + 8 + lane]);
                right_imag[target] = f64::from(f16[g16 + 12 + lane]);
            }
        }
        let allpass_gain: Vec<f64> = model
            .table_a_option_values
            .iter()
            .map(|value| f64::from(*value))
            .collect();
        let allpass_delay = model.table_a_option_ids.clone();
        let mut allpass_real = Vec::with_capacity(allpass_delay.len());
        let mut allpass_imag = Vec::with_capacity(allpass_delay.len());
        for delay in &allpass_delay {
            allpass_real.push(vec![0.0f64; *delay as usize * bands]);
            allpass_imag.push(vec![0.0f64; *delay as usize * bands]);
        }
        let allpass_position = vec![0i32; allpass_real.len()];
        let mut memory_real = vec![0.0f64; capacity as usize * bands * 4];
        let memory_imag = vec![0.0f64; memory_real.len()];
        let mut extra_matrices = Vec::with_capacity(model.table_a_extra_indices.len());
        for index in 0..model.table_a_extra_indices.len() {
            let mut matrix = [0.0f64; 16];
            for row in 0..4 {
                for column in 0..4 {
                    matrix[row + 4 * column] =
                        f64::from(model.table_a_extra_vectors[index * 16 + row + 4 * column]);
                }
            }
            extra_matrices.push(matrix);
        }
        memory_real.shrink_to_fit();
        Self {
            bands,
            delays,
            capacity,
            matrix,
            feedback_real,
            feedback_imag,
            output_tap,
            left_real,
            left_imag,
            right_real,
            right_imag,
            allpass_gain,
            allpass_delay: allpass_delay.clone(),
            allpass_real,
            allpass_imag,
            allpass_position,
            memory_real,
            memory_imag,
            position: 0,
            extra_matrices,
            extra_indices: model.table_a_extra_indices.clone(),
            extra_fields: model.table_a_extra_fields_padded.clone(),
            table_a_scalar: model.table_a_scalar,
            table_a_integer: model.table_a_integer,
        }
    }

    fn process_slot(&mut self, room_send: &[Complex], output: &mut [Complex]) {
        if self.table_a_scalar >= 0.5 {
            output.fill(Complex::new(0.0, 0.0));
            return;
        }
        let bands = self.bands;
        let mut input_real = vec![0.0f64; bands];
        let mut input_imag = vec![0.0f64; bands];
        for band in 0..bands {
            input_real[band] = room_send[band].re * ROOM_INPUT_GAIN;
            input_imag[band] = room_send[band].im * ROOM_INPUT_GAIN;
        }
        for index in 0..self.allpass_gain.len() {
            let delay = self.allpass_delay[index] as usize;
            let position = self.allpass_position[index] as usize;
            let gain = self.allpass_gain[index];
            for band in 0..bands {
                let cell = position * bands + band;
                let previous_real = self.allpass_real[index][cell];
                let previous_imag = self.allpass_imag[index][cell];
                let residual_real = input_real[band] - previous_real * gain;
                let residual_imag = input_imag[band] - previous_imag * gain;
                input_real[band] = residual_real * gain + previous_real;
                input_imag[band] = residual_imag * gain + previous_imag;
                self.allpass_real[index][cell] = residual_real;
                self.allpass_imag[index][cell] = residual_imag;
            }
            self.allpass_position[index] = ((position + 1) % delay) as i32;
        }

        let mut branch_real = vec![0.0f64; bands * 4];
        let mut branch_imag = vec![0.0f64; bands * 4];
        let mut delayed_real = vec![0.0f64; bands * 4];
        let mut delayed_imag = vec![0.0f64; bands * 4];
        for branch in 0..4 {
            let slot =
                ((self.position - self.delays[branch] + self.capacity) % self.capacity) as usize;
            for band in 0..bands {
                let source = (slot * bands + band) * 4 + branch;
                delayed_real[band * 4 + branch] = self.memory_real[source];
                delayed_imag[band * 4 + branch] = self.memory_imag[source];
            }
        }
        for band in 0..bands {
            for row in 0..4 {
                let mut real = input_real[band];
                let mut imag = input_imag[band];
                for column in 0..4 {
                    let coefficient = self.matrix[row + 4 * column];
                    real += delayed_real[band * 4 + column] * coefficient;
                    imag += delayed_imag[band * 4 + column] * coefficient;
                }
                branch_real[band * 4 + row] = real;
                branch_imag[band * 4 + row] = imag;
            }
        }

        let tap_slot =
            ((self.position - self.table_a_integer + self.capacity) % self.capacity) as usize;
        let mut tap_real = vec![0.0f64; bands * 4];
        let mut tap_imag = vec![0.0f64; bands * 4];
        for band in 0..bands {
            for lane in 0..4 {
                let source = (tap_slot * bands + band) * 4 + lane;
                tap_real[band * 4 + lane] = self.memory_real[source];
                tap_imag[band * 4 + lane] = self.memory_imag[source];
            }
        }
        for cell in 0..branch_real.len() {
            let real = branch_real[cell];
            let imag = branch_imag[cell];
            self.memory_real[(self.position as usize * bands) * 4 + cell] =
                real * self.feedback_real[cell] - imag * self.feedback_imag[cell];
            self.memory_imag[(self.position as usize * bands) * 4 + cell] =
                imag * self.feedback_real[cell] + real * self.feedback_imag[cell];
        }
        self.position = (self.position + 1) % self.capacity;

        let mut extra_real = vec![0.0f64; bands * 4];
        let mut extra_imag = vec![0.0f64; bands * 4];
        for index in 0..self.extra_indices.len() {
            let delay = self.extra_indices[index] as usize;
            let slot =
                ((self.position - delay as i32 - 1 + self.capacity * 2) % self.capacity) as usize;
            for band in 0..bands {
                for row in 0..4 {
                    let mut mixed_real = 0.0;
                    let mut mixed_imag = 0.0;
                    for column in 0..4 {
                        let source = (slot * bands + band) * 4 + column;
                        let coefficient = self.extra_matrices[index][row + 4 * column];
                        mixed_real += self.memory_real[source] * coefficient;
                        mixed_imag += self.memory_imag[source] * coefficient;
                    }
                    let group = band / 4;
                    let lane = band % 4;
                    let field = (index * 20 + group) * 2 * 4;
                    let coefficient_real = f64::from(self.extra_fields[field + lane]);
                    let coefficient_imag = f64::from(self.extra_fields[field + 4 + lane]);
                    let target = band * 4 + row;
                    extra_real[target] +=
                        mixed_real * coefficient_real - mixed_imag * coefficient_imag;
                    extra_imag[target] +=
                        mixed_imag * coefficient_real + mixed_real * coefficient_imag;
                }
            }
        }

        for band in 0..bands {
            let mut left_real = 0.0;
            let mut left_imag = 0.0;
            let mut right_real = 0.0;
            let mut right_imag = 0.0;
            for lane in 0..4 {
                let cell = band * 4 + lane;
                let real = tap_real[cell] * self.output_tap[cell] + extra_real[cell];
                let imag = tap_imag[cell] * self.output_tap[cell] + extra_imag[cell];
                left_real += self.left_real[cell] * real - self.left_imag[cell] * imag;
                left_imag += self.left_imag[cell] * real + self.left_real[cell] * imag;
                right_real += self.right_real[cell] * real - self.right_imag[cell] * imag;
                right_imag += self.right_imag[cell] * real + self.right_real[cell] * imag;
            }
            output[band] = Complex::new(left_real, left_imag);
            output[HYBRID_BANDS + band] = Complex::new(right_real, right_imag);
        }
        output[bands..HYBRID_BANDS].fill(Complex::new(0.0, 0.0));
        output[HYBRID_BANDS + bands..].fill(Complex::new(0.0, 0.0));
    }
}

struct RoomFir {
    length: usize,
    kernel: Vec<Complex>,
    tail: Vec<Complex>,
    kernel_fft: Vec<Complex>,
    fft_cache_size: usize,
    line: Vec<Complex>,
    input_fft: Vec<Complex>,
    block: Vec<Complex>,
    forward: Option<Arc<dyn RustFft<f64>>>,
    inverse: Option<Arc<dyn RustFft<f64>>>,
}

impl RoomFir {
    fn new(model: &RosellaModel, impulse_slots: usize) -> Option<Self> {
        let mut reference = RoomState::new(model);
        let mut kernel = vec![Complex::new(0.0, 0.0); impulse_slots * 2 * ROOM_BANDS];
        let mut slot_output = [Complex::new(0.0, 0.0); 2 * HYBRID_BANDS];
        for slot in 0..impulse_slots {
            let mut impulse = [Complex::new(0.0, 0.0); HYBRID_BANDS];
            for band in 0..ROOM_BANDS {
                impulse[band] = Complex::new(if slot == 0 { 1.0 } else { 0.0 }, 0.0);
            }
            reference.process_slot(&impulse, &mut slot_output);
            for ear in 0..2 {
                for band in 0..ROOM_BANDS {
                    kernel[(slot * 2 + ear) * ROOM_BANDS + band] =
                        slot_output[ear * HYBRID_BANDS + band];
                }
            }
        }
        Some(Self {
            length: impulse_slots,
            kernel,
            tail: vec![Complex::new(0.0, 0.0); (impulse_slots - 1) * 2 * ROOM_BANDS],
            kernel_fft: Vec::new(),
            fft_cache_size: 0,
            line: Vec::new(),
            input_fft: Vec::new(),
            block: Vec::new(),
            forward: None,
            inverse: None,
        })
    }

    fn reset(&mut self) {
        self.tail.fill(Complex::new(0.0, 0.0));
    }

    fn process_chunk(&mut self, values: &[Complex], slots: usize) -> Vec<Complex> {
        let mut output = vec![Complex::new(0.0, 0.0); slots * 2 * HYBRID_BANDS];
        if slots == 0 {
            return output;
        }
        let needed = slots + self.length - 1;
        let mut fft_size = 1usize;
        while fft_size < needed {
            fft_size <<= 1;
        }
        if fft_size != self.fft_cache_size {
            self.kernel_fft = vec![Complex::new(0.0, 0.0); fft_size * 2 * ROOM_BANDS];
            let mut planner = FftPlanner::<f64>::new();
            self.forward = Some(planner.plan_fft_forward(fft_size));
            self.inverse = Some(planner.plan_fft_inverse(fft_size));
            self.line = vec![Complex::new(0.0, 0.0); fft_size];
            for ear in 0..2 {
                for band in 0..ROOM_BANDS {
                    for slot in 0..self.length {
                        self.line[slot] = self.kernel[(slot * 2 + ear) * ROOM_BANDS + band];
                    }
                    self.line[self.length..].fill(Complex::new(0.0, 0.0));
                    self.forward.as_ref().unwrap().process(&mut self.line);
                    for index in 0..fft_size {
                        self.kernel_fft[(ear * ROOM_BANDS + band) * fft_size + index] =
                            self.line[index];
                    }
                }
            }
            self.fft_cache_size = fft_size;
        }
        if self.line.len() != fft_size {
            self.line = vec![Complex::new(0.0, 0.0); fft_size];
        }
        let input_fft_size = fft_size * ROOM_BANDS;
        if self.input_fft.len() < input_fft_size {
            self.input_fft
                .resize(input_fft_size, Complex::new(0.0, 0.0));
        }
        for band in 0..ROOM_BANDS {
            for slot in 0..slots {
                self.line[slot] = values[slot * HYBRID_BANDS + band];
            }
            self.line[slots..].fill(Complex::new(0.0, 0.0));
            self.forward.as_ref().unwrap().process(&mut self.line);
            for index in 0..fft_size {
                self.input_fft[band * fft_size + index] = self.line[index];
            }
        }
        let block_size = needed * 2 * ROOM_BANDS;
        if self.block.len() < block_size {
            self.block.resize(block_size, Complex::new(0.0, 0.0));
        }
        for ear in 0..2 {
            for band in 0..ROOM_BANDS {
                for index in 0..fft_size {
                    self.line[index] = self.input_fft[band * fft_size + index]
                        * self.kernel_fft[(ear * ROOM_BANDS + band) * fft_size + index];
                }
                self.inverse.as_ref().unwrap().process(&mut self.line);
                let scale = 1.0 / fft_size as f64;
                for slot in 0..needed {
                    self.block[(slot * 2 + ear) * ROOM_BANDS + band] = self.line[slot] * scale;
                }
            }
        }
        let tail_length = self.length - 1;
        for index in 0..tail_length * 2 * ROOM_BANDS {
            self.block[index] += self.tail[index];
        }
        for slot in 0..slots {
            for ear in 0..2 {
                for band in 0..ROOM_BANDS {
                    output[slot * 2 * HYBRID_BANDS + ear * HYBRID_BANDS + band] =
                        self.block[(slot * 2 + ear) * ROOM_BANDS + band];
                }
            }
        }
        for index in 0..tail_length * 2 * ROOM_BANDS {
            self.tail[index] = self.block[slots * 2 * ROOM_BANDS + index];
        }
        output
    }
}

#[derive(Clone)]
struct Key {
    special_lfe: bool,
    profile: usize,
    position: [f64; 3],
}

impl Key {
    fn equals(&self, other: &Key) -> bool {
        self.special_lfe == other.special_lfe
            && self.profile == other.profile
            && self.position == other.position
    }
}

struct Core {
    model: RosellaModel,
    field_left: Vec<f64>,
    field_right: Vec<f64>,
    room: RoomFir,
    source_channels: usize,
    gains: Vec<Complex>,
    room_sends: Vec<f64>,
    keys: Vec<Option<Key>>,
}

impl Core {
    fn configure(
        model: RosellaModel,
        room_impulse_slots: usize,
        source_channels: usize,
    ) -> Result<Self, HrtfError> {
        let field_left = logical_field(&model.field_left_padded);
        let field_right = logical_field(&model.field_right_padded);
        let room = RoomFir::new(&model, room_impulse_slots)
            .ok_or_else(|| HrtfError::Unsupported("alternate Rosella table-A room mode".into()))?;
        let mut core = Self {
            model,
            field_left,
            field_right,
            room,
            source_channels,
            gains: vec![Complex::new(0.0, 0.0); source_channels * 2 * HYBRID_BANDS],
            room_sends: vec![0.0; source_channels],
            keys: vec![None; source_channels],
        };
        for source in 0..source_channels {
            let position = [0.0, 1.0, 0.0];
            core.set_source(source, &position, 3, false)?;
        }
        Ok(core)
    }

    fn reset(&mut self) {
        self.room.reset();
    }

    fn set_source(
        &mut self,
        source: usize,
        position: &[f64; 3],
        profile: usize,
        special_lfe: bool,
    ) -> Result<(), HrtfError> {
        let effective_profile = if special_lfe { 0 } else { profile };
        let key = Key {
            special_lfe,
            profile: effective_profile,
            position: *position,
        };
        if let Some(existing) = &self.keys[source]
            && existing.equals(&key)
        {
            return Ok(());
        }
        let result = if special_lfe {
            special_lfe_direct()
        } else {
            direct_and_room_send(
                &self.model,
                &self.field_left,
                &self.field_right,
                position,
                effective_profile,
            )
        };
        for ear in 0..2 {
            for band in 0..HYBRID_BANDS {
                self.gains[(source * 2 + ear) * HYBRID_BANDS + band] = result.gains[ear][band];
            }
        }
        self.room_sends[source] = result.room_send;
        self.keys[source] = Some(key);
        Ok(())
    }

    fn direct_and_send_static(
        &self,
        hybrid: &[Complex],
        slots: usize,
        direct: &mut Vec<Complex>,
        room_send: &mut Vec<Complex>,
    ) {
        direct.clear();
        direct.resize(slots * 2 * HYBRID_BANDS, Complex::new(0.0, 0.0));
        room_send.clear();
        room_send.resize(slots * HYBRID_BANDS, Complex::new(0.0, 0.0));
        for source in (0..self.source_channels).rev() {
            let base = source * HYBRID_BANDS;
            for slot in 0..slots {
                let input = slot * self.source_channels * HYBRID_BANDS + base;
                let room_target = slot * HYBRID_BANDS;
                for band in 0..HYBRID_BANDS {
                    let value = hybrid[input + band];
                    for ear in 0..2 {
                        direct[slot * 2 * HYBRID_BANDS + ear * HYBRID_BANDS + band] +=
                            value * self.gains[(source * 2 + ear) * HYBRID_BANDS + band];
                    }
                    room_send[room_target + band] += value * self.room_sends[source];
                }
            }
        }
    }
}

/// Rosella binaural renderer.
pub struct RosellaRenderer {
    options: RosellaRenderOptions,
    source_channels: usize,
    block_positions: Vec<[f64; 3]>,
    core: Core,
    analysis_bank: Box<dyn Filterbank>,
    synthesis_bank: Box<dyn Filterbank>,
    scene: Option<SceneTimeline>,
    buffer: Vec<f64>,
    buffer_used: usize,
    output: Vec<f64>,
    input_samples: i64,
    processed_input_samples: i64,
    raw_output_samples: i64,
    metadata_updates: u64,
    open: bool,
    finished: bool,
    hops: Vec<f64>,
    qmf: Vec<Complex>,
    hybrid: Vec<Complex>,
    rendered: Vec<Complex>,
    room_sends_buffer: Vec<Complex>,
    room_out: Vec<Complex>,
    qmf_out: Vec<Complex>,
    bands: Vec<f64>,
}

impl RosellaRenderer {
    /// Create and configure a renderer for `model`.
    ///
    /// `source_channels` is `object_count + 1`; channel 0 is the LFE. It is
    /// derived from the negotiated caps and is not a fixed upper bound.
    pub fn new(
        model: RosellaModel,
        options: RosellaRenderOptions,
        source_channels: usize,
    ) -> Result<Self, HrtfError> {
        if source_channels == 0 {
            return Err(HrtfError::InvalidArgument(
                "source_channels must be positive".into(),
            ));
        }
        if options.chunk_frames == 0 {
            return Err(HrtfError::InvalidArgument(
                "chunk_frames must be positive".into(),
            ));
        }
        if options.room_impulse_slots == 0 {
            return Err(HrtfError::InvalidArgument(
                "room_impulse_slots must be positive".into(),
            ));
        }
        if options.tail_seconds.is_nan() || options.tail_seconds < 0.0 {
            return Err(HrtfError::InvalidArgument(
                "tail_seconds must be non-negative".into(),
            ));
        }
        if !options.output_gain.is_finite() {
            return Err(HrtfError::InvalidArgument(
                "output_gain must be finite".into(),
            ));
        }
        if options.object_delay_samples < 0 {
            return Err(HrtfError::InvalidArgument(
                "object_delay_samples must be non-negative".into(),
            ));
        }
        let core = Core::configure(model, options.room_impulse_slots, source_channels)?;
        let analysis_bank = create(options.filterbank, source_channels);
        let synthesis_bank = create(options.filterbank, OUTPUT_CHANNELS);
        let chunk_samples = options.chunk_frames * FRAME_SAMPLES;
        let mut renderer = Self {
            buffer: vec![0.0f64; chunk_samples * source_channels],
            source_channels,
            block_positions: Vec::new(),
            analysis_bank,
            synthesis_bank,
            core,
            scene: None,
            buffer_used: 0,
            output: Vec::new(),
            input_samples: 0,
            processed_input_samples: 0,
            raw_output_samples: 0,
            metadata_updates: 0,
            open: true,
            finished: false,
            options,
            hops: Vec::new(),
            qmf: Vec::new(),
            hybrid: Vec::new(),
            rendered: Vec::new(),
            room_sends_buffer: Vec::new(),
            room_out: Vec::new(),
            qmf_out: Vec::new(),
            bands: Vec::new(),
        };
        renderer.room_sends_buffer =
            vec![Complex::new(0.0, 0.0); (renderer.chunk_samples() / QMF_HOP) * HYBRID_BANDS];
        Ok(renderer)
    }

    pub fn options(&self) -> &RosellaRenderOptions {
        &self.options
    }

    pub fn model(&self) -> &RosellaModel {
        &self.core.model
    }

    pub fn summary(&self) -> String {
        self.core.model.summary()
    }

    pub fn latency_samples(&self) -> usize {
        self.options.filterbank.latency()
    }

    pub fn source_channels(&self) -> usize {
        self.source_channels
    }

    pub fn object_count(&self) -> usize {
        self.source_channels - 1
    }

    fn chunk_samples(&self) -> usize {
        self.options.chunk_frames * FRAME_SAMPLES
    }

    /// Submit metadata at the absolute start sample of its PCM buffer.
    pub fn submit_metadata(
        &mut self,
        payload: &SpatialPayload,
        buffer_start: i64,
    ) -> Result<(), HrtfError> {
        payload
            .validate_layout(48_000, self.source_channels)
            .map_err(|error| HrtfError::Format(error.to_string()))?;
        match &mut self.scene {
            Some(scene) => scene.submit(payload, buffer_start),
            None => SceneTimeline::from_payload(payload, buffer_start)
                .map(|scene| self.scene = Some(scene)),
        }
        .map_err(|error| HrtfError::Format(error.to_string()))?;
        self.metadata_updates += 1;
        Ok(())
    }

    /// Submit one 1536-sample planar-objects frame with `source_channels`
    /// channels. `objects_planar` is channel-major (`channel * 1536 + sample`).
    pub fn submit_frame(
        &mut self,
        objects_planar: &[f32],
        update: Option<&SpatialPayload>,
    ) -> Result<(), HrtfError> {
        if !self.open {
            return Err(HrtfError::State("renderer is not open".into()));
        }
        if self.finished {
            return Err(HrtfError::State("renderer is already finished".into()));
        }
        if objects_planar.len() < self.source_channels * FRAME_SAMPLES {
            return Err(HrtfError::InvalidArgument(format!(
                "object frame must contain {} * {FRAME_SAMPLES} samples",
                self.source_channels
            )));
        }
        if let Some(update) = update {
            self.submit_metadata(update, self.input_samples)?;
        }
        let chunk = self.chunk_samples();
        let mut offset = 0usize;
        while offset < FRAME_SAMPLES {
            let room = chunk - self.buffer_used;
            let count = room.min(FRAME_SAMPLES - offset);
            for sample in 0..count {
                let row = (self.buffer_used + sample) * self.source_channels;
                for channel in 0..self.source_channels {
                    self.buffer[row + channel] =
                        f64::from(objects_planar[channel * FRAME_SAMPLES + offset + sample])
                            * if channel == 0 {
                                1.0
                            } else {
                                self.scene.as_ref().map_or(1.0, |scene| {
                                    f64::from(
                                        scene
                                            .state_at(
                                                channel - 1,
                                                self.input_samples + (offset + sample) as i64,
                                            )
                                            .effective_gain(),
                                    )
                                })
                            };
                }
            }
            self.buffer_used += count;
            offset += count;
            if self.buffer_used == chunk {
                let buffer = std::mem::take(&mut self.buffer);
                let result = self.process_samples(&buffer, chunk);
                self.buffer = buffer;
                result?;
                self.buffer_used = 0;
            }
        }
        self.input_samples += FRAME_SAMPLES as i64;
        Ok(())
    }

    fn process_samples(&mut self, values: &[f64], count: usize) -> Result<(), HrtfError> {
        if !count.is_multiple_of(BLOCK_SAMPLES) {
            return Err(HrtfError::BlockAlignment(BLOCK_SAMPLES));
        }
        let blocks = count / BLOCK_SAMPLES;
        let slots = blocks * (BLOCK_SAMPLES / QMF_HOP);
        let hops_size = slots * self.source_channels * QMF_HOP;
        if self.hops.len() < hops_size {
            self.hops.resize(hops_size, 0.0);
        }
        for block in 0..blocks {
            for hop in 0..BLOCK_SAMPLES / QMF_HOP {
                for channel in 0..self.source_channels {
                    for index in 0..QMF_HOP {
                        self.hops[((block * (BLOCK_SAMPLES / QMF_HOP) + hop)
                            * self.source_channels
                            + channel)
                            * QMF_HOP
                            + index] = values[(block * BLOCK_SAMPLES + hop * QMF_HOP + index)
                            * self.source_channels
                            + channel];
                    }
                }
            }
        }
        self.analysis_bank
            .analyze_qmf(&self.hops, slots, &mut self.qmf);
        self.analysis_bank
            .analyze_hybrid(&self.qmf, slots, &mut self.hybrid);

        self.rendered
            .resize(slots * 2 * HYBRID_BANDS, Complex::new(0.0, 0.0));
        if self.room_sends_buffer.len() != slots * HYBRID_BANDS {
            self.room_sends_buffer
                .resize(slots * HYBRID_BANDS, Complex::new(0.0, 0.0));
        }
        for block in 0..blocks {
            let first_slot = block * (BLOCK_SAMPLES / QMF_HOP);
            let block_slots = BLOCK_SAMPLES / QMF_HOP;
            let sample = self.processed_input_samples + (block * BLOCK_SAMPLES) as i64;
            self.set_block_parameters(sample)?;
            let start = first_slot * self.source_channels * HYBRID_BANDS;
            let end = (first_slot + block_slots) * self.source_channels * HYBRID_BANDS;
            let block_hybrid = &self.hybrid[start..end];
            let (mut direct, mut room_send) = (Vec::new(), Vec::new());
            self.core.direct_and_send_static(
                block_hybrid,
                block_slots,
                &mut direct,
                &mut room_send,
            );
            self.rendered
                [first_slot * 2 * HYBRID_BANDS..(first_slot + block_slots) * 2 * HYBRID_BANDS]
                .copy_from_slice(&direct);
            self.room_sends_buffer
                [first_slot * HYBRID_BANDS..(first_slot + block_slots) * HYBRID_BANDS]
                .copy_from_slice(&room_send);
        }
        self.room_out = self.core.room.process_chunk(&self.room_sends_buffer, slots);
        for (value, add) in self.rendered.iter_mut().zip(&self.room_out) {
            *value += add;
        }
        self.synthesis_bank
            .synthesize_hybrid(&self.rendered, slots, &mut self.qmf_out);
        self.synthesis_bank
            .synthesize_qmf(&self.qmf_out, slots, &mut self.bands);
        let produced = blocks * BLOCK_SAMPLES;
        let mut time = vec![0.0f64; produced * OUTPUT_CHANNELS];
        for slot in 0..slots {
            for channel in 0..OUTPUT_CHANNELS {
                for band in 0..QMF_HOP {
                    time[(slot * QMF_HOP + band) * OUTPUT_CHANNELS + channel] =
                        self.bands[(slot * OUTPUT_CHANNELS + channel) * QMF_HOP + band];
                }
            }
        }
        for sample in 0..produced {
            for channel in 0..OUTPUT_CHANNELS {
                time[sample * OUTPUT_CHANNELS + channel] *= self.options.output_gain;
            }
        }
        self.append_output(&time, produced);
        self.processed_input_samples += count as i64;
        if let Some(scene) = &mut self.scene {
            scene.discard_before(
                (self.processed_input_samples - self.options.object_delay_samples)
                    .min(self.input_samples),
            );
        }
        Ok(())
    }

    fn set_block_parameters(&mut self, sample: i64) -> Result<(), HrtfError> {
        let profile = self.options.profile.index();
        let lfe = [0.0, 1.0, 0.0];
        self.core.set_source(0, &lfe, 3, true)?;
        self.block_positions.clear();
        for object in 0..self.object_count() {
            self.block_positions
                .push(self.scene.as_ref().map_or([0.0; 3], |scene| {
                    scene
                        .state_at(object, sample - self.options.object_delay_samples)
                        .position
                        .map(f64::from)
                }));
        }
        for object in 0..self.block_positions.len() {
            self.core
                .set_source(object + 1, &self.block_positions[object], profile, false)?;
        }
        Ok(())
    }

    fn append_output(&mut self, stereo: &[f64], count: usize) {
        let skip = if self.options.skip_latency {
            let remaining = self.latency_samples() as i64 - self.raw_output_samples;
            remaining.clamp(0, count as i64) as usize
        } else {
            0
        };
        self.raw_output_samples += count as i64;
        for sample in skip..count {
            self.output.push(stereo[sample * OUTPUT_CHANNELS]);
            self.output.push(stereo[sample * OUTPUT_CHANNELS + 1]);
        }
    }

    /// Drain the flush tail. Returns only samples after the program.
    pub fn finish(&mut self, flush_samples: usize) -> Result<Vec<f64>, HrtfError> {
        if !self.open {
            return Err(HrtfError::State("renderer is not open".into()));
        }
        if self.finished {
            return Ok(Vec::new());
        }
        let program_end = self.output.len();
        if self.buffer_used != 0 {
            let count = self.buffer_used;
            let buffer = std::mem::take(&mut self.buffer);
            let result = self.process_samples(&buffer, count);
            self.buffer = buffer;
            result?;
            self.buffer_used = 0;
        }
        let chunk = self.chunk_samples();
        let mut remaining = flush_samples;
        while remaining > 0 {
            let count = remaining.min(chunk);
            let silence = vec![0.0f64; count * self.source_channels];
            self.process_samples(&silence, count)?;
            remaining -= count;
        }
        self.finished = true;
        Ok(self.output[program_end..].to_vec())
    }

    pub fn finish_capacity(&self, tail_seconds: f64) -> usize {
        let samples =
            tail_seconds * 48_000.0 + self.latency_samples() as f64 + BLOCK_SAMPLES as f64;
        let blocks = (samples / BLOCK_SAMPLES as f64).ceil();
        blocks as usize * BLOCK_SAMPLES
    }

    pub fn reset(&mut self) -> Result<(), HrtfError> {
        if !self.open {
            return Err(HrtfError::State("renderer is not open".into()));
        }
        self.core.reset();
        self.analysis_bank.reset();
        self.synthesis_bank.reset();
        self.buffer_used = 0;
        self.output.clear();
        self.input_samples = 0;
        self.processed_input_samples = 0;
        self.raw_output_samples = 0;
        self.metadata_updates = 0;
        self.finished = false;
        self.scene = None;
        Ok(())
    }

    pub fn output(&self) -> &[f64] {
        &self.output
    }

    pub fn take_output(&mut self) -> Vec<f64> {
        std::mem::take(&mut self.output)
    }

    pub fn input_samples(&self) -> i64 {
        self.input_samples
    }

    pub fn processed_input_samples(&self) -> i64 {
        self.processed_input_samples
    }

    pub fn metadata_updates(&self) -> u64 {
        self.metadata_updates
    }
}
