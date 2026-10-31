//! Audio-frame grouping and the codec-specific frame exposed by the parser.

use objectaudio_oamd::{OamdFrame, OamdLimits};

use crate::bed::{BedPlan, compute_bed_plan, compute_bed_plan_for_config, is_core_stream};
use crate::emdf;
use crate::error::{Eac3JocError, Result};
use crate::joc::{self, JocParams};
use crate::transport::{FrameReader, SyncFrame, SyncFrameHeader};

/// One E-AC-3 audio frame: every syncframe up to the next repeated
/// `(codec, strmtyp, substreamid)` combination.
#[derive(Debug, Clone)]
pub struct Eac3JocFrame {
    pub index: u64,
    pub sample_rate: u32,
    pub samples: usize,
    /// The independent substream header, when present.
    pub core_header: Option<SyncFrameHeader>,
    /// The first dependent substream header, when present.
    pub dependent_header: Option<SyncFrameHeader>,
    pub syncframes: Vec<SyncFrame>,
    pub joc: Option<Box<JocParams>>,
    pub oamd: Option<OamdFrame>,
    /// Raw ID11 payload, kept for the GStreamer meta.
    pub oamd_payload: Option<Vec<u8>>,
    /// EMDF payload sample offset that accompanies the OAMD payload.
    pub oamd_sample_offset: u16,
    pub bed_plan: Option<BedPlan>,
}

impl Eac3JocFrame {
    /// Concatenated syncframes, the packet shape an E-AC-3 decoder expects.
    pub fn core_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.syncframes.iter().map(|f| f.bytes.len()).sum());
        for frame in &self.syncframes {
            bytes.extend_from_slice(&frame.bytes);
        }
        bytes
    }

    pub fn has_joc(&self) -> bool {
        self.joc.is_some()
    }
}

/// Incremental parse -> `Eac3JocFrame`.
#[derive(Debug, Default)]
pub struct Eac3JocParser {
    reader: FrameReader,
    pending: Vec<SyncFrame>,
    pending_joc: Option<Box<JocParams>>,
    pending_oamd: Option<OamdFrame>,
    pending_oamd_payload: Option<Vec<u8>>,
    pending_oamd_offset: u16,
    last_bed_plan: Option<BedPlan>,
    last_sample_rate: u32,
    next_index: u64,
    finished: bool,
}

impl Eac3JocParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<Eac3JocFrame>> {
        self.reader.push(data);
        self.drain()
    }

    pub fn flush(&mut self) -> Result<Vec<Eac3JocFrame>> {
        self.reader.finish();
        self.finished = true;
        let mut frames = self.drain()?;
        if let Some(frame) = self.finish_pending()? {
            frames.push(frame);
        }
        Ok(frames)
    }

    fn drain(&mut self) -> Result<Vec<Eac3JocFrame>> {
        let mut completed = Vec::new();
        while let Some(frame) = self.reader.next_frame()? {
            let repeats = self
                .pending
                .iter()
                .any(|prior| prior.header.same_stream(&frame.header));
            if repeats && let Some(group) = self.finish_pending()? {
                completed.push(group);
            }
            self.accept(frame)?;
        }
        Ok(completed)
    }

    fn accept(&mut self, frame: SyncFrame) -> Result<()> {
        if let Some(container) = emdf::find_joc_emdf_optional(
            &frame.bytes,
            Some(&|candidate| joc::score_container_with_frame(&frame.bytes, candidate)),
        )? {
            let joc_payload = container.find(emdf::ID_JOC).ok_or_else(|| {
                Eac3JocError::EmdfTransport("EMDF container has no ID14 payload".into())
            })?;
            let params = joc::parse_id14(&emdf::extract_payload_bytes(&frame.bytes, joc_payload)?)?;
            self.pending_joc = Some(Box::new(params));
            if let Some(oamd_payload) = container.find(emdf::ID_OAMD) {
                let bytes = emdf::extract_payload_bytes(&frame.bytes, oamd_payload)?;
                let oamd = objectaudio_oamd::parse_with_limits(&bytes, OamdLimits::default())?;
                self.pending_oamd = Some(oamd);
                self.pending_oamd_payload = Some(bytes);
                self.pending_oamd_offset = oamd_payload.sample_offset;
            }
        }
        self.pending.push(frame);
        Ok(())
    }

    fn finish_pending(&mut self) -> Result<Option<Eac3JocFrame>> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let syncframes = std::mem::take(&mut self.pending);
        let joc = self.pending_joc.take();
        let oamd = self.pending_oamd.take();
        let oamd_payload = self.pending_oamd_payload.take();
        let oamd_sample_offset = std::mem::take(&mut self.pending_oamd_offset);

        let core_header = syncframes
            .iter()
            .map(|frame| frame.header)
            .find(is_core_stream);
        let dependent_header = syncframes
            .iter()
            .map(|frame| frame.header)
            .find(|header| !is_core_stream(header));
        let sample_rate = core_header
            .or(dependent_header)
            .map(|header| header.sample_rate)
            .unwrap_or(self.last_sample_rate);
        if sample_rate != 0 {
            self.last_sample_rate = sample_rate;
        }
        let samples = core_header
            .map(|header| header.samples())
            .unwrap_or(crate::joc::FRAME_SAMPLES);

        let bed_plan = if let Some(params) = &joc {
            let plan = compute_bed_plan(
                core_header.as_ref(),
                dependent_header.as_ref(),
                params.dmx_config_idx,
            )
            .or_else(|_| compute_bed_plan_for_config(params.dmx_config_idx))?;
            self.last_bed_plan = Some(plan);
            Some(plan)
        } else {
            self.last_bed_plan
        };

        let frame = Eac3JocFrame {
            index: self.next_index,
            sample_rate,
            samples,
            core_header,
            dependent_header,
            syncframes,
            joc,
            oamd,
            oamd_payload,
            oamd_sample_offset,
            bed_plan,
        };
        self.next_index += 1;
        Ok(Some(frame))
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}
