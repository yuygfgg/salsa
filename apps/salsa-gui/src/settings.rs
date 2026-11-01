//! Small JSON settings store. No hidden defaults: every persisted value is
//! shown in the UI and only changes when the user changes it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::pipeline::{BinauralConfig, DeviceSelection, OutputConfig, OutputMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentFile {
    pub path: PathBuf,
    pub codec: String,
    pub objects: u32,
    pub opened_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mode: OutputMode,
    #[serde(default = "default_layout")]
    pub layout: String,
    #[serde(default)]
    pub binaural: BinauralConfig,
    #[serde(default)]
    pub device: DeviceSelection,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default = "default_true")]
    pub visual_enabled: bool,
    #[serde(default)]
    pub reduce_motion: bool,
    #[serde(default)]
    pub recent: Vec<RecentFile>,
    #[serde(default)]
    pub window_size: Option<[f32; 2]>,
    #[serde(default)]
    pub window_position: Option<[f32; 2]>,
}

fn default_layout() -> String {
    "7.1.4".to_string()
}

fn default_volume() -> f32 {
    0.8
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: OutputMode::Speaker,
            layout: default_layout(),
            binaural: BinauralConfig::default(),
            device: DeviceSelection::Auto,
            volume: default_volume(),
            visual_enabled: true,
            reduce_motion: false,
            recent: Vec::new(),
            window_size: None,
            window_position: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn output_config(&self) -> OutputConfig {
        OutputConfig {
            mode: self.mode.clone(),
            layout: self.layout.clone(),
            binaural: self.binaural.clone(),
            device: self.device.clone(),
        }
    }

    pub fn note_recent(&mut self, path: &Path, codec: &str, objects: u32) {
        self.recent.retain(|entry| entry.path != path);
        self.recent.insert(
            0,
            RecentFile {
                path: path.to_path_buf(),
                codec: codec.to_string(),
                objects,
                opened_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0),
            },
        );
        self.recent.truncate(10);
    }
}

fn settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("SalsaStudio")
                .join("settings.json")
        })
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|appdata| {
            PathBuf::from(appdata)
                .join("SalsaStudio")
                .join("settings.json")
        })
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
            Some(
                PathBuf::from(config)
                    .join("salsa-studio")
                    .join("settings.json"),
            )
        } else {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home)
                    .join(".config")
                    .join("salsa-studio")
                    .join("settings.json")
            })
        }
    }
}
