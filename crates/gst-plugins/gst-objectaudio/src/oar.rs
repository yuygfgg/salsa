//! `salsaoar`: object audio -> standard speaker layout.

use std::sync::{Mutex, OnceLock};

use gst::glib;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

use gst_base::prelude::*;
use gst_base::subclass::base_transform::GenerateOutputSuccess;
use gst_base::subclass::prelude::*;

use objectaudio_render::{
    BLOCK_SAMPLES, SpeakerChannel, SpeakerLayout, SpeakerRenderer, layout_by_channel_count,
    layout_by_name, layouts,
};

const DEFAULT_LAYOUT: &str = "5.1";
/// Renderer-side offset is codec agnostic; the decoder folds any codec delay
/// into the meta offsets it sends.
const DEFAULT_METADATA_OFFSET: u32 = 0;
/// Upper bound advertised to GStreamer. The renderer allocates by object count;
/// this is only a resource boundary, not an API limit.
const MAX_OBJECT_CHANNELS: i32 = 1024;

#[derive(Debug)]
struct State {
    layout: String,
    limiter_enable: bool,
    discard_latency: bool,
    metadata_offset: u32,
    renderer: Option<SpeakerRenderer>,
    runtime: RuntimeState,
    pending: PendingAudio,
    in_bpf: usize,
    out_bpf: usize,
    rate: u32,
    in_channels: usize,
    out_channels: usize,
}

#[derive(Debug, Default)]
struct RuntimeState {
    scene: Option<objectaudio_ir::SceneTimeline>,
    received: i64,
    rendered: i64,
}
impl RuntimeState {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            layout: DEFAULT_LAYOUT.to_string(),
            limiter_enable: false,
            discard_latency: false,
            metadata_offset: DEFAULT_METADATA_OFFSET,
            renderer: None,
            runtime: RuntimeState::default(),
            pending: PendingAudio::default(),
            in_bpf: 0,
            out_bpf: 0,
            rate: 48_000,
            in_channels: 0,
            out_channels: 0,
        }
    }
}

#[derive(Default)]
pub struct Oar {
    state: Mutex<State>,
}

impl Oar {
    fn requested_layout(&self) -> Option<&'static SpeakerLayout> {
        let state = self.state.lock().unwrap();
        layout_by_name(&state.layout)
    }

    fn src_caps(
        &self,
        format: Option<&glib::SendValue>,
        rate: Option<&glib::SendValue>,
    ) -> gst::Caps {
        let requested = self.requested_layout();
        let mut ordered: Vec<&'static SpeakerLayout> = Vec::with_capacity(layouts().len());
        if let Some(requested) = requested {
            ordered.push(requested);
        }
        for layout in layouts() {
            if requested.is_none_or(|candidate| candidate.name != layout.name) {
                ordered.push(layout);
            }
        }

        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            for layout in ordered {
                let positions = layout_positions(layout);
                let mask = gst_audio::AudioChannelPosition::positions_to_mask(&positions, false)
                    .unwrap_or(0);
                let mut structure = gst::Structure::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", gst::IntRange::new(32_000, 48_000))
                    .field("channels", layout.channel_count() as i32)
                    .field("channel-mask", gst::Bitmask(mask))
                    .build();
                if let Some(format) = format {
                    structure.set_value("format", format.to_send_value());
                }
                if let Some(rate) = rate {
                    structure.set_value("rate", rate.to_send_value());
                }
                caps.append_structure(structure);
            }
        }
        caps
    }

    fn sink_caps(format: Option<&glib::SendValue>, rate: Option<&glib::SendValue>) -> gst::Caps {
        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            for features in objectaudio_meta::sink_feature_sets() {
                let mut structure = gst::Structure::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", gst::IntRange::new(32_000, 48_000))
                    .field("channels", gst::IntRange::new(1, MAX_OBJECT_CHANNELS))
                    .field("channel-mask", gst::Bitmask(0))
                    .build();
                if let Some(format) = format {
                    structure.set_value("format", format.to_send_value());
                }
                if let Some(rate) = rate {
                    structure.set_value("rate", rate.to_send_value());
                }
                match features {
                    Some(features) => caps.append_structure_full(structure, Some(features)),
                    None => caps.append_structure(structure),
                }
            }
        }
        caps
    }
}

