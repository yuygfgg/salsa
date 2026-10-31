//! `salsaoar`: object audio -> standard speaker layout.

use std::sync::{Mutex, OnceLock};

use gst::glib;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

use gst_base::subclass::prelude::*;

use objectaudio_render::{
    BLOCK_SAMPLES, SpeakerChannel, SpeakerLayout, SpeakerMetadata, SpeakerRenderer,
    layout_by_channel_count, layout_by_name, layouts,
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
    in_bpf: usize,
    out_bpf: usize,
    rate: u32,
    in_channels: usize,
    out_channels: usize,
}

#[derive(Debug, Default)]
struct RuntimeState {
    positions_q15: Vec<[u16; 3]>,
    gains: Vec<f64>,
}

impl RuntimeState {
    fn configure(&mut self, object_count: usize) {
        self.positions_q15 = vec![[0; 3]; object_count];
        self.gains = vec![1.0; object_count];
    }

    fn reset(&mut self) {
        self.positions_q15.fill([0; 3]);
        self.gains.fill(1.0);
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
                    .blurb("Kept for Dolby oar compatibility; the speaker renderer has no latency")
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
        state.runtime.configure(object_count);
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

    fn transform_size(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        size: usize,
        othercaps: &gst::Caps,
    ) -> Option<usize> {
        let state = self.state.lock().unwrap();
        let in_bpf = if state.in_bpf != 0 {
            state.in_bpf
        } else {
            gst_audio::AudioInfo::from_caps(caps).ok()?.bpf() as usize
        };
        let out_bpf = if state.out_bpf != 0 {
            state.out_bpf
        } else {
            gst_audio::AudioInfo::from_caps(othercaps).ok()?.bpf() as usize
        };
        if in_bpf == 0 || out_bpf == 0 {
            return None;
        }
        if direction == gst::PadDirection::Sink {
            Some(size / in_bpf * out_bpf)
        } else {
            Some(size / out_bpf * in_bpf)
        }
    }

    fn unit_size(&self, caps: &gst::Caps) -> Option<usize> {
        gst_audio::AudioInfo::from_caps(caps)
            .ok()
            .map(|info| info.bpf() as usize)
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (in_bpf, in_channels, out_bpf, out_channels, rate, metadata_offset, limiter) = {
            let state = self.state.lock().unwrap();
            if state.renderer.is_none() || state.in_bpf == 0 || state.in_channels == 0 {
                return Err(gst::FlowError::NotNegotiated);
            }
            (
                state.in_bpf,
                state.in_channels,
                state.out_bpf,
                state.out_channels,
                state.rate,
                state.metadata_offset,
                state.limiter_enable,
            )
        };

        let inmap = inbuf.map_readable().map_err(|_| gst::FlowError::Error)?;
        let samples = inmap.size() / in_bpf;
        if samples == 0 {
            return Ok(gst::FlowSuccess::Ok);
        }
        if !samples.is_multiple_of(BLOCK_SAMPLES) {
            gst::error!(
                gst::CAT_RUST,
                "salsaoar: input has {samples} samples, not a multiple of {BLOCK_SAMPLES}"
            );
            return Err(gst::FlowError::Error);
        }

        let mut input = Vec::with_capacity(samples * in_channels);
        let (chunks, _) = inmap.as_slice().as_chunks::<4>();
        for chunk in chunks {
            input.push(f32::from_le_bytes(*chunk));
        }
        drop(inmap);

        let events = self.metadata_for_buffer(inbuf, samples, metadata_offset);
        let metadata: Vec<SpeakerMetadata<'_>> = events
            .iter()
            .map(|event| SpeakerMetadata {
                offset: event.offset,
                ramp_samples: event.ramp_samples,
                positions_q15: &event.positions_q15,
                region_indices: None,
                height_enabled: None,
                object_gains: Some(&event.gains),
            })
            .collect();
        let mut rendered = vec![0.0f64; samples * out_channels];
        {
            let mut state = self.state.lock().unwrap();
            let renderer = state
                .renderer
                .as_mut()
                .ok_or(gst::FlowError::NotNegotiated)?;
            renderer
                .process(&input, samples, &metadata, &mut rendered)
                .map_err(|error| {
                    gst::error!(gst::CAT_RUST, "salsaoar: render failed: {error}");
                    gst::FlowError::Error
                })?;
        }

        if limiter {
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

        let out_size = samples * out_bpf;
        if outbuf.size() != out_size {
            outbuf.set_size(out_size);
        }
        let mut outmap = outbuf.map_writable().map_err(|_| gst::FlowError::Error)?;
        for (index, value) in rendered.iter().enumerate() {
            let bytes = (*value as f32).to_le_bytes();
            let start = index * 4;
            outmap[start..start + 4].copy_from_slice(&bytes);
        }
        drop(outmap);

        let duration = gst::ClockTime::from_nseconds(
            (samples as u64).saturating_mul(1_000_000_000) / u64::from(rate.max(1)),
        );
        outbuf.set_pts(inbuf.pts());
        outbuf.set_duration(duration);
        let offset = inbuf.offset();
        outbuf.set_offset(offset);
        outbuf.set_offset_end(offset.saturating_add(samples as u64));
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if event.type_() == gst::EventType::FlushStop
            && let Ok(mut state) = self.state.lock()
        {
            state.runtime.reset();
            if let Some(renderer) = state.renderer.as_mut() {
                renderer.reset();
            }
        }
        self.parent_sink_event(event)
    }
}

struct BufferMetadata {
    offset: u32,
    ramp_samples: u32,
    positions_q15: Vec<[u16; 3]>,
    gains: Vec<f64>,
}
impl Oar {
    fn metadata_for_buffer(
        &self,
        buffer: &gst::BufferRef,
        samples: usize,
        metadata_offset: u32,
    ) -> Vec<BufferMetadata> {
        let Some(snapshot) = objectaudio_meta::get_spatial(buffer) else {
            return Vec::new();
        };

        let mut state = self.state.lock().unwrap();
        let object_count = state.runtime.positions_q15.len();
        let index_for_id = |id: u32| {
            snapshot
                .payload
                .objects
                .iter()
                .position(|object| object.id == id)
        };

        for (index, object) in snapshot
            .payload
            .objects
            .iter()
            .take(object_count)
            .enumerate()
        {
            state.runtime.positions_q15[index] = objectaudio_ir::adm_to_q15(object.position);
            state.runtime.gains[index] = if object.enabled {
                f64::from(object.gain)
            } else {
                0.0
            };
        }

        let clamp_offset =
            |offset: usize| (meta_offset(offset, metadata_offset)).min(samples) as u32;
        let mut output = vec![BufferMetadata {
            offset: clamp_offset(snapshot.offset),
            ramp_samples: snapshot
                .payload
                .events
                .first()
                .map_or(0, |event| event.ramp_samples),
            positions_q15: state.runtime.positions_q15.clone(),
            gains: state.runtime.gains.clone(),
        }];

        for event in &snapshot.payload.events {
            let Some(index) = index_for_id(event.object).filter(|index| *index < object_count)
            else {
                continue;
            };
            state.runtime.positions_q15[index] = objectaudio_ir::adm_to_q15(event.state.position);
            state.runtime.gains[index] = if event.state.enabled {
                f64::from(event.state.gain)
            } else {
                0.0
            };
            output.push(BufferMetadata {
                offset: clamp_offset(event.offset as usize),
                ramp_samples: event.ramp_samples,
                positions_q15: state.runtime.positions_q15.clone(),
                gains: state.runtime.gains.clone(),
            });
        }
        output.sort_by_key(|event| event.offset);
        output
    }
}

fn meta_offset(offset: usize, metadata_offset: u32) -> usize {
    offset.saturating_add(metadata_offset as usize)
}
