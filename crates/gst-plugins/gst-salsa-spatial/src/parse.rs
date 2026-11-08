//! `salsaspatialparse`: GstBaseParse framing for complete salsa-spatial tiles.

use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;

use objectaudio_ir::SpatialParser;
use salsa_spatial::{
    BED_CHANNEL_COUNT, MAX_OBJECTS, SAMPLE_RATE, SalsaSpatialFrame, SalsaSpatialParser,
    TILE_SAMPLES,
};

use crate::framing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapsInfo {
    rate: u32,
    object_count: usize,
}

struct State {
    parser: SalsaSpatialParser,
    caps: Option<CapsInfo>,
    sink_object_count: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            parser: SalsaSpatialParser::new(),
            caps: None,
            sink_object_count: 0,
        }
    }
}

impl State {
    fn reset(&mut self) {
        self.parser = SalsaSpatialParser::new();
        self.caps = None;
    }

    fn reset_parser(&mut self) {
        self.parser = SalsaSpatialParser::new();
    }
}

#[derive(Default)]
pub struct SalsaSpatialParse {
    state: Mutex<Box<State>>,
}

impl SalsaSpatialParse {
    fn push_src_caps(&self, info: CapsInfo) -> bool {
        let src_caps = gst::Caps::builder("audio/x-salsa-spatial")
            .field("framed", true)
            .field("alignment", "frame")
            .field("rate", info.rate as i32)
            .field("channels", BED_CHANNEL_COUNT as i32)
            .field("object-count", info.object_count as i32)
            .build();
        salsa_gst::push_parse_caps!(self, src_caps)
    }

    fn frame_info(&self, frames: Option<&SalsaSpatialFrame>, default_objects: usize) -> CapsInfo {
        let rate = frames
            .map(|frame| frame.sample_rate)
            .filter(|rate| *rate > 0)
            .unwrap_or(SAMPLE_RATE);
        let object_count = if default_objects > 0 {
            default_objects
        } else {
            frames.map(|frame| frame.objects.len()).unwrap_or_default()
        };
        CapsInfo { rate, object_count }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SalsaSpatialParse {
    const NAME: &'static str = "GstSalsaSpatialParse";
    type Type = super::SalsaSpatialParse;
    type ParentType = gst_base::BaseParse;
}

impl ObjectImpl for SalsaSpatialParse {}

impl GstObjectImpl for SalsaSpatialParse {}

impl ElementImpl for SalsaSpatialParse {
    salsa_gst::element_metadata!(
        "salsa-spatial Parser",
        "Codec/Parser/Audio",
        "Frames complete salsa-spatial Opus multistream tiles carrying object audio"
    );

    salsa_gst::pad_templates!({
        let sink_caps: gst::Caps = format!(
            "audio/x-salsa-spatial, rate=(int){SAMPLE_RATE}, \
             channels=(int){BED_CHANNEL_COUNT}, object-count=(int)[0,{MAX_OBJECTS}]"
        )
        .parse()
        .unwrap();
        let src_caps: gst::Caps = format!(
            "audio/x-salsa-spatial, framed=(boolean)true, \
             alignment=(string)frame, rate=(int){SAMPLE_RATE}, \
             channels=(int){BED_CHANNEL_COUNT}, object-count=(int)[0,{MAX_OBJECTS}]"
        )
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

impl BaseParseImpl for SalsaSpatialParse {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::SalsaSpatialParse = borrowed.as_ref();
        object.set_min_frame_size(1);
        object.set_has_timing_info(true);
        salsa_gst::lock(&self.state).reset();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        salsa_gst::lock(&self.state).reset();
        Ok(())
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let structure = caps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(gst::CAT_RUST, "empty sink caps"))?;
        if structure.name() != "audio/x-salsa-spatial" {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsaspatialparse requires audio/x-salsa-spatial"
            ));
        }
        let object_count = structure
            .get::<i32>("object-count")
            .ok()
            .filter(|count| *count >= 0)
            .map_or(0, |count| (count as usize).min(MAX_OBJECTS));
        let mut state = salsa_gst::lock(&self.state);
        state.reset();
        state.sink_object_count = object_count;
        Ok(())
    }

    fn handle_frame(
        &self,
        frame: gst_base::BaseParseFrame,
    ) -> Result<(gst::FlowSuccess, u32), gst::FlowError> {
        let Some(buffer) = frame.buffer() else {
            return Ok((gst::FlowSuccess::Ok, 0));
        };
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            // Keep the already negotiated stream caps across a seek; only the
            // transport parser state is dropped.
            salsa_gst::lock(&self.state).reset_parser();
        }
        let borrowed = self.obj();
        let object: &super::SalsaSpatialParse = borrowed.as_ref();
        let draining = object.is_draining();
        let (size, parsed, info, changed, has_audio) = {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            if map.is_empty() {
                return Ok((gst::FlowSuccess::Ok, 0));
            }
            let size = match framing::tile_bytes(&map) {
                Some(size) => size,
                // BaseParse calls us with the remaining bytes while draining,
                // so a short final tile is emitted here rather than dropped.
                None if draining => map.len(),
                None => return Ok((gst::FlowSuccess::Ok, 0)),
            };
            if size == 0 || size > map.len() {
                return Ok((gst::FlowSuccess::Ok, 0));
            }

            let mut state = salsa_gst::lock(&self.state);
            let parsed = state.parser.push_bytes(&map[..size]).map_err(|error| {
                gst::error!(
                    gst::CAT_RUST,
                    "salsaspatialparse: parse failed at {}: {error}",
                    frame.offset()
                );
                gst::FlowError::Error
            })?;
            let default_objects = state
                .parser
                .config()
                .map(|config| config.object_count)
                .filter(|count| *count > 0)
                .unwrap_or(state.sink_object_count);
            let info = self.frame_info(parsed.first(), default_objects);
            let changed = state.caps != Some(info);
            if changed {
                state.caps = Some(info);
            }
            let has_audio = framing::has_audio_page(&map[..size]);
            (size, parsed, info, changed, has_audio)
        };
        if parsed.is_empty() && !has_audio {
            // Header-only or end-of-stream Ogg pages carry no complete tile.
            // Consume them without producing an empty output buffer.
            return Ok((gst::FlowSuccess::Ok, size as u32));
        }
        if changed && !self.push_src_caps(info) {
            return Err(gst::FlowError::NotNegotiated);
        }

        if info.rate > 0 {
            let samples = parsed
                .first()
                .map(|frame| frame.frames)
                .filter(|frames| *frames > 0)
                .unwrap_or(TILE_SAMPLES);
            object.set_frame_rate(gst::Fraction::new(info.rate as i32, samples as i32), 0, 0);
        }

        object.finish_frame(frame, size as u32)?;
        Ok((gst::FlowSuccess::Ok, 0))
    }
}
