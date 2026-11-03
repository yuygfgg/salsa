//! `salsaobjectaudioadapt`: bridge between salsa `SpatialPayload` metadata and
//! Dolby's OAMD `DlbObjectAudioMeta`.
//!
//! GStreamer interop converts metadata here through `objectaudio-oamd`.
//! Renderers exchange only the versioned salsa `SpatialPayload`.

use std::sync::{Mutex, OnceLock};

use gst::glib;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use objectaudio_meta::{CAPS_FEATURE, DLB_CAPS_FEATURE};

/// Dolby's `dlboar` sink caps accept `channels = [1, 32]`.
const MAX_CHANNELS: i32 = 32;
const MIN_RATE: i32 = 32_000;
const MAX_RATE: i32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Direction {
    #[default]
    Auto,
    ToDolby,
    ToSalsa,
}

impl Direction {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "to-dolby" => Some(Self::ToDolby),
            "to-salsa" => Some(Self::ToSalsa),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ToDolby => "to-dolby",
            Self::ToSalsa => "to-salsa",
        }
    }
}

#[derive(Debug)]
struct State {
    convert_to_float: bool,
    remove_dlb: bool,
    direction: Direction,
    resolved: Direction,
    rate: u32,
    in_format: Option<gst_audio::AudioFormat>,
    out_format: Option<gst_audio::AudioFormat>,
    scene: Option<objectaudio_ir::SceneTimeline>,
    received: i64,
    bpf: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            convert_to_float: false,
            remove_dlb: false,
            direction: Direction::Auto,
            resolved: Direction::ToDolby,
            rate: 48_000,
            in_format: None,
            out_format: None,
            scene: None,
            received: 0,
            bpf: 0,
        }
    }
}

#[derive(Default)]
pub struct ObjectAudioAdapt {
    state: Mutex<State>,
}

impl ObjectAudioAdapt {
    fn salsa_features() -> Vec<gst::CapsFeatures> {
        vec![
            gst::CapsFeatures::new([CAPS_FEATURE]),
            gst::CapsFeatures::new(["memory:SystemMemory", CAPS_FEATURE]),
        ]
    }

    fn dolby_features() -> Vec<gst::CapsFeatures> {
        vec![
            gst::CapsFeatures::new([DLB_CAPS_FEATURE]),
            gst::CapsFeatures::new(["memory:SystemMemory", DLB_CAPS_FEATURE]),
        ]
    }

    fn sink_features() -> Vec<Option<gst::CapsFeatures>> {
        let mut features: Vec<Option<gst::CapsFeatures>> =
            Self::salsa_features().into_iter().map(Some).collect();
        features.extend(Self::dolby_features().into_iter().map(Some));
        features.push(None);
        features
    }

    fn src_features(direction: Direction, remove: bool) -> Vec<Option<gst::CapsFeatures>> {
        match direction {
            Direction::ToDolby if !remove => Self::dolby_features().into_iter().map(Some).collect(),
            Direction::ToDolby | Direction::ToSalsa => {
                Self::salsa_features().into_iter().map(Some).collect()
            }
            Direction::Auto => {
                let mut features: Vec<Option<gst::CapsFeatures>> = if remove {
                    Self::salsa_features().into_iter().map(Some).collect()
                } else {
                    Self::dolby_features().into_iter().map(Some).collect()
                };
                features.extend(Self::salsa_features().into_iter().map(Some));
                features
            }
        }
    }

