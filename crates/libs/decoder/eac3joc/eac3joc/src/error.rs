//! Errors raised while turning an E-AC-3 bitstream into a [`crate::Eac3JocFrame`].

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Eac3JocError {
    #[error("syncframe header truncated at end of input")]
    TruncatedHeader,

    #[error("invalid syncframe header at byte {offset}")]
    InvalidSyncframe { offset: usize },

    #[error(
        "last syncframe extends past the end of input (declared {declared} bytes, {remaining} remaining)"
    )]
    TruncatedFrame { declared: usize, remaining: usize },

    #[error("bitstream truncated")]
    BitstreamTruncated,

    #[error("EMDF syntax error: {0}")]
    EmdfSyntax(String),

    #[error("EMDF transport error: {0}")]
    EmdfTransport(String),

    #[error("JOC syntax error: {0}")]
    JocSyntax(String),

    #[error("unsupported JOC variant: {0}")]
    UnsupportedVariant(String),

    #[error("OAMD error: {0}")]
    Oamd(#[from] objectaudio_oamd::OamdError),
}

pub type Result<T> = std::result::Result<T, Eac3JocError>;
