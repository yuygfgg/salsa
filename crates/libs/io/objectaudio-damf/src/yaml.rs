//! Small helpers for the deliberately permissive DAMF YAML schema.
//!
//! Dolby's conversion tools and third-party exporters disagree about whether
//! numeric fields are YAML numbers or strings, so every accessor accepts both.

use serde_yaml::Value;

pub(crate) fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

pub(crate) fn as_sequence(value: &Value) -> Option<&Vec<Value>> {
    value.as_sequence()
}

pub(crate) fn as_str(value: &Value) -> Option<&str> {
    value.as_str()
}

pub(crate) fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().map(|number| number as i64))
            .or_else(|| number.as_f64().map(|number| number as i64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| {
                number
                    .as_i64()
                    .and_then(|number| u64::try_from(number).ok())
            })
            .or_else(|| number.as_f64().map(|number| number as u64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn as_u32(value: &Value) -> Option<u32> {
    as_u64(value).and_then(|value| u32::try_from(value).ok())
}

pub(crate) fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn as_f32(value: &Value) -> Option<f32> {
    as_f64(value).map(|value| value as f32)
}

pub(crate) fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        Value::Number(number) => number.as_i64().map(|value| value != 0),
        _ => None,
    }
}

/// Read a three-component vector from either a YAML sequence or a string like
/// `[1, 0.5, 0]`. The latter appears in some minimal YAML writers.
pub(crate) fn as_vec3(value: &Value) -> Option<[f32; 3]> {
    if let Some(sequence) = value.as_sequence() {
        if sequence.len() < 3 {
            return None;
        }
        return Some([
            as_f32(&sequence[0])?,
            as_f32(&sequence[1])?,
            as_f32(&sequence[2])?,
        ]);
    }
    let text = value.as_str()?.trim();
    let inner = text.strip_prefix('[')?.strip_suffix(']')?;
    let mut parts = inner.split(',').map(str::trim);
    let position = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    Some(position)
}
