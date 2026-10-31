//! Parser for Dolby `.personalized_headphone` (Rosella) models.
//!
//! The file is JSON whose `virtualizer_parameters` carry the raw `rp` coefficient
//! lanes. Everything the renderer needs is unpacked here, in the same float32
//! arithmetic the reference uses, because those values are part of the model.

use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::HrtfError;

const Q15: f32 = 1.0 / 32768.0;

#[derive(Debug, Clone, Default)]
pub struct RosellaDistanceProfile {
    pub bounds: [f32; 6],
    pub distance_scale_m: f32,
    pub inverse_distance_per_m: f32,
    pub axis_scales_internal: [f32; 3],
    pub minimum_normalized_radius: f32,
}

#[derive(Debug, Clone, Default)]
pub struct RosellaCaptureMetadata {
    pub capture_submission_date: String,
    pub capture_type: String,
    pub label: String,
    pub name: String,
    pub algorithm_version: String,
    pub creation_date: String,
    pub uuid: String,
    pub version: String,
}

#[derive(Debug, Clone, Default)]
pub struct RosellaModel {
    pub source_path: String,
    pub coefficient_sha256: String,
    pub coefficient_version: String,
    pub room_model: String,
    pub capture: RosellaCaptureMetadata,

    pub table_a_dimension: usize,
    pub table_a_option: usize,
    pub table_a_extra: usize,
    pub table_a_control: i32,
    pub table_a_option_ids: Vec<i32>,
    pub table_a_option_values: Vec<f32>,
    pub table_a_scalar: f32,
    pub table_a_filter_16x64_padded: Vec<f32>,
    pub table_a_four_integers: [i32; 4],
    pub table_a_integer: i32,
    pub table_a_filter_8x64_padded: Vec<f32>,
    pub table_a_vector16: Vec<f32>,
    pub table_a_filter_4x64_padded: Vec<f32>,
    pub table_a_extra_indices: Vec<i32>,
    pub table_a_extra_fields_padded: Vec<f32>,
    pub table_a_extra_vectors: Vec<f32>,

    pub sample_rate: usize,
    pub matrix_exponent: i32,
    pub field_exponent: i32,
    pub matrix_left: Vec<f32>,
    pub matrix_right: Vec<f32>,
    pub vector_left: Vec<f32>,
    pub vector_right: Vec<f32>,
    pub field_left_padded: Vec<f32>,
    pub field_right_padded: Vec<f32>,
    pub field_left_odd_serialized_zero: bool,
    pub hybrid_flags: Vec<i32>,
    pub hybrid_values: Vec<f32>,
    pub model_scalars: Vec<f32>,
    pub header_float_scalars: [f32; 2],
    pub header_integer_fields: [i32; 2],
    pub profiles: [RosellaDistanceProfile; 4],
    pub profile_tail: Vec<f32>,
    pub post_fields: [i32; 3],
}

impl RosellaModel {
    pub fn summary(&self) -> String {
        let name = if self.capture.name.is_empty() {
            "unnamed"
        } else {
            &self.capture.name
        };
        let room = if self.room_model.is_empty() {
            "unknown room"
        } else {
            &self.room_model
        };
        format!(
            "Rosella personalized_headphone '{name}' ({room}), {} HQMF / 77 hybrid @ {} Hz",
            self.table_a_dimension, self.sample_rate
        )
    }
}

fn fail(message: impl Into<String>) -> HrtfError {
    HrtfError::Format(message.into())
}

fn unsupported(message: impl Into<String>) -> HrtfError {
    HrtfError::Unsupported(message.into())
}

fn q15(value: i32) -> f32 {
    value as f32 * Q15
}

fn q15_exp(value: i32, exponent: i32) -> f32 {
    q15(value) * (2.0f64.powi(exponent) as f32)
}

fn low16(value: i32) -> i32 {
    value & 0xFFFF
}

fn as_i32(value: &Value) -> Option<i32> {
    value.as_i64().and_then(|value| i32::try_from(value).ok())
}

fn parse_lanes(value: &Value) -> Result<Vec<i32>, HrtfError> {
    let array = value
        .as_array()
        .ok_or_else(|| fail("rosella_coefficients must be a JSON array"))?;
    let mut lanes = Vec::with_capacity(array.len());
    for item in array {
        let Some(value) = as_i32(item) else {
            return Err(fail("rosella_coefficients contains a non-integer value"));
        };
        lanes.push(value);
    }
    Ok(lanes)
}

