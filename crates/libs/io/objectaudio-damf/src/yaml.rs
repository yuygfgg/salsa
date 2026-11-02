//! Small helpers for the deliberately permissive DAMF YAML schema.
//!
//! Dolby's conversion tools and third-party exporters disagree about whether
//! numeric fields are YAML numbers or strings, so every accessor accepts both.

use std::fmt::Display;

use serde::{Serialize, Serializer};
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

/// A sequence that serde writes inline (`[a, b, c]`) instead of as a YAML
/// block sequence.
pub(crate) struct VecDisplay<T>(pub Vec<T>);

impl<T: Display> Serialize for VecDisplay<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use std::fmt::Write;

        let mut result = String::with_capacity(self.0.len() * 8 + 2);
        result.push('[');
        for (index, item) in self.0.iter().enumerate() {
            if index > 0 {
                result.push_str(", ");
            }
            write!(result, "{item}").unwrap();
        }
        result.push(']');
        serializer.collect_str(&result)
    }
}

/// Re-indent serde's YAML into Dolby's DAMF layout.
///
/// Only leading whitespace and quoting are rewritten, never the content of a
/// value. Numbers and inline numeric sequences are unquoted because DAMF
/// writers emit them bare.
pub(crate) fn format_yaml_string(yaml_str: String) -> String {
    let mut out = String::with_capacity(yaml_str.len() * 2);
    for line in yaml_str.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let (marker, content) = match line[indent..].strip_prefix("- ") {
            Some(content) => ("- ", content),
            None => ("", &line[indent..]),
        };

        for _ in 0..indent * 2 + marker.len() {
            out.push(' ');
        }
        out.push_str(marker);

        match content
            .split_once(": ")
            .map(|(key, value)| (key, bare_scalar(value)))
        {
            Some((key, Some(value))) => {
                out.push_str(key);
                out.push_str(": ");
                out.push_str(value);
            }
            _ => out.push_str(content),
        }
        out.push('\n');
    }
    out
}

fn bare_scalar(value: &str) -> Option<&str> {
    let inner = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))?;

    let is_sequence = inner
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .is_some_and(|items| {
            items.trim().is_empty() || items.split(',').all(|item| is_number(item.trim()))
        });

    (is_number(inner) || is_sequence).then_some(inner)
}

fn is_number(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (integer, fraction) = match digits.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (digits, None),
    };

    !integer.is_empty()
        && integer.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit())
        })
}
