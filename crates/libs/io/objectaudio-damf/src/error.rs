use std::path::PathBuf;

/// Errors raised while reading a Dolby Atmos Master Format presentation.
#[derive(Debug, thiserror::Error)]
pub enum DamfError {
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid YAML in {path}: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("invalid DAMF: {0}")]
    Invalid(String),

    #[error("missing required DAMF field: {0}")]
    MissingField(&'static str),

    #[error("unsupported CAF feature: {0}")]
    UnsupportedCaf(String),

    #[error("channel layout mismatch: CAF has {caf} channels, manifest describes {manifest}")]
    ChannelMismatch { caf: usize, manifest: usize },
}

pub type Result<T> = std::result::Result<T, DamfError>;
