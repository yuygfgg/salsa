//! `salsahrtfrender`: Rosella personalized-headphone binaural renderer.

use std::sync::{Mutex, OnceLock};

use gst::glib;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

use gst_base::prelude::BaseTransformExtManual;
use gst_base::subclass::prelude::*;

use objectaudio_hrtf::{
    FRAME_SAMPLES, FilterbankKind, RosellaProfile, RosellaRenderOptions, RosellaRenderer,
    load_personalized_headphone,
};

const OUTPUT_CHANNELS: usize = 2;
const SAMPLE_RATE: u32 = salsa_codec::SAMPLE_RATE;
/// The renderer is codec agnostic; decoders fold the codec delay into the
/// metadata offsets they attach.
const DEFAULT_OBJECT_DELAY: u32 = 0;
/// Resource boundary for negotiated object channels; not an API limit.
const MAX_OBJECT_CHANNELS: i32 = 1024;

struct State {
    model_path: String,
    filterbank: FilterbankKind,
    profile: RosellaProfile,
    object_delay: u32,
    tail_seconds: f64,
    output_gain: f64,
    chunk_frames: u32,
    room_impulse_slots: u32,
    renderer: Option<RosellaRenderer>,
    in_bpf: usize,
    in_channels: usize,
    object_count: usize,
    out_bpf: usize,
    rate: u32,
    pending: Vec<f32>,
    received_samples: i64,
    available: Vec<f64>,
    next_pts: Option<gst::ClockTime>,
    total_out_samples: u64,
    tail_sent: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            model_path: String::new(),
            filterbank: FilterbankKind::Public,
            profile: RosellaProfile::Mid,
            object_delay: DEFAULT_OBJECT_DELAY,
            tail_seconds: 5.0,
            output_gain: 1.0,
            chunk_frames: 1,
            room_impulse_slots: 4096,
            renderer: None,
            in_bpf: 0,
            in_channels: 0,
            object_count: 0,
            out_bpf: 0,
            rate: SAMPLE_RATE,
            pending: Vec::new(),
            received_samples: 0,
            available: Vec::new(),
            next_pts: None,
            total_out_samples: 0,
            tail_sent: false,
        }
    }
}

impl State {
    fn reset_stream(&mut self) {
        self.pending.clear();
        self.received_samples = 0;
        self.available.clear();
        self.next_pts = None;
        self.total_out_samples = 0;
        self.tail_sent = false;
        if let Some(renderer) = self.renderer.as_mut() {
            let _ = renderer.reset();
        }
    }

    fn latency_ns(&self) -> u64 {
        let samples = self.renderer.as_ref().map_or(
            FilterbankKind::Public.latency(),
            RosellaRenderer::latency_samples,
        );
        samples as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE)
    }
}

#[derive(Default)]
pub struct HrtfRender {
    state: Mutex<State>,
}

