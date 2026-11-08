//! Errors shared by the salsa-spatial syntax, parameter and framing layers.

/// Result alias for this crate.
pub type Result<T> = core::result::Result<T, SpatialError>;

/// A syntax, framing, parameter or entropy failure.
#[derive(Debug, thiserror::Error)]
pub enum SpatialError {
    #[error("truncated salsa-spatial data")]
    Truncated,
    #[error("invalid salsa-spatial syntax: {0}")]
    Syntax(&'static str),
    #[error("invalid salsa-spatial value: {0}")]
    InvalidValue(&'static str),
    #[error("invalid salsa-spatial parameter: {0}")]
    Parameter(&'static str),
    #[error("rANS stream is empty or truncated")]
    RansTruncated,
    #[error("rANS probability model is invalid")]
    RansModel,
    #[error("unsupported stream version {version}")]
    UnsupportedVersion { version: u16 },
    #[error("object count {0} exceeds the profile maximum")]
    TooManyObjects(usize),
    #[error("bitstream error")]
    Bitstream,
}

impl From<salsa_bitstream::BitstreamError> for SpatialError {
    fn from(_: salsa_bitstream::BitstreamError) -> Self {
        Self::Bitstream
    }
}
