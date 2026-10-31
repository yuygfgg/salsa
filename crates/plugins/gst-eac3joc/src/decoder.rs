//! `salsaeac3jocdec`: GstAudioDecoder that emits raw 16-channel object audio.

use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;

use eac3joc::{Eac3JocFrame, Eac3JocParser};
use eac3joc_dsp::Eac3JocDecoder;
use objectaudio_decode::{
    EAC3_RATES, object_audio_src_caps, object_audio_template_caps, push_spatial,
};
use objectaudio_ir::SpatialFrame;

/// E-AC-3 decoder delay, in samples, relative to the source timeline.
///
/// This is the only place the E-AC-3 delay is defined. It is added to both the
/// metadata event offsets and the snapshot offset handed to downstream.
pub const EAC3_DECODER_DELAY: usize = 1473;

struct State {
    parser: Eac3JocParser,
    decoder: Option<Box<Eac3JocDecoder>>,
    rate: i32,
    channels: usize,
    caps_set: bool,
    attach_meta: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            parser: Eac3JocParser::new(),
            decoder: None,
            rate: 48_000,
            channels: 0,
            caps_set: false,
            attach_meta: false,
        }
    }
}

#[derive(Default)]
pub struct Eac3JocDec {
    // Boxed: the JOC kernel state is far larger than GLib's 64 KiB private limit.
    state: Mutex<Box<State>>,
}

impl Eac3JocDec {
    fn ensure_output_caps(&self, channels: usize) -> Result<(), gst::FlowError> {
        let rate = {
            let state = self.state.lock().unwrap();
            if state.caps_set {
                if state.channels == channels {
                    return Ok(());
                }
                gst::error!(
                    gst::CAT_RUST,
                    "salsaeac3jocdec: channel count changed from {} to {channels}",
                    state.channels
                );
                return Err(gst::FlowError::NotNegotiated);
            }
            state.rate
        };
        let borrowed = self.obj();
        let object: &super::Eac3JocDec = borrowed.as_ref();
        let attach_meta = objectaudio_decode::negotiate_attach_meta(object);
        let caps = object_audio_src_caps(rate, channels, attach_meta);
        object
            .set_output_caps(&caps)
            .map_err(|_| gst::FlowError::NotNegotiated)?;
        let mut state = self.state.lock().unwrap();
        state.channels = channels;
        state.caps_set = true;
        state.attach_meta = attach_meta;
        Ok(())
    }

    fn drain(&self) -> Result<(), gst::FlowError> {
        let frames = {
            let mut state = self.state.lock().unwrap();
            state.parser.flush().map_err(|error| {
                gst::error!(gst::CAT_RUST, "salsaeac3jocdec: flush failed: {error}");
                gst::FlowError::Error
            })?
        };
        for frame in frames {
            self.decode_and_push(&frame)?;
        }
        Ok(())
    }

    fn decode_and_push(&self, frame: &Eac3JocFrame) -> Result<(), gst::FlowError> {
        let spatial = {
            let mut state = self.state.lock().unwrap();
            let Some(decoder) = state.decoder.as_deref_mut() else {
                return Err(gst::FlowError::NotNegotiated);
            };
            objectaudio_ir::SpatialDecoder::decode(decoder, frame).map_err(|error| {
                gst::error!(
                    gst::CAT_RUST,
                    "salsaeac3jocdec: decode failed at frame {}: {error}",
                    frame.index
                );
                gst::FlowError::Error
            })?
        };
        self.ensure_output_caps(spatial.pcm.channel_count())?;
        self.push_spatial(frame, spatial)
    }