impl HrtfRender {
    fn sink_caps(format: Option<&glib::SendValue>, rate: Option<&glib::SendValue>) -> gst::Caps {
        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            for features in objectaudio_meta::sink_feature_sets() {
                let Some(features) = features else {
                    continue;
                };
                let mut structure = gst::Structure::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("channels", gst::IntRange::new(1, MAX_OBJECT_CHANNELS))
                    .field("rate", SAMPLE_RATE as i32)
                    .field("channel-mask", gst::Bitmask(0))
                    .build();
                if let Some(format) = format {
                    structure.set_value("format", format.to_send_value());
                }
                if let Some(rate) = rate {
                    structure.set_value("rate", rate.to_send_value());
                }
                caps.append_structure_full(structure, Some(features));
            }
        }
        caps
    }

    fn src_caps(format: Option<&glib::SendValue>, rate: Option<&glib::SendValue>) -> gst::Caps {
        let mut structure = gst::Structure::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("channels", OUTPUT_CHANNELS as i32)
            .field("rate", SAMPLE_RATE as i32)
            .field(
                "channel-mask",
                gst::Bitmask(
                    gst_audio::AudioChannelPosition::positions_to_mask(
                        &[
                            gst_audio::AudioChannelPosition::FrontLeft,
                            gst_audio::AudioChannelPosition::FrontRight,
                        ],
                        false,
                    )
                    .unwrap_or(0x3),
                ),
            )
            .build();
        if let Some(format) = format {
            structure.set_value("format", format.to_send_value());
        }
        if let Some(rate) = rate {
            structure.set_value("rate", rate.to_send_value());
        }
        gst::Caps::builder_full().structure(structure).build()
    }

    fn tail_buffer(&self) -> Option<gst::Buffer> {
        let mut state = salsa_gst::lock(&self.state);
        if state.tail_sent {
            return None;
        }
        let tail_seconds = state.tail_seconds;
        let renderer = state.renderer.as_mut()?;
        let capacity = renderer.finish_capacity(tail_seconds);
        let tail = renderer.finish(capacity).ok()?;
        state.tail_sent = true;
        if tail.is_empty() {
            return None;
        }
        let samples = tail.len() / OUTPUT_CHANNELS;
        let mut bytes = Vec::with_capacity(tail.len() * 4);
        for value in tail {
            bytes.extend_from_slice(&(value as f32).to_le_bytes());
        }
        let mut buffer = gst::Buffer::from_slice(bytes);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(state.next_pts);
            buffer.set_duration(gst::ClockTime::from_nseconds(
                samples as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE),
            ));
            buffer.set_offset(state.total_out_samples);
            buffer.set_offset_end(state.total_out_samples + samples as u64);
        }
        state.total_out_samples += samples as u64;
        if let Some(pts) = state.next_pts {
            state.next_pts = Some(
                pts + gst::ClockTime::from_nseconds(
                    samples as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE),
                ),
            );
        }
        Some(buffer)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for HrtfRender {
    const NAME: &'static str = "GstSalsaHrtfRender";
    type Type = super::HrtfRender;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for HrtfRender {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecString::builder("personalized-headphone")
                    .nick("Personalized headphone")
                    .blurb("Rosella .personalized_headphone JSON model")
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt::builder("filterbank")
                    .nick("Rosella filterbank")
                    .blurb("1 = public (13-tap / 6-slot, 961 samples), 2 = captured (11-tap / 3-slot, 448 samples)")
                    .default_value(1)
                    .minimum(1)
                    .maximum(2)
                    .readwrite()
                    .build(),
                glib::ParamSpecString::builder("profile")
                    .nick("Distance profile")
                    .blurb("near, mid or far")
                    .default_value("mid")
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt::builder("object-delay")
                    .nick("Object delay")
                    .blurb("OAMD object delay in samples")
                    .default_value(DEFAULT_OBJECT_DELAY)
                    .maximum(65_535)
                    .readwrite()
                    .build(),
                glib::ParamSpecDouble::builder("tail-seconds")
                    .nick("Room tail")
                    .blurb("Room/filterbank flush tail in seconds")
                    .default_value(5.0)
                    .minimum(0.0)
                    .maximum(60.0)
                    .readwrite()
                    .build(),
                glib::ParamSpecDouble::builder("output-gain")
                    .nick("Output gain")
                    .blurb("Linear gain applied to the rendered stereo")
                    .default_value(1.0)
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt::builder("chunk-frames")
                    .nick("Chunk frames")
                    .blurb("Internal 1536-sample frames per render chunk")
                    .default_value(1)
                    .minimum(1)
                    .maximum(1024)
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt::builder("room-impulse-slots")
                    .nick("Room impulse slots")
                    .blurb("Room FIR length in 64-sample slots")
                    .default_value(4096)
                    .minimum(1)
                    .maximum(65_536)
                    .readwrite()
                    .build(),
            ]
        })
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut state = salsa_gst::lock(&self.state);
        match pspec.name() {
            "personalized-headphone" => {
                state.model_path = value.get::<String>().unwrap_or_default();
            }
            "filterbank" => {
                state.filterbank = match value.get::<u32>().unwrap_or(1) {
                    2 => FilterbankKind::Captured,
                    _ => FilterbankKind::Public,
                };
            }
            "profile" => {
                let text = value.get::<String>().unwrap_or_default();
                if let Some(profile) = RosellaProfile::parse(&text) {
                    state.profile = profile;
                }
            }
            "object-delay" => state.object_delay = value.get().unwrap_or(DEFAULT_OBJECT_DELAY),
            "tail-seconds" => state.tail_seconds = value.get().unwrap_or(5.0),
            "output-gain" => state.output_gain = value.get().unwrap_or(1.0),
            "chunk-frames" => state.chunk_frames = value.get().unwrap_or(1),
            "room-impulse-slots" => state.room_impulse_slots = value.get().unwrap_or(4096),
            _ => {}
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let state = salsa_gst::lock(&self.state);
        match pspec.name() {
            "personalized-headphone" => state.model_path.to_value(),
            "filterbank" => match state.filterbank {
                FilterbankKind::Public => 1u32,
                FilterbankKind::Captured => 2u32,
            }
            .to_value(),
            "profile" => state.profile.name().to_value(),
            "object-delay" => state.object_delay.to_value(),
            "tail-seconds" => state.tail_seconds.to_value(),
            "output-gain" => state.output_gain.to_value(),
            "chunk-frames" => state.chunk_frames.to_value(),
            "room-impulse-slots" => state.room_impulse_slots.to_value(),
            _ => pspec.default_value().clone(),
        }
    }
}

