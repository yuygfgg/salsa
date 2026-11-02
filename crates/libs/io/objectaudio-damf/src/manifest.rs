//! `.atmos` manifest parsing and DAMF sidecar resolution.

use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};
use serde_yaml::Value;

use crate::error::{DamfError, Result};
use crate::yaml::{VecDisplay, as_f64, as_i64, as_sequence, as_str, format_yaml_string, get};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BedEntry {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectEntry {
    pub id: i64,
    pub description: Option<String>,
}

/// The parts of a DAMF manifest used by the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub path: PathBuf,
    pub audio: PathBuf,
    pub metadata: PathBuf,
    pub offset: f64,
    pub fps: f64,
    pub beds: Vec<BedEntry>,
    pub objects: Vec<ObjectEntry>,
}

impl Manifest {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let document = read_yaml(&path)?;
        let presentations = get(&document, "presentations")
            .and_then(as_sequence)
            .filter(|presentations| !presentations.is_empty())
            .ok_or_else(|| DamfError::Invalid("manifest has no presentations".to_string()))?;
        let presentation = &presentations[0];

        let audio_name = get(presentation, "audio")
            .and_then(as_str)
            .ok_or(DamfError::MissingField("presentations[0].audio"))?;
        let metadata_name = get(presentation, "metadata")
            .and_then(as_str)
            .ok_or(DamfError::MissingField("presentations[0].metadata"))?;

        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let audio = base.join(audio_name);
        let metadata = base.join(metadata_name);

        let mut beds = Vec::new();
        if let Some(instances) = get(presentation, "bedInstances").and_then(as_sequence) {
            for instance in instances {
                let Some(channels) = get(instance, "channels").and_then(as_sequence) else {
                    continue;
                };
                for channel in channels {
                    let name = get(channel, "channel")
                        .and_then(as_str)
                        .ok_or(DamfError::MissingField("bedInstances[].channels[].channel"))?;
                    let id = get(channel, "ID")
                        .and_then(as_i64)
                        .ok_or(DamfError::MissingField("bedInstances[].channels[].ID"))?;
                    beds.push(BedEntry {
                        id,
                        name: name.to_string(),
                    });
                }
            }
        }

        let mut objects = Vec::new();
        if let Some(entries) = get(presentation, "objects").and_then(as_sequence) {
            for entry in entries {
                let id = get(entry, "ID")
                    .and_then(as_i64)
                    .ok_or(DamfError::MissingField("objects[].ID"))?;
                let description = get(entry, "description")
                    .and_then(as_str)
                    .map(ToOwned::to_owned);
                objects.push(ObjectEntry { id, description });
            }
        }

        let offset = get(presentation, "offset").and_then(as_f64).unwrap_or(0.0);
        let fps = get(presentation, "fps").and_then(as_f64).unwrap_or(24.0);
        let audio = resolve_audio(&audio, &path);
        let metadata = resolve_metadata(&metadata, &path);

        Ok(Self {
            path,
            audio,
            metadata,
            offset,
            fps,
            beds,
            objects,
        })
    }
}

/// A DAMF manifest to write.
///
/// This is deliberately format-level: the caller decides the bed channels,
/// object IDs and presentation metadata. The reader resolves sidecar paths,
/// while the writer stores the names exactly as given.
#[derive(Debug, Clone, Copy)]
pub struct ManifestSpec<'a> {
    pub audio_name: &'a str,
    pub metadata_name: &'a str,
    pub offset: f64,
    pub fps: f64,
    pub sc_bed_configuration: &'a [i64],
    pub creation_tool: &'a str,
    pub creation_tool_version: &'a str,
    pub beds: &'a [BedEntry],
    pub objects: &'a [ObjectEntry],
}

#[derive(Serialize)]
struct ManifestDocument<'a> {
    version: &'a str,
    presentations: Vec<PresentationDocument<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresentationDocument<'a> {
    #[serde(rename = "type")]
    presentation_type: &'a str,
    simplified: bool,
    metadata: &'a str,
    audio: &'a str,
    offset: f64,
    #[serde(serialize_with = "serialize_fps")]
    fps: f64,
    #[serde(rename = "scBedConfiguration")]
    sc_bed_configuration: VecDisplay<i64>,
    creation_tool: &'a str,
    creation_tool_version: &'a str,
    bed_instances: Vec<BedInstanceDocument<'a>>,
    objects: Vec<ObjectDocument<'a>>,
}

#[derive(Serialize)]
struct BedInstanceDocument<'a> {
    channels: Vec<ChannelDocument<'a>>,
}

#[derive(Serialize)]
struct ChannelDocument<'a> {
    channel: &'a str,
    #[serde(rename = "ID")]
    id: i64,
}

#[derive(Serialize)]
struct ObjectDocument<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(rename = "ID")]
    id: i64,
}

fn serialize_fps<S: Serializer>(fps: &f64, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    if fps.fract() == 0.0 {
        serializer.collect_str(&format!("{}", *fps as i64))
    } else {
        serializer.serialize_f64(*fps)
    }
}