    fn template_caps(features: &[Option<gst::CapsFeatures>]) -> gst::Caps {
        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            for feature in features {
                let structure = gst::Structure::builder("audio/x-raw")
                    .field("format", gst::List::new(["F32LE", "S32LE"]))
                    .field("layout", "interleaved")
                    .field("channels", gst::IntRange::new(1, MAX_CHANNELS))
                    .field("rate", gst::IntRange::new(MIN_RATE, MAX_RATE))
                    .field("channel-mask", gst::Bitmask(0))
                    .build();
                match feature {
                    Some(feature) => caps.append_structure_full(structure, Some(feature.clone())),
                    None => caps.append_structure(structure),
                }
            }
        }
        caps
    }

    fn transform_caps_impl(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let (convert, direction_mode, remove) = {
            let state = self.state.lock().unwrap();
            (state.convert_to_float, state.direction, state.remove_dlb)
        };

        let mut out = gst::Caps::new_empty();
        {
            let out_ref = out.get_mut().unwrap();
            for index in 0..caps.size() {
                let Some(structure) = caps.structure(index) else {
                    continue;
                };
                let mut structure = structure.to_owned();

                if convert {
                    if direction == gst::PadDirection::Sink {
                        structure.set("format", "F32LE");
                    } else {
                        structure.set("format", gst::List::new(["F32LE", "S32LE"]));
                    }
                }

                let features = if direction == gst::PadDirection::Sink {
                    Self::src_features(direction_mode, remove)
                } else {
                    Self::sink_features()
                };
                for feature in features {
                    match feature {
                        Some(feature) => {
                            out_ref.append_structure_full(structure.clone(), Some(feature))
                        }
                        None => out_ref.append_structure(structure.clone()),
                    }
                }
            }
        }

        if let Some(filter) = filter {
            out = out.intersect_with_mode(filter, gst::CapsIntersectMode::First);
        }
        Some(out)
    }

    fn resolve_direction(mode: Direction, incaps: &gst::Caps) -> Direction {
        match mode {
            Direction::Auto => {
                let features = incaps.features(0);
                if features
                    .map(|features| features.contains(DLB_CAPS_FEATURE))
                    .unwrap_or(false)
                {
                    Direction::ToSalsa
                } else {
                    Direction::ToDolby
                }
            }
            mode => mode,
        }
    }
}

