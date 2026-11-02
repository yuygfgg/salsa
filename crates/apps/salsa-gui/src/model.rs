//! Small UI-facing state shared by the app and the panels.

use std::path::PathBuf;

use crate::pipeline::Codec;

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub path: PathBuf,
    pub codec: Codec,
    pub rate: u32,
    pub objects: u32,
}

impl SourceInfo {
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("Unknown file")
            .to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Empty,
    Loading,
    Paused,
    Playing,
    Seeking,
    Eos,
    Error,
}

impl Phase {
    pub fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "Idle",
            Self::Loading => "Loading",
            Self::Paused => "Paused",
            Self::Playing => "Playing",
            Self::Seeking => "Seeking",
            Self::Eos => "End of stream",
            Self::Error => "Error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiError {
    pub message: String,
    pub retry_path: Option<PathBuf>,
    pub show_model: bool,
}

impl UiError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_path: None,
            show_model: false,
        }
    }

    pub fn with_retry(message: impl Into<String>, path: PathBuf) -> Self {
        Self {
            message: message.into(),
            retry_path: Some(path),
            show_model: false,
        }
    }

    pub fn missing_model() -> Self {
        Self {
            message: "Choose a Rosella .personalized_headphone model to enable binaural."
                .to_string(),
            retry_path: None,
            show_model: true,
        }
    }
}
