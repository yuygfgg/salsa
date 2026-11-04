// SPDX-License-Identifier: MPL-2.0
//! Library error type for the DAMF -> E-AC-3 JOC encoder.

use std::fmt::Display;

/// Failure while encoding a DAMF master.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EncoderError {
    #[error("{0}")]
    Message(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Damf(#[from] objectaudio_damf::DamfError),

    #[error(transparent)]
    Ffmpeg(#[from] ffmpeg_next::Error),

    #[error(transparent)]
    Reac3e(#[from] reac3e::EncodeError),

    #[error(transparent)]
    Oamd(#[from] objectaudio_oamd::OamdError),
}

pub type Result<T> = std::result::Result<T, EncoderError>;

impl EncoderError {
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
        self.map_err(|error| EncoderError::msg(format_args!("{message}: {error}")))
    }

    fn with_context<F: FnOnce() -> String>(self, message: F) -> Result<T> {
        self.map_err(|error| EncoderError::msg(format_args!("{}: {error}", message())))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, message: impl Display) -> Result<T> {
        self.ok_or_else(|| EncoderError::msg(message))
    }

    fn with_context<F: FnOnce() -> String>(self, message: F) -> Result<T> {
        self.ok_or_else(|| EncoderError::msg(message()))
    }
}

#[macro_export]
macro_rules! ensure {
    ($condition:expr, $($arg:tt)*) => {
        if $condition {
        } else {
            return Err($crate::EncoderError::msg(format_args!($($arg)*)));
        }
    };
}

#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::EncoderError::msg(format_args!($($arg)*)))
    };
}
