//! `salsaeac3jocparse`: GstBaseParse syncframe framing for E-AC-3 JOC.

use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;

use eac3joc::{Codec, Eac3JocError, FrameReader, SyncFrameHeader};

fn channel_count(header: &SyncFrameHeader) -> i32 {
    let mut positions = Vec::new();
    eac3joc::acmod_positions(header.acmod, &mut positions);
    if let Some(chanmap) = header.chanmap {
        let mut extra = Vec::new();
        eac3joc::chanmap_extra_pair(&positions, chanmap, &mut extra);
        positions.extend(extra);
    }
    (positions.len() + usize::from(header.lfeon)).min(8) as i32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapsInfo {
    channels: i32,
    rate: u32,
    atmos: bool,
}

#[derive(Debug, Default)]
pub struct Eac3JocParse {
    caps: Mutex<Option<CapsInfo>>,
}

impl Eac3JocParse {
    fn push_src_caps(&self, info: CapsInfo) -> bool {
        let src_caps = gst::Caps::builder("audio/x-eac3")
            .field("framed", true)
            .field("alignment", "frame")
            .field("rate", info.rate as i32)
            .field("channels", info.channels)
            .field("atmos", info.atmos)
            .build();
        let borrowed = self.obj();
        let object: &super::Eac3JocParse = borrowed.as_ref();
        <super::Eac3JocParse as gst_base::prelude::BaseParseExtManual>::src_pad(object)
            .push_event(gst::event::Caps::new(&src_caps))
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Eac3JocParse {
    const NAME: &'static str = "GstEac3JocParse";
    type Type = super::Eac3JocParse;
    type ParentType = gst_base::BaseParse;
}

impl ObjectImpl for Eac3JocParse {}

impl GstObjectImpl for Eac3JocParse {}

impl ElementImpl for Eac3JocParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
            std::sync::OnceLock::new();
        Some(METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "E-AC-3 JOC Parser",
                "Codec/Parser/Audio",
                "Frames E-AC-3 JOC syncframes carrying object audio",
                "salsa",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> = std::sync::OnceLock::new();
        TEMPLATES.get_or_init(|| {
            let max_channels = 8;
            let src_caps: gst::Caps = format!(
                "audio/x-eac3, framed=(boolean)true, alignment=(string)frame, \
                 rate=(int)[8000,48000], channels=(int)[1,{max_channels}], \
                 atmos=(boolean){{false,true}}"
            )
            .parse()
            .unwrap();
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &gst::Caps::builder("audio/x-eac3").build(),
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

impl BaseParseImpl for Eac3JocParse {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::Eac3JocParse = borrowed.as_ref();
        object.set_min_frame_size(6);
        self.caps.lock().unwrap().take();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.caps.lock().unwrap().take();
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
        enum Action {
            Frame {
                size: usize,
                samples: usize,
                info: CapsInfo,
            },
            NeedMore,
            Skip(usize),
        }
        let action = {
            let Some(buffer) = frame.buffer() else {
                return Ok((gst::FlowSuccess::Ok, 0));
            };
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            match FrameReader::frame_bytes(&map, 0) {
                Ok(size) if map.len() < size => Action::NeedMore,
                Ok(size) => {
                    let Some(header) = eac3joc::parse_syncframe_header(&map) else {
                        return Ok((gst::FlowSuccess::Ok, 1));
                    };
                    if header.codec != Codec::Eac3 {
                        return Ok((gst::FlowSuccess::Ok, 1));
                    }
                    let atmos = eac3joc::parse_syncframe_optional(&map[..size])
                        .ok()
                        .flatten()
                        .is_some();
                    Action::Frame {
                        size,
                        samples: header.samples(),
                        info: CapsInfo {
                            channels: channel_count(&header),
                            rate: header.sample_rate,
                            atmos,
                        },
                    }
                }
                Err(Eac3JocError::TruncatedFrame { .. } | Eac3JocError::TruncatedHeader) => {
                    Action::NeedMore
                }
                Err(_) => Action::Skip(1),
            }
        };
        match action {
            Action::NeedMore => Ok((gst::FlowSuccess::Ok, 0)),
            Action::Skip(bytes) => Ok((gst::FlowSuccess::Ok, bytes as u32)),
            Action::Frame {
                size,
                samples,
                info,
            } => {
                let caps_to_push = {
                    let mut caps = self.caps.lock().unwrap();
                    match caps.as_ref() {
                        None => {
                            *caps = Some(info);
                            Some(info)
                        }
                        Some(previous)
                            if previous.rate != info.rate || previous.atmos != info.atmos =>
                        {
                            let next = CapsInfo {
                                channels: previous.channels,
                                ..info
                            };
                            *caps = Some(next);
                            Some(next)
                        }
                        Some(_) => None,
                    }
                };
                if let Some(info) = caps_to_push
                    && !self.push_src_caps(info)
                {
                    return Err(gst::FlowError::NotNegotiated);
                }
                let borrowed = self.obj();
                let object: &super::Eac3JocParse = borrowed.as_ref();
                if info.rate > 0 && samples > 0 {
                    object.set_frame_rate(
                        gst::Fraction::new(info.rate as i32, samples as i32),
                        0,
                        0,
                    );
                }
                // Propagate FLUSHING so a seek racing with the downstream
                // decoder stops the parser task instead of posting an error.
                object.finish_frame(frame, size as u32)?;
                Ok((gst::FlowSuccess::Ok, 0))
            }
        }
    }
}
