//! `.atmos.metadata` event parsing.
//!
//! The file is a time-ordered list of per-object keyframes. Dolby's writer only
//! emits `samplePos` on the first object of a time group; later objects in the
//! same group omit it and inherit the previous position. This is the same rule
//! used by `atmos_cluster/damf/metadata.py` and is essential for correct timing.
//!
//! Some writers (e.g. Dolby Atmos Renderer 3.3) also emit a trailing keyframe
//! for the last object of a group without repeating its `ID`. A missing `ID`
//! therefore inherits the previous event's object ID, just as a missing
//! `samplePos` inherits the previous position.

use std::path::Path;

use serde_yaml::Value;

use crate::error::{DamfError, Result};
use crate::yaml::{as_bool, as_f32, as_i64, as_sequence, as_str, as_u32, as_u64, as_vec3, get};

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

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: i64,
    /// Absolute sample position in the master timeline.
    pub sample_pos: u64,
    pub position: Option<[f32; 3]>,
    pub gain: Option<Gain>,
    pub ramp_length: Option<u32>,
    pub active: Option<bool>,
    pub size: Option<f32>,
    /// Per-object trim bypass (`trimBypass` in the DAMF metadata).
    pub trim_bypass: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Metadata {
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
                position: get(raw, "pos").and_then(as_vec3),
                gain: get(raw, "gain").and_then(parse_gain),
                ramp_length: get(raw, "rampLength").and_then(as_u32),
                active: get(raw, "active").and_then(as_bool),
                size: get(raw, "size").and_then(as_f32),
                trim_bypass: get(raw, "trimBypass").and_then(as_bool),
            });
        }

        Ok(Self {
            sample_rate,
            events,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn unique_path(name: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "salsa-metadata-{}-{nonce}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn inherits_group_sample_positions_and_parses_fields() {
        let path = unique_path("master.atmos.metadata");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            br#"sampleRate: 48000
events:
  - ID: 3
    samplePos: 0
    gain: 0
  - ID: 10
    samplePos: 1536
    pos: [-1, 1, 0]
    gain: -6
    rampLength: 1536
    active: true
    size: 0.25
  - ID: 11
    pos: [1, 1, 0]
  - ID: 10
    samplePos: 3072
    gain: -inf
    active: false
"#,
        )
        .unwrap();
        let metadata = Metadata::load(&path).unwrap();
        assert_eq!(metadata.sample_rate, 48_000);
        assert_eq!(metadata.events.len(), 4);
        assert_eq!(metadata.events[0].sample_pos, 0);
        assert_eq!(metadata.events[1].sample_pos, 1536);
        assert_eq!(metadata.events[2].sample_pos, 1536);
        assert_eq!(metadata.events[2].position, Some([1.0, 1.0, 0.0]));
        assert_eq!(metadata.events[1].ramp_length, Some(1536));
        assert_eq!(metadata.events[3].sample_pos, 3072);
        assert_eq!(metadata.events[3].gain, Some(Gain::MinusInfinity));
        assert_eq!(metadata.events[3].active, Some(false));
        assert!((metadata.events[1].gain.unwrap().linear() - 0.501187).abs() < 1e-5);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn inherits_missing_object_id_from_previous_event() {
        let path = unique_path("master.atmos.metadata");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [-1, 1, 0]
  - ID: 65
    samplePos: 1536
    pos: [0.13, 1, 0]
  - samplePos: 3072
    pos: [0.14, 1, 0]
  - ID: 58
    samplePos: 4608
    pos: [-0.75, 1, 0]
"#,
        )
        .unwrap();
        let metadata = Metadata::load(&path).unwrap();
        assert_eq!(metadata.events.len(), 4);
        assert_eq!(metadata.events[2].id, 65);
        assert_eq!(metadata.events[2].sample_pos, 3072);
        assert_eq!(metadata.events[2].position, Some([0.14, 1.0, 0.0]));
        assert_eq!(metadata.events[3].id, 58);
        std::fs::remove_file(path).unwrap();
    }
}
