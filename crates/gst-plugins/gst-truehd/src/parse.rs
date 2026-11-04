//! `salsatruehdparse`: GstBaseParse access-unit framing for TrueHD.

use std::sync::{Arc, Mutex};

use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;

use truehd::process::extract::Extractor;
use truehd::process::parse::Parser as AccessUnitParser;
use truehd::structs::access_unit::AccessUnit;
use truehd::utils::errors::ExtractError;

const DEFAULT_RATE: u32 = salsa_codec::SAMPLE_RATE;
const DEFAULT_SAMPLES: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapsInfo {
    rate: u32,
    samples: usize,
    atmos: bool,
}

struct Pending {
    bytes: Arc<[u8]>,
    offset: u64,
    info: CapsInfo,
}

struct State {
    extractor: Extractor,
    parser: AccessUnitParser,
    /// Stream byte offset that `pushed_until` and `base_consumed` are relative to.
    stream_offset: u64,
    pushed_until: u64,
    base_consumed: u64,
    pending: Option<Pending>,
    caps: Option<CapsInfo>,
    rate: u32,
    samples: usize,
    atmos: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            extractor: Extractor::default(),
            parser: AccessUnitParser::default(),
            stream_offset: 0,
            pushed_until: 0,
            base_consumed: 0,
            pending: None,
            caps: None,
            rate: DEFAULT_RATE,
            samples: DEFAULT_SAMPLES,
            atmos: false,
        }
    }
}

impl State {
    /// Discard all transport state and re-anchor to `offset`.
    ///
    /// `offset` is the absolute stream position of the first byte BaseParse
    /// will hand to the next `handle_frame` call. Keeping the counters
    /// relative to it lets the same code work across backward and forward
    /// seeks without tracking absolute extractor offsets.
    fn reset_transport(&mut self, offset: u64) {
        self.extractor = Extractor::default();
        self.parser = AccessUnitParser::default();
        self.stream_offset = offset;
        self.pushed_until = 0;
        self.base_consumed = 0;
        self.pending = None;
    }
}

fn access_unit_info(state: &mut State, access_unit: &AccessUnit) -> CapsInfo {
    if let Some(major_sync) = &access_unit.major_sync_info {
        if let Ok(rate) = major_sync.format_info.sampling_frequency_1() {
            state.rate = rate;
        }
        if let Ok(samples) = major_sync.format_info.samples_per_au() {
            state.samples = samples;
        }
    }
    if !truehd_atmos::oamd_payloads(access_unit).is_empty() {
        state.atmos = true;
    }
    CapsInfo {
        rate: state.rate,
        samples: state.samples.max(1),
        atmos: state.atmos,
    }
}

#[derive(Default)]
pub struct TrueHdParse {
    state: Mutex<Box<State>>,
}

impl TrueHdParse {
    fn push_src_caps(&self, info: CapsInfo) -> bool {
        let src_caps = gst::Caps::builder("audio/x-true-hd")
            .field("framed", true)
            .field("alignment", "frame")
            .field("rate", info.rate as i32)
            .field("atmos", info.atmos)
            .build();
        salsa_gst::push_parse_caps!(self, src_caps)
    }

    fn update_caps(&self, state: &mut State, info: CapsInfo) -> Option<CapsInfo> {
        if state.caps == Some(info) {
            return None;
        }
        state.caps = Some(info);
        Some(info)
    }

