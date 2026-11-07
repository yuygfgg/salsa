//! Adapter around the salsa-spatial DSP decoder.
//!
//! The GStreamer element owns transport/negotiation state; the codec DSP and
//! parameter reconstruction stay in `salsa-spatial-dsp`.

use std::fmt;

use objectaudio_ir::{SpatialDecoder, SpatialFrame};
use salsa_spatial::{SalsaSpatialFrame, SalsaSpatialStreamConfig};
use salsa_spatial_dsp::SalsaSpatialDecoder;

/// Backend construction or decode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BackendError {}

/// One DSP decoder instance.
pub struct Decoder {
    inner: SalsaSpatialDecoder,
}

impl Decoder {
    pub fn new(config: SalsaSpatialStreamConfig) -> Result<Self, BackendError> {
        let inner =
            SalsaSpatialDecoder::new(config).map_err(|error| BackendError(error.to_string()))?;
        Ok(Self { inner })
    }

    pub fn decode(&mut self, frame: &SalsaSpatialFrame) -> Result<SpatialFrame, BackendError> {
        self.inner
            .decode(frame)
            .map_err(|error| BackendError(error.to_string()))
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    pub fn finish(&mut self) -> Result<Option<SpatialFrame>, BackendError> {
        self.inner
            .finish()
            .map_err(|error| BackendError(error.to_string()))
    }
}
