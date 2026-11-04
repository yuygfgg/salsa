//! Parse stage: TrueHD access units and their raw object-audio payloads.

use objectaudio_ir::SpatialParser;
use truehd::process::extract::{Extractor, Frame as ExtractFrame};
use truehd::process::parse::Parser;
use truehd::structs::access_unit::AccessUnit;
use truehd::utils::errors::ExtractError;

use crate::TrueHdError;

/// OAMD payload bytes carried by an access unit's Evolution frame.
///
/// The payload is the raw ID11 body, the same bytes the E-AC-3 path hands to
/// `objectaudio-oamd`. `evo_sample_offset` is the Evolution frame's own sample
/// shift; it is added to the OAMD block offset when the update is placed on the
/// output timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OamdPayload {
    pub bytes: Vec<u8>,
    pub evo_sample_offset: u32,
}

/// One parsed TrueHD access unit.
///
/// `AccessUnit` is kept opaque to callers: parse and decode stay separate, and
/// the access unit is never serialized through a GStreamer pipeline.
#[derive(Debug)]
pub struct TrueHdFrame {
    pub access_unit: AccessUnit,
    /// Access-unit index within the stream.
    pub index: u64,
    /// Byte offset of the access unit from the start of the pushed stream.
    pub offset: u64,
    /// Raw OAMD payloads carried by this access unit, in stream order.
    pub oamd: Vec<OamdPayload>,
}

/// Streaming TrueHD parser: `Extractor` framing plus `truehd::Parser`.
#[derive(Default)]
pub struct TrueHdParser {
    extractor: Extractor,
    parser: Parser,
    corrupt_frames: usize,
    parsed_frames: usize,
}

impl TrueHdParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of access units the extractor had to skip while resynchronising.
    pub fn corrupt_frames(&self) -> usize {
        self.corrupt_frames
    }

    /// Number of access units that parsed successfully.
    pub fn parsed_frames(&self) -> usize {
        self.parsed_frames
    }

    fn parse_frame(&mut self, frame: &ExtractFrame) -> Result<TrueHdFrame, TrueHdError> {
        let access_unit = self
            .parser
            .parse(frame)
            .map_err(|error| TrueHdError::Parse(error.into()))?;
        self.parsed_frames += 1;
        Ok(TrueHdFrame {
            oamd: oamd_payloads(&access_unit),
            access_unit,
            index: frame.index,
            offset: frame.offset,
        })
    }
}

impl SpatialParser for TrueHdParser {
    type Frame = TrueHdFrame;
    type Error = TrueHdError;

    fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<Self::Frame>, Self::Error> {
        self.extractor.push_bytes(data);
        let mut frames = Vec::new();
        while let Some(item) = self.extractor.next() {
            match item {
                Ok(frame) => frames.push(self.parse_frame(&frame)?),
                Err(ExtractError::InsufficientData) => break,
                Err(_) => {
                    // The extractor resynchronises on the next major sync; a
                    // corrupt access unit is skipped rather than aborting the
                    // whole stream.
                    self.corrupt_frames += 1;
                }
            }
        }
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<Self::Frame>, Self::Error> {
        // The extractor only emits complete access units, so an incomplete tail
        // is dropped. TrueHD has no parser-side delay to drain.
        Ok(Vec::new())
    }
}

/// Parser for access units that have already been framed by a parser element.
///
/// Unlike [`TrueHdParser`], this does not run `truehd::Extractor`; the caller
/// guarantees that `data` is exactly one access unit. The `truehd::Parser`
/// state is kept across calls, so the decoder sees the same state as the
/// unframed path.
#[derive(Default)]
pub struct TrueHdAccessUnitParser {
    parser: Parser,
    parsed_frames: usize,
}

impl TrueHdAccessUnitParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parsed_frames(&self) -> usize {
        self.parsed_frames
    }

    pub fn parse(
        &mut self,
        data: &[u8],
        index: u64,
        offset: u64,
    ) -> Result<TrueHdFrame, TrueHdError> {
        let frame = ExtractFrame {
            timestamp: None,
            data: std::sync::Arc::from(data),
            index,
            offset,
        };
        let access_unit = self
            .parser
            .parse(&frame)
            .map_err(|error| TrueHdError::Parse(error.into()))?;
        self.parsed_frames += 1;
        Ok(TrueHdFrame {
            oamd: oamd_payloads(&access_unit),
            access_unit,
            index,
            offset,
        })
    }
}

/// Extract the raw OAMD payloads (`evo_payload_id == OAMD_PAYLOAD_ID`) from an access unit.
pub fn oamd_payloads(access_unit: &AccessUnit) -> Vec<OamdPayload> {
    let Some(extra_data) = &access_unit.extra_data else {
        return Vec::new();
    };
    let Some(evo_frame) = &extra_data.evo_frame else {
        return Vec::new();
    };

    evo_frame
        .evo_payloads
        .iter()
        .filter(|payload| payload.evo_payload_id == u32::from(objectaudio_oamd::OAMD_PAYLOAD_ID))
        .map(|payload| OamdPayload {
            bytes: payload.evo_payload_byte.clone(),
            evo_sample_offset: payload.evo_payload_config.smploffst.unwrap_or(0),
        })
        .collect()
}