    fn set_frame_rate(&self, info: CapsInfo) {
        let borrowed = self.obj();
        let object: &super::TrueHdParse = borrowed.as_ref();
        object.set_frame_rate(
            gst::Fraction::new(info.rate as i32, info.samples as i32),
            0,
            0,
        );
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TrueHdParse {
    const NAME: &'static str = "GstSalsaTrueHdParse";
    type Type = super::TrueHdParse;
    type ParentType = gst_base::BaseParse;
}

impl ObjectImpl for TrueHdParse {}

impl GstObjectImpl for TrueHdParse {}

impl ElementImpl for TrueHdParse {
    salsa_gst::element_metadata!(
        "TrueHD Parser",
        "Codec/Parser/Audio",
        "Frames TrueHD Atmos access units"
    );

    salsa_gst::pad_templates!({
        let sink_caps: gst::Caps = "audio/x-true-hd; audio/x-truehd".parse().unwrap();
        let src_caps: gst::Caps =
            "audio/x-true-hd, framed=(boolean)true, alignment=(string)frame, \
     rate=(int){44100,48000,88200,96000,176400,192000}, \
     atmos=(boolean){false,true}"
                .parse()
                .unwrap();
        vec![
            gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap(),
            gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap(),
        ]
    });
}

impl BaseParseImpl for TrueHdParse {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::TrueHdParse = borrowed.as_ref();
        object.set_min_frame_size(6);
        **salsa_gst::lock(&self.state) = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        **salsa_gst::lock(&self.state) = State::default();
        Ok(())
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        if caps.structure(0).is_none() {
            return Err(gst::loggable_error!(gst::CAT_RUST, "empty sink caps"));
        }
        Ok(())
    }

    fn handle_frame(
        &self,
        frame: gst_base::BaseParseFrame,
    ) -> Result<(gst::FlowSuccess, u32), gst::FlowError> {
        let end = {
            let Some(buffer) = frame.buffer() else {
                return Ok((gst::FlowSuccess::Ok, 0));
            };
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            // A DISCONT buffer (flushing or non-flushing seek) and a BaseParse
            // stream offset that does not match the extractor's anchor both
            // mean the old framing state is invalid: drop it and resynchronise
            // on the next major sync.
            let discont = buffer.flags().contains(gst::BufferFlags::DISCONT);
            let mut state = salsa_gst::lock(&self.state);
            let expected = state.stream_offset.saturating_add(state.base_consumed);
            if discont || frame.offset() != expected {
                state.reset_transport(frame.offset());
            }
            let start = state.base_consumed;
            let end = start.saturating_add(map.len() as u64);
            let push_from = state.pushed_until.max(start);
            if push_from < end {
                let from = (push_from - start) as usize;
                state.extractor.push_bytes(&map[from..]);
                state.pushed_until = end;
            }
            end
        };

        let mut state = salsa_gst::lock(&self.state);

        if let Some(pending) = state.pending.take() {
            if pending.offset > state.base_consumed {
                let skip = (pending.offset - state.base_consumed) as u32;
                state.base_consumed += u64::from(skip);
                state.pending = Some(pending);
                return Ok((gst::FlowSuccess::Ok, skip));
            }
            if pending.offset < state.base_consumed {
                return Ok((gst::FlowSuccess::Ok, 0));
            }
            let size = pending.bytes.len() as u32;
            if u64::from(size) > end.saturating_sub(state.base_consumed) {
                state.pending = Some(pending);
                return Ok((gst::FlowSuccess::Ok, 0));
            }
            state.base_consumed += u64::from(size);
            let caps = self.update_caps(&mut state, pending.info);
            drop(state);
            if let Some(info) = caps
                && !self.push_src_caps(info)
            {
                return Err(gst::FlowError::NotNegotiated);
            }
            self.set_frame_rate(pending.info);
            let borrowed = self.obj();
            let object: &super::TrueHdParse = borrowed.as_ref();
            object.finish_frame(frame, size)?;
            return Ok((gst::FlowSuccess::Ok, 0));
        }

        loop {
            match state.extractor.next() {
                Some(Ok(access_unit)) => {
                    let info = match state.parser.parse(&access_unit) {
                        Ok(parsed) => access_unit_info(&mut state, &parsed),
                        Err(_) => CapsInfo {
                            rate: state.rate,
                            samples: state.samples.max(1),
                            atmos: state.atmos,
                        },
                    };
                    let pending = Pending {
                        offset: access_unit.offset,
                        bytes: access_unit.data.clone(),
                        info,
                    };
                    if pending.offset > state.base_consumed {
                        let skip = (pending.offset - state.base_consumed) as u32;
                        state.base_consumed += u64::from(skip);
                        state.pending = Some(pending);
                        return Ok((gst::FlowSuccess::Ok, skip));
                    }
                    let size = pending.bytes.len() as u32;
                    state.base_consumed += u64::from(size);
                    let caps = self.update_caps(&mut state, pending.info);
                    drop(state);
                    if let Some(info) = caps
                        && !self.push_src_caps(info)
                    {
                        return Err(gst::FlowError::NotNegotiated);
                    }
                    self.set_frame_rate(pending.info);
                    let borrowed = self.obj();
                    let object: &super::TrueHdParse = borrowed.as_ref();
                    object.finish_frame(frame, size)?;
                    return Ok((gst::FlowSuccess::Ok, 0));
                }
                Some(Err(ExtractError::InsufficientData)) | None => {
                    return Ok((gst::FlowSuccess::Ok, 0));
                }
                Some(Err(_)) => continue,
            }
        }
    }
}
