//! `salsaspatialdec`: GstAudioDecoder over `salsa_spatial_dsp` object output.

use std::sync::Mutex;

use gst::glib;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;

use objectaudio_decode::{object_audio_src_caps, object_audio_template_caps, push_spatial};
use objectaudio_ir::SpatialParser;
use salsa_spatial::{SalsaSpatialFrame, SalsaSpatialParser, SalsaSpatialStreamConfig};

use crate::backend::{BackendError, Decoder as BackendDecoder};

/// The DSP decoder produces 48 kHz object audio.
const OUTPUT_RATES: &str = "48000";

struct State {
    parser: SalsaSpatialParser,
    decoder: Option<BackendDecoder>,
    config: Option<SalsaSpatialStreamConfig>,
    rate: i32,
    channels: usize,
    caps_set: bool,
    attach_meta: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            parser: SalsaSpatialParser::new(),
            decoder: None,
            config: None,
            rate: salsa_spatial::SAMPLE_RATE as i32,
            channels: 0,
            caps_set: false,
            attach_meta: false,
        }
    }
}

impl State {
    fn reset_stream(&mut self) {
        let binding = self
            .parser
            .reader()
            .binding()
            .cloned()
            .or_else(|| self.config.as_ref().map(|config| config.codec.clone()));
        self.parser = SalsaSpatialParser::new();
        if let Some(binding) = binding {
            self.parser.set_binding(binding);
        }
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.reset();
        }
    }
}

#[derive(Default)]
pub struct SalsaSpatialDec {
    state: Mutex<Box<State>>,
}

impl SalsaSpatialDec {
    fn backend_error(&self, error: BackendError) -> gst::FlowError {
        gst::error!(
            gst::CAT_RUST,
            "salsaspatialdec: salsa-spatial DSP decoder is unavailable: {error}"
        );
        gst::FlowError::Error
    }

    fn ensure_decoder(&self) -> Result<(), gst::FlowError> {
        let mut state = salsa_gst::lock(&self.state);
        if state.decoder.is_some() {
            return Ok(());
        }
        let config = state
            .parser
            .config()
            .cloned()
            .or_else(|| state.config.clone());
        let Some(config) = config else {
            gst::error!(
                gst::CAT_RUST,
                "salsaspatialdec: no stream configuration before the first tile"
            );
            return Err(gst::FlowError::NotNegotiated);
        };
        let decoder =
            BackendDecoder::new(config.clone()).map_err(|error| self.backend_error(error))?;
        state.config = Some(config);
        state.decoder = Some(decoder);
        Ok(())
    }

    fn ensure_output_caps(&self, rate: i32, channels: usize) -> Result<(), gst::FlowError> {
        {
            let state = salsa_gst::lock(&self.state);
            if state.caps_set && state.channels == channels {
                return Ok(());
            }
            if state.caps_set {
                gst::debug!(
                    gst::CAT_RUST,
                    "salsaspatialdec: renegotiating {} -> {channels} channels",
                    state.channels
                );
            }
        }
        let borrowed = self.obj();
        let object: &super::SalsaSpatialDec = borrowed.as_ref();
        let attach_meta = objectaudio_decode::negotiate_attach_meta(object);
        let caps = object_audio_src_caps(rate, channels, attach_meta);
        object
            .set_output_caps(&caps)
            .map_err(|_| gst::FlowError::NotNegotiated)?;
        let mut state = salsa_gst::lock(&self.state);
        state.rate = rate;
        state.channels = channels;
        state.caps_set = true;
        state.attach_meta = attach_meta;
        Ok(())
    }

    fn drain(&self) -> Result<(), gst::FlowError> {
        let frames = {
            let mut state = salsa_gst::lock(&self.state);
            state.parser.flush().map_err(|error| {
                gst::error!(gst::CAT_RUST, "salsaspatialdec: flush failed: {error}");
                gst::FlowError::Error
            })?
        };
        self.push_frames(&frames)?;
        let tail = {
            let mut state = salsa_gst::lock(&self.state);
            state
                .decoder
                .as_mut()
                .map(BackendDecoder::finish)
                .transpose()
                .map_err(|error| self.backend_error(error))?
                .flatten()
        };
        if let Some(tail) = tail {
            self.ensure_output_caps(tail.sample_rate as i32, tail.pcm.channel_count())?;
            if let Err(error) = self.push_spatial(tail, -1)
                && !matches!(error, gst::FlowError::Eos | gst::FlowError::Flushing)
            {
                return Err(error);
            }
        }
        Ok(())
    }

    fn push_frames(&self, frames: &[SalsaSpatialFrame]) -> Result<(), gst::FlowError> {
        salsa_gst::drain_frames(frames, |frame| self.decode_and_push(frame))
    }