impl GstObjectImpl for HrtfRender {}

impl ElementImpl for HrtfRender {
    salsa_gst::element_metadata!(
        "HRTF Binaural Renderer",
        "Audio/Renderer",
        "Renders object audio through a Rosella personalized headphone model"
    );

    salsa_gst::pad_templates!({
        let sink_caps = Self::sink_caps(None, None);
        let src_caps: gst::Caps = format!(
            "audio/x-raw, format=(string)F32LE, layout=(string)interleaved, \
     channels=(int){OUTPUT_CHANNELS}, rate=(int){SAMPLE_RATE}, \
     channel-mask=(bitmask)0x3"
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

impl BaseTransformImpl for HrtfRender {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = salsa_gst::lock(&self.state);
        state.renderer = None;
        state.reset_stream();
        state.in_bpf = 0;
        state.in_channels = 0;
        state.object_count = 0;
        state.out_bpf = 0;
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = salsa_gst::lock(&self.state);
        state.renderer = None;
        state.reset_stream();
        Ok(())
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let (format, rate) = (
            caps.structure(0)
                .and_then(|structure| structure.value("format").ok())
                .map(|value| value.to_send_value()),
            caps.structure(0)
                .and_then(|structure| structure.value("rate").ok())
                .map(|value| value.to_send_value()),
        );
        let mut other = if direction == gst::PadDirection::Src {
            Self::sink_caps(format.as_ref(), rate.as_ref())
        } else {
            Self::src_caps(format.as_ref(), rate.as_ref())
        };
        if let Some(filter) = filter {
            other = other.intersect_with_mode(filter, gst::CapsIntersectMode::First);
        }
        Some(other)
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let in_info = gst_audio::AudioInfo::from_caps(incaps).map_err(|_| {
            gst::loggable_error!(gst::CAT_RUST, "invalid salsahrtfrender sink caps")
        })?;
        let out_info = gst_audio::AudioInfo::from_caps(outcaps)
            .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid salsahrtfrender src caps"))?;
        if in_info.channels() < 1 {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsahrtfrender needs at least the LFE channel, got 0"
            ));
        }
        let in_channels = in_info.channels() as usize;
        if in_info.rate() != SAMPLE_RATE {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "the Rosella renderer is fixed at {SAMPLE_RATE} Hz, got {}",
                in_info.rate()
            ));
        }
        if in_info.format() != gst_audio::AudioFormat::F32le
            || in_info.layout() != gst_audio::AudioLayout::Interleaved
            || out_info.format() != gst_audio::AudioFormat::F32le
            || out_info.layout() != gst_audio::AudioLayout::Interleaved
            || out_info.channels() as usize != OUTPUT_CHANNELS
        {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsahrtfrender needs interleaved F32LE input and stereo F32LE output"
            ));
        }
        let mut state = salsa_gst::lock(&self.state);
        if state.model_path.is_empty() {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsahrtfrender requires personalized-headphone=<Rosella JSON model>"
            ));
        }
        let model = load_personalized_headphone(&state.model_path).map_err(|error| {
            gst::loggable_error!(gst::CAT_RUST, "cannot load Rosella model: {error}")
        })?;
        let options = RosellaRenderOptions {
            profile: state.profile,
            filterbank: state.filterbank,
            object_delay_samples: i64::from(state.object_delay),
            tail_seconds: state.tail_seconds,
            output_gain: state.output_gain,
            chunk_frames: state.chunk_frames as usize,
            room_impulse_slots: state.room_impulse_slots as usize,
            skip_latency: false,
        };
        let renderer = RosellaRenderer::new(model, options, in_channels).map_err(|error| {
            gst::loggable_error!(gst::CAT_RUST, "cannot create Rosella renderer: {error}")
        })?;
        state.renderer = Some(renderer);
        state.reset_stream();
        state.in_bpf = in_info.bpf() as usize;
        state.in_channels = in_channels;
        state.object_count = in_channels - 1;
        state.out_bpf = out_info.bpf() as usize;
        state.rate = in_info.rate();
        gst::info!(
            gst::CAT_RUST,
            "salsahrtfrender: {} bank, latency {} samples",
            state.filterbank.name(),
            state.filterbank.latency()
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
        let state = salsa_gst::lock(&self.state);
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
            // The renderer works in FRAME_SAMPLES batches but accepts input in
            // arbitrary sizes: a short buffer completes a frame started by
            // earlier buffers, so one transform can emit a whole frame for a
            // small input. Round the allocation up to whole frames instead of
            // assuming output is proportional to this buffer alone.
            let samples = size / in_bpf;
            let out_samples = samples.div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES;
            Some(out_samples * out_bpf)
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
        // Non-flushing seeks mark the first buffer after the jump as DISCONT.
        // Discard the pre-seek filter/room/timeline state; transient
        // convergence at the seek point is accepted.
        if inbuf.flags().contains(gst::BufferFlags::DISCONT) {
            let mut state = salsa_gst::lock(&self.state);
            state.reset_stream();
        }
        let (in_bpf, out_bpf) = {
            let state = salsa_gst::lock(&self.state);
            if state.renderer.is_none() || state.in_bpf == 0 {
                return Err(gst::FlowError::NotNegotiated);
            }
            (state.in_bpf, state.out_bpf)
        };
        let inmap = inbuf.map_readable().map_err(|_| gst::FlowError::Error)?;
        let samples = inmap.size() / in_bpf;
        if samples == 0 {
            return Ok(gst::FlowSuccess::Ok);
        }
        let in_channels = {
            let state = salsa_gst::lock(&self.state);
            state.in_channels
        };
        if in_channels == 0 {
            return Err(gst::FlowError::NotNegotiated);
        }
        let mut interleaved = Vec::with_capacity(samples * in_channels);
        let (chunks, _) = inmap.as_slice().as_chunks::<4>();
        for chunk in chunks {
            interleaved.push(f32::from_le_bytes(*chunk));
        }
        drop(inmap);

        let updates = objectaudio_meta::iter_spatial(inbuf);

        let mut state = salsa_gst::lock(&self.state);
        if state.next_pts.is_none() {
            state.next_pts = inbuf.pts();
        }
        let buffer_start = state.received_samples;
        for update in updates {
            state
                .renderer
                .as_mut()
                .ok_or(gst::FlowError::NotNegotiated)?
                .submit_metadata(&update.payload, buffer_start)
                .map_err(|error| {
                    gst::error!(gst::CAT_RUST, "invalid HRTF metadata: {error}");
                    gst::FlowError::Error
                })?;
        }
        state.received_samples += samples as i64;
        state.pending.extend_from_slice(&interleaved);
        let frame_samples = FRAME_SAMPLES * in_channels;
        while state.pending.len() >= frame_samples {
            let frame: Vec<f32> = state.pending.drain(..frame_samples).collect();
            let mut planar = vec![0.0f32; FRAME_SAMPLES * in_channels];
            for sample in 0..FRAME_SAMPLES {
                for channel in 0..in_channels {
                    planar[channel * FRAME_SAMPLES + sample] =
                        frame[sample * in_channels + channel];
                }
            }
            let produced = {
                let renderer = state
                    .renderer
                    .as_mut()
                    .ok_or(gst::FlowError::NotNegotiated)?;
                renderer.submit_frame(&planar, None).map_err(|error| {
                    gst::error!(gst::CAT_RUST, "salsahrtfrender: submit failed: {error}");
                    gst::FlowError::Error
                })?;
                renderer.take_output()
            };
            state.available.extend(produced);
        }
        let available = std::mem::take(&mut state.available);
        let out_samples = available.len() / OUTPUT_CHANNELS;
        let mut bytes = Vec::with_capacity(available.len() * 4);
        for value in &available {
            bytes.extend_from_slice(&(*value as f32).to_le_bytes());
        }
        let out_size = out_samples * out_bpf;
        if outbuf.size() != out_size {
            outbuf.set_size(out_size);
        }
        let mut outmap = outbuf.map_writable().map_err(|_| gst::FlowError::Error)?;
        let mapped = outmap.size();
        if bytes.len() < mapped {
            // BaseTransform asked transform_size for a bigger buffer; the
            // remaining bytes are silence.
            outmap[..bytes.len()].copy_from_slice(&bytes);
            outmap[bytes.len()..].fill(0);
        } else {
            outmap.copy_from_slice(&bytes[..mapped]);
        }
        drop(outmap);

        let duration = gst::ClockTime::from_nseconds(
            out_samples as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE),
        );
        outbuf.set_pts(state.next_pts);
        outbuf.set_duration(duration);
        outbuf.set_offset(state.total_out_samples);
        outbuf.set_offset_end(state.total_out_samples + out_samples as u64);
        state.total_out_samples += out_samples as u64;
        if let Some(pts) = state.next_pts {
            state.next_pts = Some(pts + duration);
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn query(&self, direction: gst::PadDirection, query: &mut gst::QueryRef) -> bool {
        let handled = gst_base::subclass::base_transform::BaseTransformImplExt::parent_query(
            self, direction, query,
        );
        if direction == gst::PadDirection::Sink
            && let gst::QueryViewMut::Latency(latency) = query.view_mut()
        {
            let (live, min, max) = latency.result();
            let extra = {
                let state = salsa_gst::lock(&self.state);
                gst::ClockTime::from_nseconds(state.latency_ns())
            };
            latency.set(
                live,
                min.saturating_add(extra),
                max.map(|value| value.saturating_add(extra)),
            );
            return true;
        }
        handled
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Eos(_) => {
                if let Some(buffer) = self.tail_buffer() {
                    let object = self.obj();
                    let pad = object.src_pad();
                    if pad.push(buffer).is_err() {
                        gst::warning!(gst::CAT_RUST, "salsahrtfrender: cannot push the room tail");
                    }
                }
                self.parent_sink_event(event)
            }
            gst::EventView::FlushStop(_) => {
                {
                    let mut state = salsa_gst::lock(&self.state);
                    state.reset_stream();
                }
                self.parent_sink_event(event)
            }
            _ => self.parent_sink_event(event),
        }
    }
}
