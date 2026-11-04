//! Codec-independent pipeline construction.
//!
//! Recovered access units enter through `appsrc`, then the existing Salsa
//! parser/decoder and renderer elements. Rendered PCM leaves through a bounded
//! `appsink`; the service (not this module) owns the physical output scheduler.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSrc};

use crate::queue::BoundedAppSrc;
use gstsalsaeac3joc::{Eac3JocDec, Eac3JocParse};
use gstsalsaobjectaudio::{HrtfRender, Oar};
use gstsalsatruehd::{TrueHdAtmosDec, TrueHdParse};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Eac3,
    TrueHd,
}

impl Codec {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Eac3 => "eac3",
            Self::TrueHd => "truehd",
        }
    }

    pub const fn appsrc_caps(self) -> &'static str {
        match self {
            Self::Eac3 => "audio/x-eac3",
            Self::TrueHd => "audio/x-true-hd",
        }
    }

    pub const fn signal_delay_samples(self) -> u32 {
        match self {
            Self::Eac3 => 1473,
            Self::TrueHd => 0,
        }
    }

    /// Parser lookahead needed before an access unit can be emitted. This is a
    /// scheduling requirement, not a signal delay.
    pub const fn parser_lookahead_samples(self) -> u32 {
        match self {
            Self::Eac3 => 1536,
            Self::TrueHd => 40,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderMode {
    Speaker {
        layout: String,
    },
    Binaural {
        model: PathBuf,
        filterbank: u32,
        profile: String,
    },
}

impl RenderMode {
    pub fn speaker(layout: impl Into<String>) -> Self {
        Self::Speaker {
            layout: layout.into(),
        }
    }

    pub fn binaural(model: impl Into<PathBuf>) -> Self {
        Self::Binaural {
            model: model.into(),
            filterbank: 1,
            profile: "mid".to_string(),
        }
    }

    pub const fn label(&self) -> &'static str {
        match self {
            Self::Speaker { .. } => "speaker",
            Self::Binaural { .. } => "binaural",
        }
    }

    pub const fn is_binaural(&self) -> bool {
        matches!(self, Self::Binaural { .. })
    }
}

pub struct PipelineConfig {
    pub codec: Codec,
    pub render: RenderMode,
    pub live: bool,
}

/// Register the Salsa elements explicitly so the binary does not depend on the
/// host's GStreamer plug-in search path.
pub fn init() -> Result<()> {
    gst::init().context("GStreamer init failed")?;
    objectaudio_meta::init().context("ObjectAudioMeta init failed")?;

    let _ = gst::Element::register(
        None,
        "salsaeac3jocparse",
        gst::Rank::PRIMARY,
        Eac3JocParse::static_type(),
    );
    let _ = gst::Element::register(
        None,
        "salsaeac3jocdec",
        gst::Rank::PRIMARY,
        Eac3JocDec::static_type(),
    );
    let _ = gst::Element::register(
        None,
        "salsatruehdparse",
        gst::Rank::PRIMARY,
        TrueHdParse::static_type(),
    );
    let _ = gst::Element::register(
        None,
        "salsatruehdatmosdec",
        gst::Rank::PRIMARY,
        TrueHdAtmosDec::static_type(),
    );
    let _ = gst::Element::register(None, "salsaoar", gst::Rank::PRIMARY, Oar::static_type());
    let _ = gst::Element::register(
        None,
        "salsahrtfrender",
        gst::Rank::PRIMARY,
        HrtfRender::static_type(),
    );
    Ok(())
}

fn element(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("required element '{factory}' is unavailable"))
}

pub struct DecodeRenderPipeline {
    pipeline: gst::Pipeline,
    appsrc: BoundedAppSrc,
    appsink: AppSink,
    codec: Codec,
}

impl DecodeRenderPipeline {
    pub fn new(config: &PipelineConfig) -> Result<Self> {
        Self::with_stop(config, Arc::default())
    }

    pub(crate) fn with_stop(config: &PipelineConfig, stop: Arc<AtomicBool>) -> Result<Self> {
        if let RenderMode::Binaural { model, .. } = &config.render
            && !model.exists()
        {
            bail!("binaural model {} does not exist", model.display());
        }

        let pipeline = gst::Pipeline::new();
        let appsrc = element("appsrc")?.downcast::<AppSrc>().unwrap();
        appsrc.set_property("format", gst::Format::Time);
        appsrc.set_property("is-live", config.live);
        appsrc.set_property("do-timestamp", false);
        appsrc.set_property("stream-type", gstreamer_app::AppStreamType::Stream);
        let caps: gst::Caps = config.codec.appsrc_caps().parse().unwrap();
        appsrc.set_property("caps", &caps);

        let (parser_name, decoder_name) = match config.codec {
            Codec::Eac3 => ("salsaeac3jocparse", "salsaeac3jocdec"),
            Codec::TrueHd => ("salsatruehdparse", "salsatruehdatmosdec"),
        };
        let parser = element(parser_name)?;
        let decoder = element(decoder_name)?;

        let renderer = match &config.render {
            RenderMode::Speaker { layout } => {
                let renderer = element("salsaoar")?;
                renderer.set_property("layout", layout.as_str());
                renderer
            }
            RenderMode::Binaural {
                model,
                filterbank,
                profile,
            } => {
                let renderer = element("salsahrtfrender")?;
                renderer.set_property(
                    "personalized-headphone",
                    model.to_string_lossy().to_string(),
                );
                renderer.set_property("filterbank", *filterbank);
                renderer.set_property("profile", profile.as_str());
                renderer
            }
        };

        let sink_element = element("appsink")?;
        sink_element.set_property("sync", false);
        sink_element.set_property("max-buffers", 8u32);
        sink_element.set_property("drop", false);
        sink_element.set_property("enable-last-sample", false);
        let appsink = sink_element.clone().downcast::<AppSink>().unwrap();

        pipeline
            .add_many([
                appsrc.upcast_ref(),
                &parser,
                &decoder,
                &renderer,
                &sink_element,
            ])
            .context("cannot add elements to the decode/render pipeline")?;
        gst::Element::link_many([
            appsrc.upcast_ref(),
            &parser,
            &decoder,
            &renderer,
            &sink_element,
        ])
        .context("cannot link the decode/render pipeline")?;

        let appsrc = BoundedAppSrc::new(appsrc, &pipeline, stop, 4 * 1024 * 1024);
        Ok(Self {
            pipeline,
            appsrc,
            appsink,
            codec: config.codec,
        })
    }