/// Write a standard DAMF `.atmos` manifest.
pub fn write_manifest(path: impl AsRef<Path>, spec: &ManifestSpec<'_>) -> Result<()> {
    let path = path.as_ref();
    let channels = spec
        .beds
        .iter()
        .map(|bed| ChannelDocument {
            channel: &bed.name,
            id: bed.id,
        })
        .collect();
    let objects = spec
        .objects
        .iter()
        .map(|object| ObjectDocument {
            description: object.description.as_deref(),
            id: object.id,
        })
        .collect();
    let document = ManifestDocument {
        version: "0.5.1",
        presentations: vec![PresentationDocument {
            presentation_type: "home",
            simplified: false,
            metadata: spec.metadata_name,
            audio: spec.audio_name,
            offset: spec.offset,
            fps: spec.fps,
            sc_bed_configuration: VecDisplay(spec.sc_bed_configuration.to_vec()),
            creation_tool: spec.creation_tool,
            creation_tool_version: spec.creation_tool_version,
            bed_instances: vec![BedInstanceDocument { channels }],
            objects,
        }],
    };
    let yaml = serde_yaml::to_string(&document)
        .map_err(|error| DamfError::Invalid(format!("serializing DAMF manifest: {error}")))?;
    std::fs::write(path, format_yaml_string(yaml)).map_err(|source| DamfError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Resolve the audio essence the way Cavern does, but also keep the manifest's
/// explicit path and the standard `.atmos.audio` / `.caf` spellings.
fn resolve_audio(declared: &Path, manifest: &Path) -> PathBuf {
    let mut candidates = vec![declared.to_path_buf()];
    candidates.push(PathBuf::from(format!("{}.audio", manifest.display())));
    if let Some(stem) = manifest.to_string_lossy().strip_suffix(".atmos") {
        candidates.push(PathBuf::from(format!("{stem}.caf")));
    }
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .unwrap_or_else(|| declared.to_path_buf())
}

fn resolve_metadata(declared: &Path, manifest: &Path) -> PathBuf {
    let mut candidates = vec![declared.to_path_buf()];
    candidates.push(PathBuf::from(format!("{}.metadata", manifest.display())));
    if let Some(stem) = manifest.to_string_lossy().strip_suffix(".atmos") {
        candidates.push(PathBuf::from(format!("{stem}.metadata")));
    }
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .unwrap_or_else(|| declared.to_path_buf())
}

fn read_yaml(path: &Path) -> Result<Value> {
    let file = std::fs::File::open(path).map_err(|source| DamfError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_yaml::from_reader(file).map_err(|source| DamfError::Yaml {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn unique_path(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "salsa-manifest-{}-{nonce}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn parses_beds_and_objects() {
        let path = unique_path("master.atmos");
        let audio = path.with_file_name("master.atmos.audio");
        let metadata = path.with_file_name("master.atmos.metadata");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            br#"version: 0.5.1
presentations:
  - type: home
    audio: master.atmos.audio
    metadata: master.atmos.metadata
    offset: 0.0
    bedInstances:
      - channels:
          - channel: LFE
            ID: 3
    objects:
      - ID: 10
      - ID: 11
"#,
        )
        .unwrap();
        std::fs::File::create(&audio).unwrap();
        std::fs::File::create(&metadata).unwrap();

        let manifest = Manifest::load(&path).unwrap();
        assert_eq!(manifest.audio, audio);
        assert_eq!(manifest.metadata, metadata);
        assert_eq!(
            manifest.beds,
            vec![BedEntry {
                id: 3,
                name: "LFE".to_string()
            }]
        );
        assert_eq!(
            manifest.objects,
            vec![
                ObjectEntry {
                    id: 10,
                    description: None
                },
                ObjectEntry {
                    id: 11,
                    description: None
                }
            ]
        );

        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(audio).unwrap();
        std::fs::remove_file(metadata).unwrap();
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;

    #[test]
    fn writes_a_manifest() {
        let path = std::env::temp_dir().join(format!(
            "salsa-clustered-{}-{}.atmos",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let beds = [BedEntry {
            id: 3,
            name: "LFE".to_string(),
        }];
        let objects = [
            ObjectEntry {
                id: 10,
                description: Some("dialogue".to_string()),
            },
            ObjectEntry {
                id: 11,
                description: None,
            },
        ];
        write_manifest(
            &path,
            &ManifestSpec {
                audio_name: "stem.atmos.audio",
                metadata_name: "stem.atmos.metadata",
                offset: 0.0,
                fps: 24.0,
                sc_bed_configuration: &[3],
                creation_tool: "test",
                creation_tool_version: "0.1.0",
                beds: &beds,
                objects: &objects,
            },
        )
        .unwrap();
        let manifest = Manifest::load(&path).unwrap();
        assert_eq!(manifest.fps, 24.0);
        assert_eq!(manifest.beds.len(), 1);
        assert_eq!(manifest.beds[0].name, "LFE");
        assert_eq!(manifest.objects.len(), 2);
        assert_eq!(manifest.objects[0].id, 10);
        assert_eq!(manifest.objects[0].description.as_deref(), Some("dialogue"));
        assert_eq!(manifest.objects[1].id, 11);
        std::fs::remove_file(path).unwrap();
    }
}