struct RpHeader {
    stored_checksum: i32,
    table_a_present: bool,
    table_b_present: bool,
    table_c_present: bool,
    table_a_dimension: usize,
    table_a_option: usize,
    table_a_extra: usize,
    table_b_dimension: usize,
    table_b_extra: usize,
    table_b_groups: usize,
    table_c_dimension: usize,
    active_lanes: usize,
}

fn inspect_rp(lanes: &[i32]) -> Result<RpHeader, HrtfError> {
    if lanes.len() < 5 {
        return Err(fail("Rosella rp must contain whole int32 lanes"));
    }
    if low16(lanes[0]) != 0x7072 {
        return Err(fail("bad Rosella rp magic"));
    }
    let mut header = RpHeader {
        stored_checksum: low16(lanes[1]),
        table_a_present: low16(lanes[2]) != 0,
        table_b_present: low16(lanes[3]) != 0,
        table_c_present: low16(lanes[4]) != 0,
        table_a_dimension: 77,
        table_a_option: 0,
        table_a_extra: 0,
        table_b_dimension: 0,
        table_b_extra: 0,
        table_b_groups: 0,
        table_c_dimension: 0,
        active_lanes: 0,
    };
    let mut index = 5usize;
    if header.table_a_present {
        header.table_a_dimension = low16(lanes[index]) as usize;
        header.table_a_option = low16(lanes[index + 1]) as usize;
        header.table_a_extra = low16(lanes[index + 2]) as usize;
        index += 5;
    }
    if header.table_b_present {
        if !header.table_a_present {
            return Err(fail("Rosella rp table B cannot be present without table A"));
        }
        header.table_b_dimension = low16(lanes[index]) as usize;
        header.table_b_extra = low16(lanes[index + 1]) as usize;
        header.table_b_groups = low16(lanes[index + 2]) as usize;
        index += 3;
    }
    if header.table_c_present {
        header.table_c_dimension = low16(lanes[index]) as usize;
        index += 1;
    }
    let payload_words: i64 = index as i64 - 2
        + if header.table_b_present {
            (header.table_b_dimension + 380 * header.table_b_groups + header.table_b_extra + 79)
                as i64
        } else {
            0
        }
        + if header.table_a_present {
            (171 * header.table_a_extra
                + 79
                + 2 * (header.table_a_option + 14 * header.table_a_dimension)) as i64
        } else {
            0
        }
        + 11
        + if header.table_c_present {
            (314 * header.table_c_dimension + 1) as i64
        } else {
            0
        };
    if payload_words < 0 {
        return Err(fail("malformed Rosella rp header"));
    }
    header.active_lanes = (2 + payload_words) as usize;
    if lanes.len() < header.active_lanes {
        return Err(fail("Rosella rp is truncated"));
    }
    let mut computed = 0xA569u32;
    for lane in &lanes[2..header.active_lanes] {
        computed ^= low16(*lane) as u32;
    }
    if (computed & 0xFFFF) as i32 != header.stored_checksum {
        return Err(fail(
            "invalid or non-active Rosella rp coefficient sequence",
        ));
    }
    if header.active_lanes != lanes.len() {
        return Err(fail(
            "invalid or non-active Rosella rp coefficient sequence",
        ));
    }
    Ok(header)
}

fn unpack_field(serialized: &[i32], directions: usize, exponent: i32) -> Vec<f32> {
    let mut padded = vec![0.0f32; 160 * directions];
    let stride8 = 8 * directions;
    let stride2 = 2 * directions;
    for source in 0..154 * directions {
        let group4 = (source % stride8) / stride2;
        let destination = (group4 & 3)
            + 4 * (source % stride2 + 2 * directions * (source / stride8 + (group4 >> 2)));
        padded[destination] = q15_exp(serialized[source], exponent);
    }
    padded
}

fn unpack_table_a_grid(
    serialized: &[i32],
    dimension: usize,
    serialized_rows: usize,
    padded_rows: usize,
    lane_group: usize,
) -> Vec<f32> {
    let mut padded = vec![0.0f32; padded_rows * dimension];
    let group_width = lane_group * 4;
    for source in 0..serialized_rows * dimension {
        let remainder = source % group_width;
        let destination = (remainder / lane_group)
            + 4 * (remainder % lane_group + group_width / 4 * (source / group_width));
        padded[destination] = q15(serialized[source]);
    }
    padded
}

fn unpack_table_a_extra(serialized: &[i32]) -> Vec<f32> {
    let mut padded = vec![0.0f32; 160];
    for source in 0..154 {
        let remainder = source & 7;
        let destination = (remainder >> 1) + 4 * ((source & 1) + 2 * (source >> 3));
        padded[destination] = q15(serialized[source]);
    }
    padded
}