    fn push_spatial(
        &self,
        frame: &Eac3JocFrame,
        spatial: SpatialFrame,
    ) -> Result<(), gst::FlowError> {
        let attach_meta = self.state.lock().unwrap().attach_meta;
        let meta = if attach_meta && let Some(oamd) = &frame.oamd {
            let mut payload = spatial.to_payload();
            let delay = EAC3_DECODER_DELAY as u32;
            for event in &mut payload.events {
                event.offset = event.offset.saturating_add(delay);
            }
            let offset = EAC3_DECODER_DELAY
                + usize::from(frame.oamd_sample_offset)
                + oamd.sample_offset as usize;
            Some((payload, offset))
        } else {
            None
        };
        let borrowed = self.obj();
        let object: &super::Eac3JocDec = borrowed.as_ref();
        push_spatial(object, &spatial, meta.as_ref().map(|(p, o)| (p, *o)), 1)?;
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Eac3JocDec {
    const NAME: &'static str = "GstEac3JocDec";
    type Type = super::Eac3JocDec;
    type ParentType = gst_audio::AudioDecoder;
}

impl ObjectImpl for Eac3JocDec {}

impl GstObjectImpl for Eac3JocDec {}

impl ElementImpl for Eac3JocDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
            std::sync::OnceLock::new();
        Some(METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "E-AC-3 JOC Decoder",
                "Codec/Decoder/Audio",
                "Decodes E-AC-3 JOC into 16-channel object audio",
                "salsa",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> = std::sync::OnceLock::new();
        TEMPLATES.get_or_init(|| {
            let src_caps = object_audio_template_caps(eac3joc_dsp::OUTPUT_CHANNELS, EAC3_RATES);
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &gst::Caps::builder("audio/x-eac3")
                        .field("framed", true)
                        .field("alignment", "frame")
                        .field("rate", gst::IntRange::new(1, 655_350))
                        .build(),
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

impl AudioDecoderImpl for Eac3JocDec {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::Eac3JocDec = borrowed.as_ref();
        object.set_needs_format(true);
        object.set_estimate_rate(true);
        object.set_use_default_pad_acceptcaps(true);
        let sink_pad =
            <super::Eac3JocDec as gst_audio::prelude::AudioDecoderExtManual>::sink_pad(object);
        sink_pad.set_object_flags(gst::ObjectFlags::from_bits_retain(
            gst::PadFlags::ACCEPT_TEMPLATE.bits(),
        ));
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.parser = Eac3JocParser::new();
        state.decoder = None;
        state.channels = 0;
        state.caps_set = false;
        state.attach_meta = false;
        Ok(())
    }

    fn set_format(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let structure = caps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(gst::CAT_RUST, "empty sink caps"))?;
        let rate = structure.get::<i32>("rate").unwrap_or(48_000);
        {
            let mut state = self.state.lock().unwrap();
            state.parser = Eac3JocParser::new();
            state.rate = rate;
            state.channels = 0;
            state.caps_set = false;
            state.decoder = Some(
                std::thread::Builder::new()
                    .name("salsa-joc-init".into())
                    .stack_size(16 * 1024 * 1024)
                    .spawn(|| Eac3JocDecoder::try_new().map(Box::new))
                    .map_err(|error| {
                        gst::loggable_error!(
                            gst::CAT_RUST,
                            "cannot spawn the JOC init thread: {error}"
                        )
                    })?
                    .join()
                    .map_err(|_| {
                        gst::loggable_error!(gst::CAT_RUST, "the JOC init thread panicked")
                    })?
                    .map_err(|error| {
                        gst::loggable_error!(
                            gst::CAT_RUST,
                            "cannot create the JOC decoder: {error}"
                        )
                    })?,
            );
        }
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
        let frames = {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let mut state = self.state.lock().unwrap();
            state.parser.push_bytes(&map).map_err(|error| {
                gst::error!(gst::CAT_RUST, "salsaeac3jocdec: parse failed: {error}");
                gst::FlowError::Error
            })?
        };
        for frame in frames {
            self.decode_and_push(&frame)?;
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if event.type_() == gst::EventType::Eos
            && let Err(error) = self.drain()
        {
            gst::error!(
                gst::CAT_RUST,
                "salsaeac3jocdec: EOS drain failed: {error:?}"
            );
            return false;
        }
        self.parent_sink_event(event)
    }

    fn flush(&self, hard: bool) {
        {
            let mut state = self.state.lock().unwrap();
            state.parser = Eac3JocParser::new();
            if let Some(decoder) = state.decoder.as_deref_mut() {
                decoder.reset();
            }
        }
        self.parent_flush(hard);
    }
}
