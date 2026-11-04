use thiserror::Error;

/// Failures while turning an OAMD payload into the salsa IR.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OamdError {
    #[error("OAMD LFE2 bed is not supported")]
    UnsupportedLfe2,

    #[error("failed to parse OAMD payload: {0}")]
    Parse(#[source] anyhow::Error),

    #[error("OAMD declares {count} objects, limit is {limit}")]
    TooManyObjects { count: usize, limit: usize },

    #[error("OAMD declares {count} object-info blocks, limit is {limit}")]
    TooManyBlocks { count: usize, limit: usize },

    #[error("OAMD contains {count} object elements")]
    MultipleObjectElements { count: usize },

    #[error("OAMD has {object_count} objects but no object element")]
    MissingObjectElement { object_count: usize },

    #[error("OAMD encode failed: {0}")]
    Encode(String),
}