    fn decode_and_push(&self, frame: &SalsaSpatialFrame) -> Result<(), gst::FlowError> {
        self.ensure_decoder()?;
        let spatial = {
            let mut state = salsa_gst::lock(&self.state);
            let Some(decoder) = state.decoder.as_mut() else {
                return Err(gst::FlowError::NotNegotiated);
            };
            decoder.decode(frame).map_err(|error| {
                gst::error!(
                    gst::CAT_RUST,
                    "salsaspatialdec: decode failed at tile {}: {error}",
                    frame.start_sample
                );
                gst::FlowError::Error
            })?
        };
        self.ensure_output_caps(spatial.sample_rate as i32, spatial.pcm.channel_count())?;
        self.push_spatial(spatial, 1)
    }

    fn push_spatial(
        &self,
        spatial: objectaudio_ir::SpatialFrame,
        consumed: i32,
    ) -> Result<(), gst::FlowError> {
        let attach_meta = salsa_gst::lock(&self.state).attach_meta;
        let meta = attach_meta.then(|| spatial.to_payload());
        let borrowed = self.obj();
        let object: &super::SalsaSpatialDec = borrowed.as_ref();
        push_spatial(object, &spatial, meta.as_ref(), consumed)?;
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SalsaSpatialDec {
    const NAME: &'static str = "GstSalsaSpatialDec";
    type Type = super::SalsaSpatialDec;
    type ParentType = gst_audio::AudioDecoder;
}

impl ObjectImpl for SalsaSpatialDec {}

impl GstObjectImpl for SalsaSpatialDec {}

impl ElementImpl for SalsaSpatialDec {
    salsa_gst::element_metadata!(
        "salsa-spatial Decoder",
        "Codec/Decoder/Audio",
        "Decodes salsa-spatial Opus multistream into LFE plus source objects"
    );

    salsa_gst::pad_templates!({
        let sink_caps = format!(
            "audio/x-salsa-spatial, framed=(boolean)true, \
             alignment=(string)frame, rate=(int){}, channels=(int){}, \
             object-count=(int)[0,{}]",
            salsa_spatial::SAMPLE_RATE,
            salsa_spatial::BED_CHANNEL_COUNT,
            salsa_spatial::MAX_OBJECTS,
        )
        .parse::<gst::Caps>()
        .unwrap();
        let src_caps = object_audio_template_caps(salsa_spatial::MAX_OBJECTS + 1, OUTPUT_RATES);
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

impl AudioDecoderImpl for SalsaSpatialDec {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::SalsaSpatialDec = borrowed.as_ref();
        salsa_gst::configure_audio_decoder(object);
        // The DSP decoder reports its own alignment from the sample clock, so
        // the carrier timestamps carry no extra signal delay.
        object.set_latency(gst::ClockTime::ZERO, None);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = salsa_gst::lock(&self.state);
        state.reset_stream();
        state.decoder = None;
        state.config = None;
        state.channels = 0;
        state.caps_set = false;
        state.attach_meta = false;
        Ok(())
    }

    fn set_format(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let structure = caps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(gst::CAT_RUST, "empty sink caps"))?;
        let rate = structure
            .get::<i32>("rate")
            .unwrap_or(salsa_spatial::SAMPLE_RATE as i32);
        let mut state = salsa_gst::lock(&self.state);
        state.reset_stream();
        state.decoder = None;
        state.config = None;
        state.rate = rate;
        state.channels = 0;
        state.caps_set = false;
        state.attach_meta = false;
        Ok(())
    }

    fn handle_frame(
        &self,
        buffer: Option<&gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(buffer) = buffer else {
            self.drain()?;
            return Ok(gst::FlowSuccess::Ok);
        };
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            salsa_gst::lock(&self.state).reset_stream();
        }
        let frames = {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let mut state = salsa_gst::lock(&self.state);
            let frames = state.parser.push_bytes(&map).map_err(|error| {
                gst::error!(gst::CAT_RUST, "salsaspatialdec: parse failed: {error}");
                gst::FlowError::Error
            })?;
            if state.config.is_none() {
                state.config = state.parser.config().cloned();
            }
            frames
        };
        self.push_frames(&frames)?;
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if event.type_() == gst::EventType::Eos
            && let Err(error) = self.drain()
        {
            gst::error!(
                gst::CAT_RUST,
                "salsaspatialdec: EOS drain failed: {error:?}"
            );
            return false;
        }
        self.parent_sink_event(event)
    }

    fn flush(&self, hard: bool) {
        salsa_gst::lock(&self.state).reset_stream();
        self.parent_flush(hard);
    }
}