fn layout_positions(layout: &SpeakerLayout) -> Vec<gst_audio::AudioChannelPosition> {
    layout
        .channels
        .iter()
        .map(|channel| gst_position(*channel))
        .collect()
}

fn gst_position(channel: SpeakerChannel) -> gst_audio::AudioChannelPosition {
    use gst_audio::AudioChannelPosition as P;
    match channel {
        SpeakerChannel::Left => P::FrontLeft,
        SpeakerChannel::Right => P::FrontRight,
        SpeakerChannel::Center => P::FrontCenter,
        SpeakerChannel::Lfe => P::Lfe1,
        SpeakerChannel::LeftSurround => P::SideLeft,
        SpeakerChannel::RightSurround => P::SideRight,
        SpeakerChannel::LeftRearSurround => P::RearLeft,
        SpeakerChannel::RightRearSurround => P::RearRight,
        SpeakerChannel::TopFrontLeft => P::TopFrontLeft,
        SpeakerChannel::TopFrontRight => P::TopFrontRight,
        SpeakerChannel::TopSideLeft => P::TopSideLeft,
        SpeakerChannel::TopSideRight => P::TopSideRight,
        SpeakerChannel::TopBackLeft => P::TopRearLeft,
        SpeakerChannel::TopBackRight => P::TopRearRight,
        SpeakerChannel::LeftWide => P::WideLeft,
        SpeakerChannel::RightWide => P::WideRight,
        SpeakerChannel::Lfe2 => P::Lfe2,
        SpeakerChannel::CenterSurround => P::RearCenter,
        SpeakerChannel::TopSurround => P::TopCenter,
        SpeakerChannel::VerticalHeightLeft => P::TopFrontLeft,
        SpeakerChannel::VerticalHeightCenter => P::TopFrontCenter,
        SpeakerChannel::VerticalHeightRight => P::TopFrontRight,
        SpeakerChannel::TopSurroundLeft => P::TopSurroundLeft,
        SpeakerChannel::TopSurroundRight => P::TopSurroundRight,
        SpeakerChannel::None => P::Invalid,
    }
}

fn layout_from_out_caps(
    caps: &gst::Caps,
    requested: Option<&'static SpeakerLayout>,
) -> Option<&'static SpeakerLayout> {
    let structure = caps.structure(0)?;
    let mask = structure
        .get::<gst::Bitmask>("channel-mask")
        .map(|m| m.0)
        .unwrap_or(0);
    let channels = structure.get::<i32>("channels").unwrap_or(0).max(0) as usize;

    if mask != 0 {
        for layout in layouts() {
            let positions = layout_positions(layout);
            let candidate =
                gst_audio::AudioChannelPosition::positions_to_mask(&positions, false).ok();
            if candidate == Some(mask) {
                return Some(layout);
            }
        }
    }
    if let Some(requested) = requested
        && requested.channel_count() == channels
    {
        return Some(requested);
    }
    layout_by_channel_count(channels)
}

fn copy_io_value(caps: &gst::Caps, field: &str) -> Option<glib::SendValue> {
    caps.structure(0)
        .and_then(|structure| structure.value(field).ok())
        .map(|value| value.to_send_value())
}

