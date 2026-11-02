//! `.atmos.metadata` event parsing.
//!
//! The file is a time-ordered list of per-object keyframes. Dolby's writer only
//! emits `samplePos` on the first object of a time group; later objects in the
//! same group omit it and inherit the previous position.
//!
//! Some writers (e.g. Dolby Atmos Renderer 3.3) also emit a trailing keyframe
//! for the last object of a group without repeating its `ID`. A missing `ID`
//! therefore inherits the previous event's object ID, just as a missing
//! `samplePos` inherits the previous position.

use std::path::Path;

use serde::{Serialize, Serializer};
use serde_yaml::Value;

use crate::error::{DamfError, Result};
use crate::yaml::{
    as_bool, as_f32, as_i64, as_sequence, as_str, as_u32, as_u64, as_vec3, format_yaml_string, get,
};

const DEFAULT_SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gain {
    Db(f32),
    MinusInfinity,
}

impl Gain {
    pub fn linear(self) -> f32 {
        match self {
            Self::Db(db) => 10.0f32.powf(db / 20.0),
            Self::MinusInfinity => 0.0,
        }
    }
}

impl Serialize for Gain {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Gain::Db(db) => serializer.collect_str(&format!("{db}")),
            Gain::MinusInfinity => serializer.collect_str("-inf"),
        }
    }
}

fn serialize_position<S: Serializer>(
    position: &Option<[f32; 3]>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match position {
        Some(position) => serializer.collect_str(&format!(
            "[{}, {}, {}]",
            position[0], position[1], position[2]
        )),
        None => serializer.serialize_none(),
    }
}

fn parse_gain(value: &Value) -> Option<Gain> {
    if let Value::Number(number) = value {
        return number.as_f64().map(|db| Gain::Db(db as f32));
    }
    let text = as_str(value)?.trim();
    if text.to_ascii_lowercase().starts_with("-inf") {
        return Some(Gain::MinusInfinity);
    }
    text.parse::<f32>().ok().map(Gain::Db)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    #[serde(rename = "ID")]
    pub id: i64,
    pub sample_pos: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(
        rename = "pos",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_position"
    )]
    pub position: Option<[f32; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snap: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zones: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decorr: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain: Option<Gain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ramp_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trim_bypass: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_factor: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth_factor: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_track_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binaural_render_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metadata {
    #[serde(rename = "sampleRate")]
    pub sample_rate: u32,
    pub events: Vec<Event>,
}

impl Metadata {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path).map_err(|source| DamfError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let document: Value = serde_yaml::from_reader(file).map_err(|source| DamfError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;

        let sample_rate = get(&document, "sampleRate")
            .and_then(as_u32)
            .filter(|rate| *rate != 0)
            .unwrap_or(DEFAULT_SAMPLE_RATE);

        let raw_events = get(&document, "events")
            .and_then(as_sequence)
            .ok_or_else(|| DamfError::Invalid("metadata has no events list".to_string()))?;

        let mut events = Vec::with_capacity(raw_events.len());
        let mut current_sample = 0u64;
        let mut current_id = None;
        for raw in raw_events {
            if let Some(id) = get(raw, "ID").and_then(as_i64) {
                current_id = Some(id);
            }
            let id = current_id.ok_or(DamfError::MissingField("events[].ID"))?;
            if let Some(sample) = get(raw, "samplePos").and_then(as_u64) {
                current_sample = sample;
            }
            events.push(Event {
                id,
                sample_pos: current_sample,
                active: get(raw, "active").and_then(as_bool),
                position: get(raw, "pos").and_then(as_vec3),
                snap: get(raw, "snap").and_then(as_bool),
                elevation: get(raw, "elevation").and_then(as_bool),
                zones: get(raw, "zones").and_then(as_str).map(ToOwned::to_owned),
                size: get(raw, "size").and_then(as_f32),
                decorr: get(raw, "decorr").and_then(as_u32),
                importance: get(raw, "importance").and_then(as_f32),
                gain: get(raw, "gain").and_then(parse_gain),
                ramp_length: get(raw, "rampLength").and_then(as_u32),
                trim_bypass: get(raw, "trimBypass").and_then(as_bool),
                dialog: get(raw, "dialog").and_then(as_f32),
                music: get(raw, "music").and_then(as_f32),
                screen_factor: get(raw, "screenFactor").and_then(as_f32),
                depth_factor: get(raw, "depthFactor").and_then(as_f32),
                head_track_mode: get(raw, "headTrackMode")
                    .and_then(as_str)
                    .map(ToOwned::to_owned),
                binaural_render_mode: get(raw, "binauralRenderMode")
                    .and_then(as_str)
                    .map(ToOwned::to_owned),
            });
        }

        Ok(Self {
            sample_rate,
            events,
        })
    }

    /// Write a DAMF `.atmos.metadata` file.
    ///
    /// Fields that are `None` are omitted. The YAML is produced by serde and
    /// then re-indented into Dolby's DAMF layout.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let yaml = serde_yaml::to_string(self)
            .map_err(|error| DamfError::Invalid(format!("serializing DAMF metadata: {error}")))?;
        std::fs::write(path, format_yaml_string(yaml)).map_err(|source| DamfError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;

    #[test]
    fn saves_and_reloads_extended_fields() {
        let path = std::env::temp_dir().join(format!(
            "salsa-metadata-writer-{}-{}.yaml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let metadata = Metadata {
            sample_rate: 48_000,
            events: vec![Event {
                id: 10,
                sample_pos: 1536,
                active: Some(true),
                position: Some([0.5, 0.25, 0.0]),
                snap: Some(false),
                elevation: Some(true),
                zones: Some("all".to_string()),
                size: Some(0.25),
                decorr: Some(0),
                importance: Some(0.75),
                gain: Some(Gain::Db(0.0)),
                ramp_length: Some(1536),
                trim_bypass: Some(false),
                dialog: Some(-1.0),
                music: Some(1.0),
                screen_factor: Some(0.0),
                depth_factor: Some(0.25),
                head_track_mode: Some("undefined".to_string()),
                binaural_render_mode: Some("undefined".to_string()),
            }],
        };
        metadata.save(&path).unwrap();
        let reloaded = Metadata::load(&path).unwrap();
        assert_eq!(reloaded.sample_rate, 48_000);
        assert_eq!(reloaded.events.len(), 1);
        assert_eq!(reloaded.events[0].importance, Some(0.75));
        assert_eq!(reloaded.events[0].dialog, Some(-1.0));
        assert_eq!(reloaded.events[0].music, Some(1.0));
        assert_eq!(reloaded.events[0].zones.as_deref(), Some("all"));
        assert_eq!(reloaded.events[0].snap, Some(false));
        assert_eq!(reloaded.events[0].elevation, Some(true));
        assert_eq!(reloaded.events[0].screen_factor, Some(0.0));
        assert_eq!(reloaded.events[0].depth_factor, Some(0.25));
        assert_eq!(
            reloaded.events[0].head_track_mode.as_deref(),
            Some("undefined")
        );
        std::fs::remove_file(path).unwrap();
    }
}
