use thiserror::Error;

/// Validation failures for the IR.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum IrError {
    #[error("sample rate must be non-zero")]
    ZeroSampleRate,

    #[error("metadata sample rate {actual} differs from scene rate {expected}")]
    SampleRateMismatch { expected: u32, actual: u32 },

    #[error("object {object} has an invalid {property}")]
    InvalidObjectProperty { object: u32, property: &'static str },

    #[error("the frame has {channels} PCM channels for {objects} objects; expected {expected}")]
    ChannelObjectCountMismatch {
        channels: usize,
        objects: usize,
        expected: usize,
    },

    #[error("the frame declares {declared} samples but the PCM buffer holds {actual}")]
    FrameLengthMismatch { declared: usize, actual: usize },

    #[error("channel {channel} holds {actual} samples, expected {expected}")]
    UnevenChannelLengths {
        channel: usize,
        expected: usize,
        actual: usize,
    },

    #[error("interleaved PCM length {len} is not divisible by channel count {channels}")]
    InterleavedLengthMismatch { len: usize, channels: usize },

    #[error("the frame has no LFE channel")]
    MissingLfe,

    #[error("object slot {index} has id {id}, expected {expected}")]
    ObjectIdNotContiguous {
        index: usize,
        id: u32,
        expected: u32,
    },

    #[error("an event references object id {object}, which is not in the frame")]
    UnknownEventObject { object: u32 },

    #[error("calibration has an invalid {property}")]
    InvalidCalibration { property: &'static str },

    #[error("trim configuration {index} has an invalid {property}")]
    InvalidTrimRecord {
        index: usize,
        property: &'static str,
    },

    #[error("unsupported spatial payload version {version}; this build supports {supported}")]
    UnsupportedPayloadVersion { version: u16, supported: u16 },
}

/// Failures while encoding or decoding a [`crate::SpatialPayload`].
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PayloadError {
    #[error("spatial payload encode failed: {0}")]
    Encode(String),

    #[error("spatial payload decode failed: {0}")]
    Decode(String),

    #[error("unsupported spatial payload version {version}; this build supports {supported}")]
    UnsupportedVersion { version: u16, supported: u16 },
}
