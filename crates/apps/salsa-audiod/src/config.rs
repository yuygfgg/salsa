//! Persistent service configuration.
//!
//! Installation only places the binary and LaunchAgent. Output device and
//! render settings live here and are changed at runtime with
//! `salsa-audiod configure`.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::pipeline::{Codec, RenderMode};
use crate::service::{CarrierSource, ServiceConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub shm_name: String,
    /// Physical output selector (unique ID, name, or list number).
    pub device: Option<String>,
    /// `eac3`, `truehd`, or `null` for auto-detection.
    pub codec: Option<String>,
    pub sample_rate: u32,
    pub layout: String,
    /// `speaker` or `binaural`. Older files may omit it; a present model then
    /// implies binaural.
    pub render: Option<String>,
    pub binaural_model: Option<PathBuf>,
    pub filterbank: u32,
    pub profile: String,
    pub scheduling_allowance_ms: f64,
    pub physical_latency_ms: f64,
    pub declare_latency: bool,
    pub deadline_timeout_ms: f64,
    pub hard_stall_timeout_ms: f64,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            shm_name: String::new(),
            device: None,
            codec: None,
            sample_rate: 48_000,
            layout: "7.1.4".to_string(),
            render: None,
            binaural_model: None,
            filterbank: 2,
            profile: "mid".to_string(),
            scheduling_allowance_ms: 50.0,
            physical_latency_ms: 0.0,
            declare_latency: true,
            deadline_timeout_ms: 2000.0,
            hard_stall_timeout_ms: 10000.0,
        }
    }
}

impl FileConfig {
    /// Configuration path for a per-codec service instance (`eac3`/`truehd`).
    pub fn profile_path(profile: &str) -> Result<PathBuf> {
        if let Some(path) = env::var_os("SALSA_AUDIOD_CONFIG") {
            return Ok(PathBuf::from(path));
        }
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library/Application Support/Salsa")
            .join(format!("audiod-{profile}.json")))
    }

    /// Defaults for a per-codec service instance. The shared-memory names must
    /// match `platform/macos/salsa-hal/build.sh`.
    pub fn profile_default(profile: &str) -> Self {
        let mut config = Self::default();
        match profile {
            "eac3" => {
                config.shm_name = "/salsa-hal-carrier-eac3".to_string();
                config.codec = Some("eac3".to_string());
            }
            "truehd" => {
                config.shm_name = "/salsa-hal-carrier-truehd".to_string();
                config.codec = Some("truehd".to_string());
            }
            _ => {}
        }
        config
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }

    pub fn codec(&self) -> Result<Option<Codec>> {
        match self.codec.as_deref() {
            None | Some("") | Some("auto") => Ok(None),
            Some("eac3") => Ok(Some(Codec::Eac3)),
            Some("truehd") => Ok(Some(Codec::TrueHd)),
            Some(other) => bail!("unknown codec '{other}' (use eac3 or truehd)"),
        }
    }

    pub fn render_mode(&self) -> Result<RenderMode> {
        let binaural = match self.render.as_deref() {
            Some("binaural") => true,
            Some("speaker") => false,
            None => self.binaural_model.is_some(),
            Some(other) => bail!("unknown render mode '{other}' (use speaker or binaural)"),
        };
        if binaural {
            let model = self
                .binaural_model
                .clone()
                .context("binaural rendering requires a --binaural-model path")?;
            Ok(RenderMode::Binaural {
                model,
                filterbank: self.filterbank,
                profile: self.profile.clone(),
            })
        } else {
            Ok(RenderMode::speaker(self.layout.clone()))
        }
    }

    /// Apply the persistent settings to a live service configuration. Device
    /// resolution and validation happen in the caller.
    pub fn apply(&self, config: &mut ServiceConfig) -> Result<()> {
        config.source = CarrierSource::Shared(self.shm_name.clone());
        config.codec = self.codec()?;
        config.sample_rate = self.sample_rate;
        config.render = self.render_mode()?;
        config.scheduling_allowance_ns =
            (self.scheduling_allowance_ms.max(0.0) * 1_000_000.0) as u64;
        config.physical_output_latency_ns =
            (self.physical_latency_ms.max(0.0) * 1_000_000.0) as u64;
        config.deadline_timeout =
            std::time::Duration::from_secs_f64(self.deadline_timeout_ms.max(1.0) / 1000.0);
        config.hard_stall_timeout =
            std::time::Duration::from_secs_f64(self.hard_stall_timeout_ms.max(1.0) / 1000.0);
        config.declare_latency = self.declare_latency;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_renderable() {
        let config = FileConfig::default();
        assert_eq!(config.codec().unwrap(), None);
        assert!(matches!(
            config.render_mode().unwrap(),
            RenderMode::Speaker { .. }
        ));
        assert!(config.device.is_none());
    }

    #[test]
    fn profile_defaults_match_the_hal_carriers() {
        let eac3 = FileConfig::profile_default("eac3");
        assert_eq!(eac3.shm_name, "/salsa-hal-carrier-eac3");
        assert_eq!(eac3.codec().unwrap(), Some(Codec::Eac3));
        let truehd = FileConfig::profile_default("truehd");
        assert_eq!(truehd.shm_name, "/salsa-hal-carrier-truehd");
        assert_eq!(truehd.codec().unwrap(), Some(Codec::TrueHd));
    }

    #[test]
    fn codec_and_binaural_parsing() {
        let mut config = FileConfig {
            codec: Some("truehd".into()),
            binaural_model: Some(PathBuf::from("/tmp/model")),
            ..Default::default()
        };
        assert_eq!(config.codec().unwrap(), Some(Codec::TrueHd));
        assert!(config.render_mode().unwrap().is_binaural());
        config.codec = Some("bad".into());
        assert!(config.codec().is_err());
    }

    #[test]
    fn explicit_speaker_wins_over_a_stale_model() {
        let config = FileConfig {
            render: Some("speaker".into()),
            binaural_model: Some(PathBuf::from("/tmp/model")),
            ..Default::default()
        };
        assert!(!config.render_mode().unwrap().is_binaural());
    }

    #[test]
    fn binaural_requires_a_model() {
        let config = FileConfig {
            render: Some("binaural".into()),
            ..Default::default()
        };
        assert!(config.render_mode().is_err());
    }
}