#[glib::object_subclass]
impl ObjectSubclass for Oar {
    const NAME: &'static str = "GstOar";
    type Type = super::Oar;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for Oar {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecString::builder("layout")
                    .nick("Speaker layout")
                    .blurb("Target speaker layout, or 'auto' to use the downstream caps")
                    .default_value(DEFAULT_LAYOUT)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("limiter-enable")
                    .nick("Limiter")
                    .blurb("Scale down a rendered buffer whose peak exceeds 0 dBFS")
                    .default_value(false)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("discard-latency")
                    .nick("Discard latency")
                    .blurb("Kept for Dolby oar compatibility; output remains sample-aligned")
                    .default_value(false)
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt::builder("metadata-offset")
                    .nick("Metadata offset")
                    .blurb("Sample offset inside each buffer where the OAMD snapshot is applied")
                    .default_value(DEFAULT_METADATA_OFFSET)
                    .maximum(65_535)
                    .readwrite()
                    .build(),
            ]
        })
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "layout" => {
                let layout = value.get::<String>().unwrap_or_default();
                self.state.lock().unwrap().layout = layout;
            }
            "limiter-enable" => {
                self.state.lock().unwrap().limiter_enable = value.get().unwrap_or(false);
            }
            "discard-latency" => {
                self.state.lock().unwrap().discard_latency = value.get().unwrap_or(false);
            }
            "metadata-offset" => {
                self.state.lock().unwrap().metadata_offset =
                    value.get().unwrap_or(DEFAULT_METADATA_OFFSET);
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let state = self.state.lock().unwrap();
        match pspec.name() {
            "layout" => state.layout.to_value(),
            "limiter-enable" => state.limiter_enable.to_value(),
            "discard-latency" => state.discard_latency.to_value(),
            "metadata-offset" => state.metadata_offset.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Oar {}

impl ElementImpl for Oar {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
        Some(METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "Object Audio Renderer",
                "Audio/Renderer",
                "Renders object audio to a standard speaker layout",
                "salsa",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
        TEMPLATES.get_or_init(|| {
            let sink_caps = Self::sink_caps(None, None);

            let mut src_caps = gst::Caps::new_empty();
            {
                let caps = src_caps.get_mut().unwrap();
                for layout in layouts() {
                    let positions = layout_positions(layout);
                    let mask =
                        gst_audio::AudioChannelPosition::positions_to_mask(&positions, false)
                            .unwrap_or(0);
                    caps.append_structure(
                        gst::Structure::builder("audio/x-raw")
                            .field("format", "F32LE")
                            .field("layout", "interleaved")
                            .field("rate", gst::IntRange::new(32_000, 48_000))
                            .field("channels", layout.channel_count() as i32)
                            .field("channel-mask", gst::Bitmask(mask))
                            .build(),
                    );
                }
            }

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

impl BaseTransformImpl for Oar {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.renderer = None;
        state.runtime = RuntimeState::default();
        state.pending = PendingAudio::default();
        state.in_bpf = 0;
        state.out_bpf = 0;
        state.in_channels = 0;
        state.out_channels = 0;
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.renderer = None;
        state.runtime = RuntimeState::default();
        state.pending = PendingAudio::default();
        Ok(())
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let (format, rate) = (copy_io_value(caps, "format"), copy_io_value(caps, "rate"));
        let mut other = if direction == gst::PadDirection::Src {
            Self::sink_caps(format.as_ref(), rate.as_ref())
        } else {
            self.src_caps(format.as_ref(), rate.as_ref())
        };
        if let Some(filter) = filter {
            other = other.intersect_with_mode(filter, gst::CapsIntersectMode::First);
        }
        Some(other)
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let in_info = gst_audio::AudioInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid salsaoar sink caps"))?;
        let out_info = gst_audio::AudioInfo::from_caps(outcaps)
            .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid salsaoar src caps"))?;
        if in_info.channels() < 1 {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsaoar needs at least the LFE channel, got 0"
            ));
        }
        let object_count = in_info.channels() as usize - 1;
        if in_info.format() != gst_audio::AudioFormat::F32le
            || in_info.layout() != gst_audio::AudioLayout::Interleaved
        {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsaoar needs interleaved F32LE object PCM"
            ));
        }
        if out_info.format() != gst_audio::AudioFormat::F32le
            || out_info.layout() != gst_audio::AudioLayout::Interleaved
        {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsaoar needs interleaved F32LE output"
            ));
        }

        let requested = self.requested_layout();
        let layout = layout_from_out_caps(outcaps, requested).ok_or_else(|| {
            gst::loggable_error!(
                gst::CAT_RUST,
                "unsupported salsaoar output caps: {}",
                outcaps
            )
        })?;
        if layout.channel_count() != out_info.channels() as usize {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "layout {} has {} channels, caps declare {}",
                layout.name,
                layout.channel_count(),
                out_info.channels()
            ));
        }

        let mut state = self.state.lock().unwrap();
        state.renderer = Some(SpeakerRenderer::new(layout, object_count));
        state.runtime.reset();
        state.pending = PendingAudio::default();
        state.in_bpf = in_info.bpf() as usize;
        state.out_bpf = out_info.bpf() as usize;
        state.rate = in_info.rate();
        state.in_channels = in_info.channels() as usize;
        state.out_channels = layout.channel_count();
        gst::info!(
            gst::CAT_RUST,
            "salsaoar: layout {} ({} channels), {object_count} objects",
            layout.name,
            layout.channel_count()
        );
        Ok(())
    }

    fn generate_output(&self) -> Result<GenerateOutputSuccess, gst::FlowError> {
        let Some(inbuf) = self.take_queued_buffer() else {
            return Ok(GenerateOutputSuccess::NoOutput);
        };
        let mut state = self.state.lock().unwrap();
        state.queue(&inbuf)?;
        Ok(match state.render_pending(false)? {
            Some(buffer) => GenerateOutputSuccess::Buffer(buffer),
            None => GenerateOutputSuccess::NoOutput,
        })
    }

    fn query(&self, direction: gst::PadDirection, query: &mut gst::QueryRef) -> bool {
        let handled = BaseTransformImplExt::parent_query(self, direction, query);
        if handled
            && direction == gst::PadDirection::Src
            && let gst::QueryViewMut::Latency(latency) = query.view_mut()
        {
            let (live, min, max) = latency.result();
            let rate = self.state.lock().unwrap().rate;
            let extra = sample_time((BLOCK_SAMPLES - 1) as u64, rate);
            latency.set(live, min + extra, max.map(|value| value + extra));
        }
        handled
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::FlushStop(_) => self.state.lock().unwrap().reset_stream(),
            gst::EventView::Eos(_) | gst::EventView::Segment(_) | gst::EventView::Caps(_) => {
                // Complete the old timeline before forwarding a serialized boundary.
                // A flushing seek has already discarded its pending audio.
                let tail = self.state.lock().unwrap().render_pending(true);
                match tail {
                    Ok(Some(buffer)) => {
                        if self.obj().src_pad().push(buffer).is_err() {
                            return false;
                        }
                    }
                    Ok(None) => (),
                    Err(error) => {
                        gst::element_error!(
                            self.obj(),
                            gst::StreamError::Failed,
                            ("Cannot drain the speaker renderer: {error}")
                        );
                        return false;
                    }
                }
                if matches!(event.view(), gst::EventView::Segment(_)) {
                    self.state.lock().unwrap().reset_stream();
                }
            }
            _ => (),
        }
        self.parent_sink_event(event)
    }
}

