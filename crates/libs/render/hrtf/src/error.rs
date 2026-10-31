use thiserror::Error;

/// Failures while loading a Rosella model or running the binaural renderer.
#[derive(Debug, Error)]
pub enum HrtfError {
    #[error("personalized headphone model not found: {0}")]
    ModelNotFound(String),

    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid personalized headphone JSON: {0}")]
    Json(String),

    #[error("invalid Rosella model: {0}")]
    Format(String),

    #[error("unsupported Rosella model: {0}")]
    Unsupported(String),

    #[error("invalid renderer argument: {0}")]
    InvalidArgument(String),

    #[error("renderer state error: {0}")]
    State(String),

    #[error("renderer input must be {expected} channels, got {actual}")]
    ChannelCount { expected: usize, actual: usize },

    #[error("renderer input must be a multiple of {0} samples")]
    BlockAlignment(usize),
}
