// SPDX-License-Identifier: MPL-2.0
//! Library error type for perceptual object clustering.

use std::fmt::Display;

/// Failure while clustering or rendering a DAMF master.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClusterError {
    #[error("{0}")]
    Message(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Damf(#[from] objectaudio_damf::DamfError),

    #[error("tract inference failed: {0}")]
    Tract(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("invalid array shape: {0}")]
    Shape(#[from] tract_onnx::prelude::tract_ndarray::ShapeError),

    #[error("invalid audio buffer size: {0}")]
    BufferSize(#[from] rubato::audioadapter_buffers::SizeError),

    #[error(transparent)]
    ResamplerConstruction(#[from] rubato::ResamplerConstructionError),

    #[error(transparent)]
    Resample(#[from] rubato::ResampleError),
}

pub type Result<T> = std::result::Result<T, ClusterError>;

impl From<tract_onnx::prelude::TractError> for ClusterError {
    fn from(error: tract_onnx::prelude::TractError) -> Self {
        Self::Tract(Box::from(error))
    }
}

impl ClusterError {
    pub(crate) fn msg(message: impl Display) -> Self {
        Self::Message(message.to_string())
    }
}

/// Local `Context` helper for the library error type.
pub(crate) trait Context<T> {
    fn context(self, message: impl Display) -> Result<T>;
    fn with_context<F: FnOnce() -> String>(self, message: F) -> Result<T>;
}

impl<T, E: Display> Context<T> for std::result::Result<T, E> {
    fn context(self, message: impl Display) -> Result<T> {
        self.map_err(|error| ClusterError::msg(format_args!("{message}: {error}")))
    }

    fn with_context<F: FnOnce() -> String>(self, message: F) -> Result<T> {
        self.map_err(|error| ClusterError::msg(format_args!("{}: {error}", message())))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, message: impl Display) -> Result<T> {
        self.ok_or_else(|| ClusterError::msg(message))
    }

    fn with_context<F: FnOnce() -> String>(self, message: F) -> Result<T> {
        self.ok_or_else(|| ClusterError::msg(message()))
    }
}

#[macro_export]
macro_rules! ensure {
    ($condition:expr, $($arg:tt)*) => {
        if $condition {
        } else {
            return Err($crate::ClusterError::msg(format_args!($($arg)*)));
        }
    };
}

#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::ClusterError::msg(format_args!($($arg)*)))
    };
}
