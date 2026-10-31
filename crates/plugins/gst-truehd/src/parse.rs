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

const DEFAULT_RATE: u32 = 48_000;
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
        let borrowed = self.obj();
        let object: &super::TrueHdParse = borrowed.as_ref();
        <super::TrueHdParse as gst_base::prelude::BaseParseExtManual>::src_pad(object)
            .push_event(gst::event::Caps::new(&src_caps))
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
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
            std::sync::OnceLock::new();
        Some(METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "TrueHD Parser",
                "Codec/Parser/Audio",
                "Frames TrueHD Atmos access units",
                "salsa",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> = std::sync::OnceLock::new();
        TEMPLATES.get_or_init(|| {
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
        })
    }
}

impl BaseParseImpl for TrueHdParse {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::TrueHdParse = borrowed.as_ref();
        object.set_min_frame_size(6);
        **self.state.lock().unwrap() = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        **self.state.lock().unwrap() = State::default();
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
            let mut state = self.state.lock().unwrap();
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

        let mut state = self.state.lock().unwrap();

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
            object
                .finish_frame(frame, size)
                .map_err(|_| gst::FlowError::Error)?;
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
                    object
                        .finish_frame(frame, size)
                        .map_err(|_| gst::FlowError::Error)?;
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
