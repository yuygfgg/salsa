use crate::SpatialFrame;

/// Transport/bitstream parser. It never touches PCM or DSP.
pub trait SpatialParser {
    type Frame;
    type Error: std::error::Error + Send + Sync + 'static;

    fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<Self::Frame>, Self::Error>;

    fn flush(&mut self) -> Result<Vec<Self::Frame>, Self::Error>;
}

/// Codec decoder. It consumes a parsed frame and produces the shared IR.
pub trait SpatialDecoder {
    type Frame;
    type Error: std::error::Error + Send + Sync + 'static;

    fn decode(&mut self, frame: &Self::Frame) -> Result<SpatialFrame, Self::Error>;
}