    pub const fn codec(&self) -> Codec {
        self.codec
    }

    pub fn appsink(&self) -> &AppSink {
        &self.appsink
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    pub fn play(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the decode/render pipeline")?;
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Push one complete codec access unit derived from the carrier timeline.
    pub fn push_access_unit(
        &mut self,
        bytes: &[u8],
        pts: gst::ClockTime,
        discont: bool,
    ) -> Result<()> {
        let mut buffer = gst::Buffer::from_slice(bytes.to_vec());
        {
            let buffer = buffer.get_mut().expect("fresh buffer is writable");
            buffer.set_pts(Some(pts));
            if discont {
                buffer.set_flags(gst::BufferFlags::DISCONT);
            }
        }
        self.appsrc
            .push(buffer)
            .context("appsrc rejected a codec access unit")
    }

    pub fn end_of_stream(&self) -> Result<()> {
        self.appsrc.end_of_stream().context("appsrc rejected EOS")
    }

    /// Query the latency reported by the renderer through the sink pad. The
    /// returned value includes the renderer's own contribution plus any base
    /// sink latency.
    pub fn queried_latency(&self) -> Option<(bool, gst::ClockTime, Option<gst::ClockTime>)> {
        let pad = self.appsink.static_pad("sink")?;
        let mut query = gst::query::Latency::new();
        if pad.query(&mut query) {
            Some(query.result())
        } else {
            None
        }
    }

    pub fn bus_error(&self) -> Option<(String, String)> {
        let bus = self.pipeline.bus()?;
        while let Some(message) = bus.pop() {
            if let gst::MessageView::Error(error) = message.view() {
                return Some((
                    error.error().to_string(),
                    error.debug().unwrap_or_default().to_string(),
                ));
            }
        }
        None
    }
}

/// Render a raw codec stream directly with the same parser/decoder/renderer
/// configuration. Used by the carrier/direct comparison check.
pub struct DirectPipeline {
    pipeline: gst::Pipeline,
    appsink: AppSink,
    _source: gst::Element,
}

impl DirectPipeline {
    pub fn new(path: impl AsRef<Path>, codec: Codec, render: &RenderMode) -> Result<Self> {
        let path = path.as_ref();
        let source = element("filesrc")?;
        source.set_property("location", path.to_string_lossy().to_string());
        source.set_property("blocksize", 1_048_576u32);
        let (parser_name, decoder_name) = match codec {
            Codec::Eac3 => ("salsaeac3jocparse", "salsaeac3jocdec"),
            Codec::TrueHd => ("salsatruehdparse", "salsatruehdatmosdec"),
        };
        let parser = element(parser_name)?;
        let decoder = element(decoder_name)?;
        let renderer = match render {
            RenderMode::Speaker { layout } => {
                let renderer = element("salsaoar")?;
                renderer.set_property("layout", layout.as_str());
                renderer
            }
            RenderMode::Binaural {
                model,
                filterbank,
                profile,
            } => {
                let renderer = element("salsahrtfrender")?;
                renderer.set_property(
                    "personalized-headphone",
                    model.to_string_lossy().to_string(),
                );
                renderer.set_property("filterbank", *filterbank);
                renderer.set_property("profile", profile.as_str());
                renderer
            }
        };
        let sink_element = element("appsink")?;
        sink_element.set_property("sync", false);
        sink_element.set_property("max-buffers", 8u32);
        sink_element.set_property("drop", false);
        let appsink = sink_element.clone().downcast::<AppSink>().unwrap();
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([&source, &parser, &decoder, &renderer, &sink_element])
            .context("cannot add elements to the direct pipeline")?;
        gst::Element::link_many([&source, &parser, &decoder, &renderer, &sink_element])
            .context("cannot link the direct pipeline")?;
        Ok(Self {
            pipeline,
            appsink,
            _source: source,
        })
    }

    pub fn play(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the direct pipeline")?;
        Ok(())
    }

    pub fn appsink(&self) -> &AppSink {
        &self.appsink
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    pub fn bus_error(&self) -> Option<(String, String)> {
        let bus = self.pipeline.bus()?;
        while let Some(message) = bus.pop() {
            if let gst::MessageView::Error(error) = message.view() {
                return Some((
                    error.error().to_string(),
                    error.debug().unwrap_or_default().to_string(),
                ));
            }
        }
        None
    }
}