/// Load a `.personalized_headphone` JSON model.
pub fn load_personalized_headphone(path: impl AsRef<Path>) -> Result<RosellaModel, HrtfError> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(HrtfError::ModelNotFound(path.display().to_string()));
    }
    let text = std::fs::read_to_string(path).map_err(|source| HrtfError::Io {
        path: path.display().to_string(),
        source,
    })?;
    if text.is_empty() {
        return Err(fail("empty personalized headphone model"));
    }
    let first = text
        .chars()
        .position(|character| !character.is_whitespace())
        .ok_or_else(|| fail("empty personalized headphone model"))?;
    if text.as_bytes()[first] != b'{' {
        return Err(unsupported(
            "raw rp models are not supported; use a .personalized_headphone JSON",
        ));
    }
    let root: Value =
        serde_json::from_str(&text).map_err(|error| HrtfError::Json(error.to_string()))?;
    let inner = root
        .get("personalized_hrtf")
        .ok_or_else(|| fail("personalized_hrtf is missing"))?;
    let parameters = inner
        .get("virtualizer_parameters")
        .ok_or_else(|| fail("virtualizer_parameters is missing"))?;
    let coefficient_member = parameters
        .get("rosella_coefficients")
        .ok_or_else(|| fail("rosella_coefficients is missing"))?;

    let mut model = RosellaModel {
        source_path: path.display().to_string(),
        ..RosellaModel::default()
    };
    if let Some(value) = parameters
        .get("rosella_coefficients_version")
        .and_then(Value::as_str)
    {
        model.coefficient_version = value.to_string();
    }
    if let Some(value) = parameters.get("room_model").and_then(Value::as_str) {
        model.room_model = value.to_string();
    }
    if let Some(capture) = inner.get("phrtf_capture_metadata") {
        let fields = [
            (
                "capture_submission_date",
                &mut model.capture.capture_submission_date,
            ),
            ("capture_type", &mut model.capture.capture_type),
            ("label", &mut model.capture.label),
            ("name", &mut model.capture.name),
            (
                "phrtf_algorithm_version",
                &mut model.capture.algorithm_version,
            ),
            ("phrtf_creation_date", &mut model.capture.creation_date),
            ("uuid", &mut model.capture.uuid),
            ("version", &mut model.capture.version),
        ];
        for (key, target) in fields {
            if let Some(value) = capture.get(key).and_then(Value::as_str) {
                *target = value.to_string();
            }
        }
    }

    let lanes = parse_lanes(coefficient_member)?;
    let mut hasher = Sha256::new();
    for lane in &lanes {
        hasher.update(lane.to_le_bytes());
    }
    model.coefficient_sha256 = format!("{:x}", hasher.finalize());

    let header = inspect_rp(&lanes)?;
    if !header.table_a_present || !header.table_b_present || header.table_c_present {
        return Err(unsupported(
            "the renderer requires table A+B and no table C",
        ));
    }
    if header.table_a_dimension != 64 || header.table_a_option != 3 {
        return Err(unsupported(
            "the renderer requires the observed 64-channel HQMF layout",
        ));
    }
    if header.table_b_dimension != 20 || header.table_b_groups != 36 {
        return Err(unsupported(
            "the renderer requires 20 hybrid groups and 36 direction terms",
        ));
    }

    let values = &lanes;
    let total = lanes.len();
    let mut position = 13usize;
    let extra = header.table_a_extra;
    model.table_a_dimension = header.table_a_dimension;
    model.table_a_option = header.table_a_option;
    model.table_a_extra = extra;
    model.table_a_control = low16(values[position]);
    model.field_exponent = values[position];
    position += 1;
    let option_count = header.table_a_option;
    model.table_a_option_ids = (0..option_count)
        .map(|index| low16(values[position + index]))
        .collect();
    position += option_count;
    model.table_a_option_values = (0..option_count)
        .map(|index| q15(values[position + index]))
        .collect();
    position += option_count;
    model.table_a_scalar = q15(values[position]);
    position += 1;
    let dimension = header.table_a_dimension;
    model.table_a_filter_16x64_padded =
        unpack_table_a_grid(&values[position..], dimension, 16, 20, 16);
    position += 16 * dimension;
    for index in 0..4 {
        model.table_a_four_integers[index] = low16(values[position + index]);
    }
    position += 4;
    model.table_a_integer = low16(values[position]);
    position += 1;
    model.table_a_filter_8x64_padded =
        unpack_table_a_grid(&values[position..], dimension, 8, 10, 8);
    position += 8 * dimension;
    model.table_a_vector16 = (0..16).map(|index| q15(values[position + index])).collect();
    position += 16;
    model.table_a_filter_4x64_padded = unpack_table_a_grid(&values[position..], dimension, 4, 5, 4);
    position += 4 * dimension;
    model.table_a_extra_indices = (0..extra)
        .map(|index| low16(values[position + index]))
        .collect();
    position += extra;
    model.table_a_extra_fields_padded = vec![0.0f32; extra * 160];
    for index in 0..extra {
        let unpacked = unpack_table_a_extra(&values[position..]);
        model.table_a_extra_fields_padded[index * 160..(index + 1) * 160]
            .copy_from_slice(&unpacked);
        position += 154;
    }
    model.table_a_extra_vectors = vec![0.0f32; extra * 16];
    for index in 0..extra {
        for lane in 0..16 {
            model.table_a_extra_vectors[index * 16 + lane] = q15(values[position + lane]);
        }
        position += 16;
    }
    let table_b_start = position;
    if table_b_start != 13 + 1821 + 171 * extra {
        return Err(fail("Rosella table-A parser lost its place"));
    }

    model.sample_rate = 2 * low16(values[position]) as usize;
    position += 1;
    model.matrix_exponent = values[position];
    position += 1;
    let matrix_count = 36 * 36;
    model.matrix_left = (0..matrix_count)
        .map(|index| q15_exp(values[position + index], model.matrix_exponent))
        .collect();
    position += matrix_count;
    model.matrix_right = (0..matrix_count)
        .map(|index| q15_exp(values[position + index], model.matrix_exponent))
        .collect();
    position += matrix_count;
    model.vector_left = (0..36)
        .map(|index| q15_exp(values[position + index], model.matrix_exponent))
        .collect();
    position += 36;
    model.vector_right = (0..36)
        .map(|index| q15_exp(values[position + index], model.matrix_exponent))
        .collect();
    position += 36;

    let serialized_count = 154 * 36;
    model.field_left_padded = unpack_field(&values[position..], 36, model.field_exponent);
    let mut odd_zero = true;
    for index in (1..serialized_count).step_by(2) {
        if q15_exp(values[position + index], model.field_exponent).abs() > 1.0e-6 {
            odd_zero = false;
            break;
        }
    }
    model.field_left_odd_serialized_zero = odd_zero;
    position += serialized_count;
    model.field_right_padded = unpack_field(&values[position..], 36, model.field_exponent);
    position += serialized_count;

    model.hybrid_flags = (0..20)
        .map(|index| low16(values[position + index]))
        .collect();
    position += 20;
    let active_hybrid = model.hybrid_flags.iter().filter(|flag| **flag == 1).count();
    if active_hybrid != header.table_b_extra {
        return Err(fail("hybrid value count does not match the header"));
    }
    model.hybrid_values = (0..active_hybrid)
        .map(|index| q15(values[position + index]))
        .collect();
    position += active_hybrid;
    model.model_scalars = (0..5).map(|index| q15(values[position + index])).collect();
    position += 5;
    let expected_tail = table_b_start
        + header.table_b_dimension
        + 380 * header.table_b_groups
        + header.table_b_extra
        + 79;
    if position != expected_tail {
        return Err(fail("Rosella table-B parser lost its place"));
    }

    model.header_float_scalars[0] = q15(values[position]);
    model.header_float_scalars[1] = q15(values[position + 1]) * 16.0;
    model.header_integer_fields[0] = values[position + 2];
    model.header_integer_fields[1] = low16(values[position + 3]);
    position += 4;
    for profile in 0..4 {
        let mut parsed = RosellaDistanceProfile::default();
        for index in 0..6 {
            parsed.bounds[index] = q15(values[position + index]);
        }
        position += 6;
        parsed.distance_scale_m = q15_exp(values[position], values[position + 1]);
        position += 2;
        parsed.inverse_distance_per_m = q15(values[position]);
        parsed.axis_scales_internal[0] = q15(values[position + 1]);
        parsed.axis_scales_internal[1] = q15(values[position + 2]);
        parsed.axis_scales_internal[2] = q15(values[position + 3]);
        parsed.minimum_normalized_radius = q15(values[position + 4]);
        position += 5;
        model.profiles[profile] = parsed;
    }
    model.profile_tail = (0..8).map(|index| q15(values[position + index])).collect();
    position += 8;
    for index in 0..3 {
        model.post_fields[index] = values[position + index];
    }
    position += 3;
    if position != total {
        return Err(fail("unparsed Rosella coefficient lanes"));
    }
    if model.sample_rate != 48_000 {
        return Err(fail(format!(
            "Rosella model sample rate must be 48000, got {}",
            model.sample_rate
        )));
    }
    Ok(model)
}
