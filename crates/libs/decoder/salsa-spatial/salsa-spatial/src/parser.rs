//! IAMF-backed parser for complete 100 ms codec tiles.

use objectaudio_ir::SpatialParser;

use crate::error::Result;
use crate::frame::{SalsaSpatialFrame, SalsaSpatialStreamConfig};
use crate::iamf::IamfReader;

/// Stateful parser over the IAMF transport.
#[derive(Debug, Default)]
pub struct SalsaSpatialParser {
    reader: IamfReader,
}

impl SalsaSpatialParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn config(&self) -> Option<&SalsaSpatialStreamConfig> {
        self.reader.config()
    }

    pub fn reader(&self) -> &IamfReader {
        &self.reader
    }
}

impl SpatialParser for SalsaSpatialParser {
    type Frame = SalsaSpatialFrame;
    type Error = crate::error::SpatialError;

    fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<Self::Frame>> {
        self.reader.push_bytes(data)
    }

    fn flush(&mut self) -> Result<Vec<Self::Frame>> {
        self.reader.flush()
    }
}
