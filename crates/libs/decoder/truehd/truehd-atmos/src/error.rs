use thiserror::Error;

/// Failures while parsing or decoding a TrueHD Atmos access unit.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TrueHdError {
    #[error("TrueHD access unit parse failed: {0}")]
    Parse(#[source] anyhow::Error),

    #[error("TrueHD decode failed: {0}")]
    Decode(#[source] anyhow::Error),

    #[error("OAMD payload parse failed: {0}")]
    Oamd(#[from] objectaudio_oamd::OamdError),

    #[error("IR rejected the decoded frame: {0}")]
    Ir(#[from] objectaudio_ir::IrError),

    #[error("the decoder returned {actual} samples, expected {expected}")]
    SampleLength { expected: usize, actual: usize },

    #[error("presentation 3 returned {channels} channels; TrueHD object audio has at most {max}")]
    ChannelCount { channels: usize, max: usize },
}