fn convert_s32le_to_f32le(bytes: &mut [u8]) {
    let (chunks, _) = bytes.as_chunks_mut::<4>();
    for chunk in chunks {
        let sample = i32::from_le_bytes(*chunk);
        let normalized = sample as f32 / 2_147_483_648.0;
        chunk.copy_from_slice(&normalized.to_le_bytes());
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ObjectAudioAdapt {
    const NAME: &'static str = "GstObjectAudioAdapt";
    type Type = super::ObjectAudioAdapt;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for ObjectAudioAdapt {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecString::builder("direction")
                    .nick("Bridge direction")
                    .blurb("auto, to-dolby or to-salsa")
                    .default_value("auto")
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("remove")
                    .nick("Remove Dolby caps feature")
                    .blurb("Do not add DlbObjectAudioMeta (to-dolby direction)")
                    .default_value(false)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("float")
                    .nick("Convert to float")
                    .blurb("Convert interleaved S32LE samples to F32LE")
                    .default_value(false)
                    .readwrite()
                    .build(),
            ]
        })
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "direction" => {
                let text = value.get::<String>().unwrap_or_default();
                if let Some(direction) = Direction::parse(&text) {
                    self.state.lock().unwrap().direction = direction;
                    self.obj().reconfigure_src();
                }
            }
            "remove" => {
                self.state.lock().unwrap().remove_dlb = value.get().unwrap_or(false);
                self.obj().reconfigure_src();
            }
            "float" => {
                self.state.lock().unwrap().convert_to_float = value.get().unwrap_or(false);
                self.obj().reconfigure_src();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let state = self.state.lock().unwrap();
        match pspec.name() {
            "direction" => state.direction.name().to_value(),
            "remove" => state.remove_dlb.to_value(),
            "float" => state.convert_to_float.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ObjectAudioAdapt {}

impl ElementImpl for ObjectAudioAdapt {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
        Some(METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "Salsa/Dolby Object Audio Adapter",
                "Filter/Audio",
                "Bridges salsa SpatialPayload metadata and Dolby OAMD metadata",
                "salsa",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
        TEMPLATES.get_or_init(|| {
            let sink_caps = Self::template_caps(&Self::sink_features());
            let src_caps = Self::template_caps(&Self::src_features(Direction::Auto, false));
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

impl BaseTransformImpl for ObjectAudioAdapt {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.in_format = None;
        state.out_format = None;
        state.scene = None;
        state.received = 0;
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.in_format = None;
        state.out_format = None;
        state.scene = None;
        state.received = 0;
        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if matches!(
            event.view(),
            gst::EventView::FlushStop(_) | gst::EventView::Segment(_)
        ) {
            let mut state = self.state.lock().unwrap();
            state.scene = None;
            state.received = 0;
        }
        self.parent_sink_event(event)
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        self.transform_caps_impl(direction, caps, filter)
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let in_info = gst_audio::AudioInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid adapter sink caps"))?;
        let out_info = gst_audio::AudioInfo::from_caps(outcaps)
            .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid adapter src caps"))?;
        let (mode, convert) = {
            let state = self.state.lock().unwrap();
            (state.direction, state.convert_to_float)
        };
        let resolved = Self::resolve_direction(mode, incaps);

        if in_info.channels() > MAX_CHANNELS as u32 {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "Dolby interop supports at most {MAX_CHANNELS} object channels, got {}",
                in_info.channels()
            ));
        }
        if in_info.channels() != out_info.channels()
            || in_info.rate() != out_info.rate()
            || in_info.layout() != gst_audio::AudioLayout::Interleaved
            || out_info.layout() != gst_audio::AudioLayout::Interleaved
        {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "adapter requires matching interleaved input/output audio info"
            ));
        }

        let in_format = in_info.format();
        let out_format = out_info.format();
        if convert {
            if out_format != gst_audio::AudioFormat::F32le
                || !matches!(
                    in_format,
                    gst_audio::AudioFormat::F32le | gst_audio::AudioFormat::S32le
                )
            {
                return Err(gst::loggable_error!(
                    gst::CAT_RUST,
                    "float=1 converts S32LE/F32LE input to F32LE output"
                ));
            }
        } else if in_format != out_format {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "adapter does not change the sample format unless float=1"
            ));
        }

        let mut state = self.state.lock().unwrap();
        state.resolved = resolved;
        state.rate = in_info.rate();
        state.bpf = in_info.bpf() as usize;
        state.scene = None;
        state.received = 0;
        state.in_format = Some(in_format);
        state.out_format = Some(out_format);
        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (convert, remove, direction, rate) = {
            let state = self.state.lock().unwrap();
            (
                state.convert_to_float,
                state.remove_dlb,
                state.resolved,
                state.rate,
            )
        };

        if convert {
            let in_format = self.state.lock().unwrap().in_format;
            if in_format == Some(gst_audio::AudioFormat::S32le) {
                let mut map = buffer.map_writable().map_err(|_| gst::FlowError::Error)?;
                convert_s32le_to_f32le(map.as_mut_slice());
            }
        }

        match direction {
            Direction::ToDolby => {
                if remove {
                    return Ok(gst::FlowSuccess::Ok);
                }
                let snapshots = objectaudio_meta::iter_spatial(buffer);
                if !objectaudio_meta::dlb_available() {
                    gst::error!(
                        gst::CAT_RUST,
                        "DlbObjectAudioMeta is not registered; load a Dolby plugin (dlbac3dec/dlboar)"
                    );
                    return Err(gst::FlowError::NotNegotiated);
                }
                if !objectaudio_meta::dlb_payloads(buffer).is_empty() {
                    return Ok(gst::FlowSuccess::Ok);
                }
                let mut state = self.state.lock().unwrap();
                if buffer.flags().contains(gst::BufferFlags::DISCONT) {
                    state.scene = None;
                    state.received = 0;
                }
                let start = state.received;
                let bpf = state.bpf;
                if bpf == 0 {
                    return Err(gst::FlowError::NotNegotiated);
                }
                let frames = buffer.size() / bpf;
                for snapshot in snapshots {
                    snapshot
                        .payload
                        .validate_layout(rate, bpf / 4)
                        .map_err(|error| {
                            gst::error!(gst::CAT_RUST, "invalid adapter layout: {error}");
                            gst::FlowError::Error
                        })?;
                    let result = match &mut state.scene {
                        Some(scene) => scene.submit(&snapshot.payload, start),
                        None => {
                            objectaudio_ir::SceneTimeline::from_payload(&snapshot.payload, start)
                                .map(|scene| state.scene = Some(scene))
                        }
                    };
                    result.map_err(|error| {
                        gst::error!(gst::CAT_RUST, "invalid adapter metadata: {error}");
                        gst::FlowError::Error
                    })?;
                }
                state.received += frames as i64;
                if let Some(scene) = &mut state.scene {
                    // OAMD offsets fit within a codec frame. Splitting here is
                    // a format decision; scene slicing owns all time semantics.
                    let mut boundaries: Vec<_> = (0..frames)
                        .step_by(1536)
                        .chain(
                            scene
                                .calibration_changes(start, start + frames as i64)
                                .map(|(sample, _)| (sample - start) as usize),
                        )
                        .chain(std::iter::once(frames))
                        .collect();
                    boundaries.sort_unstable();
                    boundaries.dedup();
                    for interval in boundaries.windows(2) {
                        let offset = interval[0];
                        let count = interval[1] - offset;
                        let payload = scene.payload(start + offset as i64, count);
                        let bytes = objectaudio_oamd::encode(&payload).map_err(|error| {
                            gst::error!(gst::CAT_RUST, "cannot encode scene as OAMD: {error}");
                            gst::FlowError::Error
                        })?;
                        objectaudio_meta::add_dlb_payload(buffer, &bytes, offset, bpf).map_err(
                            |error| {
                                gst::error!(
                                    gst::CAT_RUST,
                                    "failed to add DlbObjectAudioMeta: {error}"
                                );
                                gst::FlowError::Error
                            },
                        )?;
                    }
                    scene.discard_before(start + frames as i64);
                }
                Ok(gst::FlowSuccess::Ok)
            }
            Direction::ToSalsa => {
                let payloads: Vec<_> = objectaudio_meta::dlb_payloads(buffer)
                    .into_iter()
                    .map(|payload| (payload.bytes.to_vec(), payload.offset, payload.bpf))
                    .collect();
                if payloads.is_empty() {
                    return Ok(gst::FlowSuccess::Ok);
                }
                if payloads[0].2 == 0 {
                    return Err(gst::FlowError::Error);
                }
                objectaudio_meta::remove_all(buffer);
                for (bytes, offset, bpf) in payloads {
                    let frame = objectaudio_oamd::parse(&bytes).map_err(|error| {
                        gst::error!(gst::CAT_RUST, "cannot parse Dolby OAMD: {error}");
                        gst::FlowError::Error
                    })?;
                    let mapping =
                        frame.map_to_ir(offset as u32, objectaudio_oamd::BedPolicy::Expand);
                    let mut payload =
                        objectaudio_ir::SpatialPayload::new(rate, mapping.objects, mapping.events)
                            .with_snapshot_ramp(mapping.snapshot_ramp_samples)
                            .with_snapshot_offset(i64::from(mapping.snapshot_offset));
                    if let Some(calibration) = mapping.calibration {
                        payload = payload.with_calibration(calibration);
                    }
                    objectaudio_meta::add_spatial(buffer, &payload, bpf).map_err(|error| {
                        gst::error!(gst::CAT_RUST, "cannot add SpatialPayload: {error}");
                        gst::FlowError::Error
                    })?;
                }
                Ok(gst::FlowSuccess::Ok)
            }
            Direction::Auto => unreachable!("resolved at set_caps"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::convert_s32le_to_f32le;

    #[test]
    fn converts_s32le_to_normalized_f32() {
        let samples = [0i32, 1 << 30, -(1 << 30), i32::MAX, i32::MIN];
        let mut bytes = Vec::new();
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        convert_s32le_to_f32le(&mut bytes);
        let values: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect();
        assert_eq!(values[0], 0.0);
        assert!((values[1] - 0.5).abs() < 1e-6);
        assert!((values[2] + 0.5).abs() < 1e-6);
        assert!((values[3] - 1.0).abs() < 1e-6);
        assert_eq!(values[4], -1.0);
    }
}