/// PCM and spatial events share one sample origin until the next complete block.
#[derive(Debug, Default)]
struct PendingAudio {
    pcm: Vec<f32>,
    pts: Option<gst::ClockTime>,
    offset: Option<u64>,
    emitted: u64,
    discont: bool,
}

impl State {
    fn reset_stream(&mut self) {
        self.pending = PendingAudio::default();
        self.runtime.reset();
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.reset();
        }
    }

    fn queue(&mut self, buffer: &gst::BufferRef) -> Result<(), gst::FlowError> {
        if self.renderer.is_none() || self.in_bpf == 0 {
            return Err(gst::FlowError::NotNegotiated);
        }
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            self.reset_stream();
            self.pending.discont = true;
        }
        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
        if !map.size().is_multiple_of(self.in_bpf) {
            gst::error!(
                gst::CAT_RUST,
                "salsaoar: input contains an incomplete PCM frame"
            );
            return Err(gst::FlowError::Error);
        }
        let buffered = self.pending.pcm.len() / self.in_channels;
        if buffered == 0 {
            self.pending.pts = buffer.pts();
            self.pending.offset = (buffer.offset() != u64::MAX).then_some(buffer.offset());
            self.pending.emitted = 0;
        }
        for snapshot in objectaudio_meta::iter_spatial(buffer) {
            let mut payload = snapshot.payload;
            payload
                .validate_layout(self.rate, self.in_channels)
                .map_err(|error| {
                    gst::error!(gst::CAT_RUST, "invalid speaker layout: {error}");
                    gst::FlowError::Error
                })?;
            payload.rebase(i64::from(self.metadata_offset));
            let result = match &mut self.runtime.scene {
                Some(scene) => scene.submit(&payload, self.runtime.received),
                None => {
                    objectaudio_ir::SceneTimeline::from_payload(&payload, self.runtime.received)
                        .map(|scene| self.runtime.scene = Some(scene))
                }
            };
            result.map_err(|error| {
                gst::error!(gst::CAT_RUST, "invalid speaker metadata: {error}");
                gst::FlowError::Error
            })?;
        }
        self.runtime.received += (map.size() / self.in_bpf) as i64;
        self.pending.pcm.extend(
            map.as_slice()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes)),
        );
        Ok(())
    }

    fn render_pending(&mut self, drain: bool) -> Result<Option<gst::Buffer>, gst::FlowError> {
        if self.pending.pcm.is_empty() {
            return Ok(None);
        }
        let available = self.pending.pcm.len() / self.in_channels;
        let samples = if drain {
            available
        } else {
            available / BLOCK_SAMPLES * BLOCK_SAMPLES
        };
        if samples == 0 {
            return Ok(None);
        }
        let render_samples = samples.next_multiple_of(BLOCK_SAMPLES);
        if drain {
            // Pad only the renderer's final block. Padding is never emitted.
            self.pending
                .pcm
                .resize(render_samples * self.in_channels, 0.0);
        }
        let mut rendered = vec![0.0; render_samples * self.out_channels];
        let renderer = self
            .renderer
            .as_mut()
            .ok_or(gst::FlowError::NotNegotiated)?;
        let input = &self.pending.pcm[..render_samples * self.in_channels];
        let result = match &self.runtime.scene {
            Some(scene) => objectaudio_render::render_scene(
                renderer,
                scene,
                input,
                self.runtime.rendered,
                render_samples,
                &mut rendered,
            ),
            None => renderer.process(input, render_samples, &[], &mut rendered),
        };
        result.map_err(|error| {
            gst::error!(gst::CAT_RUST, "salsaoar: render failed: {error}");
            gst::FlowError::Error
        })?;
        self.runtime.rendered += samples as i64;
        if let Some(scene) = &mut self.runtime.scene {
            scene.discard_before(self.runtime.rendered);
        }
        rendered.truncate(samples * self.out_channels);
        if self.limiter_enable {
            let peak = rendered
                .iter()
                .fold(0.0f64, |peak, value| peak.max(value.abs()));
            if peak > 1.0 {
                let scale = 1.0 / peak;
                for value in &mut rendered {
                    *value *= scale;
                }
            }
        }
        let mut output =
            gst::Buffer::with_size(samples * self.out_bpf).map_err(|_| gst::FlowError::Error)?;
        let outbuf = output.get_mut().unwrap();
        {
            let mut map = outbuf.map_writable().map_err(|_| gst::FlowError::Error)?;
            for (bytes, value) in map
                .as_mut_slice()
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(rendered)
            {
                *bytes = (value as f32).to_le_bytes();
            }
        }
        let start = sample_time(self.pending.emitted, self.rate);
        let end = sample_time(self.pending.emitted + samples as u64, self.rate);
        outbuf.set_pts(self.pending.pts.map(|pts| pts + start));
        outbuf.set_duration(end - start);
        if let Some(offset) = self.pending.offset {
            outbuf.set_offset(offset + self.pending.emitted);
            outbuf.set_offset_end(offset + self.pending.emitted + samples as u64);
        }
        if std::mem::take(&mut self.pending.discont) {
            outbuf.set_flags(gst::BufferFlags::DISCONT);
        }
        self.pending.emitted += samples as u64;
        self.pending.pcm.drain(..samples * self.in_channels);
        if drain {
            self.pending.pcm.clear();
        }
        Ok(Some(output))
    }
}

fn sample_time(samples: u64, rate: u32) -> gst::ClockTime {
    gst::ClockTime::from_nseconds((u128::from(samples) * 1_000_000_000 / u128::from(rate)) as u64)
}
