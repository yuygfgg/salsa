//! `salsadamfsink`: raw object audio + `ObjectAudioMeta` -> DAMF presentation.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use gst::glib;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

use gst_audio::prelude::*;
use gst_base::subclass::prelude::*;

use objectaudio_damf::{BedEntry, CafWriter, ManifestSpec, Metadata, ObjectEntry, write_manifest};
use objectaudio_ir::{SceneTimeline, SpatialPayload};

const MAX_CHANNELS: i32 = 1024;
const MIN_RATE: i32 = 8_000;
const MAX_RATE: i32 = 192_000;
const DEFAULT_LFE_ID: i32 = 3;
const DEFAULT_OBJECT_ID_BASE: i32 = 10;
const DEFAULT_FPS: f64 = 24.0;
const DEFAULT_CREATION_TOOL: &str = "salsa-damf-sink";

#[derive(Debug)]
struct State {
    location: String,
    creation_tool: String,
    creation_tool_version: String,
    fps: f64,
    offset: f64,
    lfe_id: i32,
    object_id_base: i32,
    little_endian: bool,
    rate: u32,
    channels: usize,
    format: Option<gst_audio::AudioFormat>,
    writer: Option<CafWriter>,
    audio_location: Option<PathBuf>,
    metadata_location: Option<PathBuf>,
    timeline: Option<SceneTimeline>,
    received: u64,
    finalized: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            location: String::new(),
            creation_tool: DEFAULT_CREATION_TOOL.to_string(),
            creation_tool_version: env!("CARGO_PKG_VERSION").to_string(),
            fps: DEFAULT_FPS,
            offset: 0.0,
            lfe_id: DEFAULT_LFE_ID,
            object_id_base: DEFAULT_OBJECT_ID_BASE,
            little_endian: false,
            rate: 0,
            channels: 0,
            format: None,
            writer: None,
            audio_location: None,
            metadata_location: None,
            timeline: None,
            received: 0,
            finalized: false,
        }
    }
}

impl State {
    fn object_count(&self) -> usize {
        self.channels.saturating_sub(1)
    }

    fn reset(&mut self) {
        self.rate = 0;
        self.channels = 0;
        self.format = None;
        self.writer = None;
        self.audio_location = None;
        self.metadata_location = None;
        self.timeline = None;
        self.received = 0;
        self.finalized = false;
    }

    fn collect_payload(
        &mut self,
        payload: &SpatialPayload,
        buffer_start: u64,
    ) -> objectaudio_damf::Result<()> {
        payload
            .validate_layout(self.rate, self.channels)
            .map_err(|error| objectaudio_damf::DamfError::Invalid(format!("{error}")))?;
        let start = i64::try_from(buffer_start).map_err(|_| {
            objectaudio_damf::DamfError::Invalid("DAMF duration exceeds the IR timeline".into())
        })?;
        if let Some(timeline) = &mut self.timeline {
            timeline.submit(payload, start)
        } else {
            SceneTimeline::from_payload(payload, start)
                .map(|timeline| self.timeline = Some(timeline))
        }
        .map_err(|error| objectaudio_damf::DamfError::Invalid(error.to_string()))?;
        Ok(())
    }

    fn finalize(&mut self) -> objectaudio_damf::Result<()> {
        if self.finalized {
            return Ok(());
        }
        if self.writer.is_none() {
            return Ok(());
        }
        let (Some(audio), Some(metadata)) = (&self.audio_location, &self.metadata_location) else {
            return Err(objectaudio_damf::DamfError::Invalid(
                "DAMF sink has no sidecar locations".into(),
            ));
        };
        let object_end = i64::from(self.object_id_base) + self.object_count() as i64;
        if self.object_count() > 0
            && i64::from(self.lfe_id) >= i64::from(self.object_id_base)
            && i64::from(self.lfe_id) < object_end
        {
            return Err(objectaudio_damf::DamfError::Invalid(format!(
                "LFE id {} collides with object id range {}..{}",
                self.lfe_id, self.object_id_base, object_end
            )));
        }
        let metadata_doc = if let Some(timeline) = &self.timeline {
            let frames = usize::try_from(self.received).map_err(|_| {
                objectaudio_damf::DamfError::Invalid("DAMF duration exceeds address space".into())
            })?;
            Metadata::from_timeline(timeline, frames, i64::from(self.object_id_base))?
        } else {
            Metadata {
                sample_rate: self.rate,
                events: Vec::new(),
            }
        };
        let writer = self.writer.take().expect("writer checked above");
        writer.finish()?;
        metadata_doc.save(metadata)?;

        let beds = [BedEntry {
            id: i64::from(self.lfe_id),
            name: "LFE".to_string(),
        }];
        let objects: Vec<ObjectEntry> = (0..self.object_count())
            .map(|index| ObjectEntry {
                id: i64::from(self.object_id_base) + index as i64,
                description: None,
            })
            .collect();
        let audio_name = file_name(audio)?;
        let metadata_name = file_name(metadata)?;
        let manifest = PathBuf::from(&self.location);
        write_manifest(
            &manifest,
            &ManifestSpec {
                audio_name: &audio_name,
                metadata_name: &metadata_name,
                offset: self.offset,
                fps: self.fps,
                sc_bed_configuration: &[i64::from(self.lfe_id)],
                creation_tool: &self.creation_tool,
                creation_tool_version: &self.creation_tool_version,
                beds: &beds,
                objects: &objects,
            },
        )?;
        self.finalized = true;
        Ok(())
    }
}

fn file_name(path: &Path) -> objectaudio_damf::Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            objectaudio_damf::DamfError::Invalid(format!(
                "invalid DAMF sidecar path {}",
                path.display()
            ))
        })
}

fn sidecar(manifest: &str, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{manifest}.{suffix}"))
}

fn broad_caps() -> gst::Caps {
    format!(
        "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
           format=(string){{F32LE,S24LE}}, layout=(string)interleaved, \
           channels=(int)[1,{MAX_CHANNELS}], rate=(int)[{MIN_RATE},{MAX_RATE}], \
           channel-mask=(bitmask)0; \
         audio/x-raw(meta:ObjectAudioMeta), \
           format=(string){{F32LE,S24LE}}, layout=(string)interleaved, \
           channels=(int)[1,{MAX_CHANNELS}], rate=(int)[{MIN_RATE},{MAX_RATE}], \
           channel-mask=(bitmask)0"
    )
    .parse()
    .expect("static DAMF sink caps")
}

#[derive(Default)]
pub struct DamfSink {
    state: Mutex<State>,
}

impl DamfSink {
    fn finalize(&self) -> objectaudio_damf::Result<()> {
        self.state.lock().unwrap().finalize()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for DamfSink {
    const NAME: &'static str = "GstSalsaDamfSink";
    type Type = super::DamfSink;
    type ParentType = gst_base::BaseSink;
}

impl ObjectImpl for DamfSink {
    fn constructed(&self) {
        self.parent_constructed();
        // A file writer must run as fast as the upstream pipeline allows.
        self.obj().set_sync(false);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Path to the DAMF .atmos manifest to write")
                    .readwrite()
                    .build(),
                glib::ParamSpecString::builder("creation-tool")
                    .nick("Creation Tool")
                    .blurb("Manifest creation tool name")
                    .readwrite()
                    .build(),
                glib::ParamSpecString::builder("creation-tool-version")
                    .nick("Creation Tool Version")
                    .blurb("Manifest creation tool version")
                    .readwrite()
                    .build(),
                glib::ParamSpecDouble::builder("fps")
                    .nick("FPS")
                    .blurb("Manifest frame rate")
                    .default_value(DEFAULT_FPS)
                    .minimum(0.0)
                    .maximum(1000.0)
                    .readwrite()
                    .build(),
                glib::ParamSpecDouble::builder("offset")
                    .nick("Offset")
                    .blurb("Manifest presentation offset")
                    .default_value(0.0)
                    .readwrite()
                    .build(),
                glib::ParamSpecInt::builder("lfe-id")
                    .nick("LFE ID")
                    .blurb("DAMF bed ID used for the LFE channel")
                    .default_value(DEFAULT_LFE_ID)
                    .minimum(1)
                    .maximum(i32::MAX)
                    .readwrite()
                    .build(),
                glib::ParamSpecInt::builder("object-id-base")
                    .nick("Object ID Base")
                    .blurb("DAMF ID of salsa object 1")
                    .default_value(DEFAULT_OBJECT_ID_BASE)
                    .minimum(1)
                    .maximum(i32::MAX)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("little-endian")
                    .nick("Little Endian")
                    .blurb("Write little-endian CAF samples instead of big-endian")
                    .default_value(false)
                    .readwrite()
                    .build(),
            ]
        })
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut state = self.state.lock().unwrap();
        match pspec.name() {
            "location" => state.location = value.get().unwrap_or_default(),
            "creation-tool" => state.creation_tool = value.get().unwrap_or_default(),
            "creation-tool-version" => {
                state.creation_tool_version = value.get().unwrap_or_default()
            }
            "fps" => state.fps = value.get().unwrap_or(DEFAULT_FPS),
            "offset" => state.offset = value.get().unwrap_or(0.0),
            "lfe-id" => state.lfe_id = value.get().unwrap_or(DEFAULT_LFE_ID),
            "object-id-base" => {
                state.object_id_base = value.get().unwrap_or(DEFAULT_OBJECT_ID_BASE)
            }
            "little-endian" => state.little_endian = value.get().unwrap_or(false),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let state = self.state.lock().unwrap();
        match pspec.name() {
            "location" => state.location.to_value(),
            "creation-tool" => state.creation_tool.to_value(),
            "creation-tool-version" => state.creation_tool_version.to_value(),
            "fps" => state.fps.to_value(),
            "offset" => state.offset.to_value(),
            "lfe-id" => state.lfe_id.to_value(),
            "object-id-base" => state.object_id_base.to_value(),
            "little-endian" => state.little_endian.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for DamfSink {}

impl ElementImpl for DamfSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
        Some(METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "DAMF Sink",
                "Sink/Audio",
                "Writes raw object audio and ObjectAudioMeta as a DAMF presentation",
                "salsa",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
        TEMPLATES.get_or_init(|| {
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &broad_caps(),
                )
                .unwrap(),
            ]
        })
    }
}

impl BaseSinkImpl for DamfSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let location = self.state.lock().unwrap().location.clone();
        if location.is_empty() {
            return Err(gst::error_msg!(
                gst::ResourceError::NotFound,
                ("DAMF sink has no location")
            ));
        }
        self.state.lock().unwrap().reset();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let result = self.state.lock().unwrap().finalize();
        result.map_err(|error| {
            gst::error_msg!(
                gst::ResourceError::Write,
                ("cannot finish DAMF presentation: {error}")
            )
        })
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = gst_audio::AudioInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid DAMF sink caps"))?;
        let format = info.format();
        if !matches!(
            format,
            gst_audio::AudioFormat::F32le | gst_audio::AudioFormat::S24le
        ) {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "DAMF sink accepts only F32LE or S24LE"
            ));
        }
        let channels = info.channels() as usize;
        let rate = info.rate();
        if channels == 0 {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "DAMF sink needs at least the LFE channel"
            ));
        }

        let mut state = self.state.lock().unwrap();
        if state.writer.is_some() {
            if state.rate != rate || state.channels != channels || state.format != Some(format) {
                return Err(gst::loggable_error!(
                    gst::CAT_RUST,
                    "DAMF sink cannot renegotiate its audio format mid-stream"
                ));
            }
            return Ok(());
        }

        let audio = sidecar(&state.location, "audio");
        let metadata = sidecar(&state.location, "metadata");
        let writer = CafWriter::create_streaming(&audio, rate, channels, state.little_endian)
            .map_err(|error| {
                gst::loggable_error!(
                    gst::CAT_RUST,
                    "cannot create DAMF essence {}: {error}",
                    audio.display()
                )
            })?;
        state.rate = rate;
        state.channels = channels;
        state.format = Some(format);
        state.writer = Some(writer);
        state.audio_location = Some(audio);
        state.metadata_location = Some(metadata);
        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let Some(_) = state.writer.as_ref() else {
            return Err(gst::FlowError::NotNegotiated);
        };
        let format = state.format.ok_or(gst::FlowError::NotNegotiated)?;
        let channels = state.channels;
        let sample_bytes = match format {
            gst_audio::AudioFormat::F32le => 4,
            gst_audio::AudioFormat::S24le => 3,
            _ => return Err(gst::FlowError::NotNegotiated),
        };
        let frame_bytes = channels
            .checked_mul(sample_bytes)
            .ok_or(gst::FlowError::Error)?;
        if frame_bytes == 0 || !buffer.size().is_multiple_of(frame_bytes) {
            gst::error!(
                gst::CAT_RUST,
                "salsadamfsink: buffer size {} is not a multiple of frame size {frame_bytes}",
                buffer.size()
            );
            return Err(gst::FlowError::Error);
        }
        let frames = buffer.size() / frame_bytes;

        let start = state.received;
        for snapshot in objectaudio_meta::iter_spatial(buffer) {
            state
                .collect_payload(&snapshot.payload, start)
                .map_err(|error| {
                    gst::error!(gst::CAT_RUST, "salsadamfsink: invalid metadata: {error}");
                    gst::FlowError::Error
                })?;
        }

        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
        let samples = match format {
            gst_audio::AudioFormat::F32le => decode_f32le(map.as_slice()),
            gst_audio::AudioFormat::S24le => decode_s24le(map.as_slice()),
            _ => unreachable!(),
        };
        state
            .writer
            .as_mut()
            .expect("writer checked above")
            .write_frames(&samples)
            .map_err(|error| {
                gst::error!(gst::CAT_RUST, "salsadamfsink: CAF write failed: {error}");
                gst::FlowError::Error
            })?;
        state.received += frames as u64;
        Ok(gst::FlowSuccess::Ok)
    }

    fn event(&self, event: gst::Event) -> bool {
        if matches!(event.view(), gst::EventView::Eos(_))
            && let Err(error) = self.finalize()
        {
            gst::error!(gst::CAT_RUST, "salsadamfsink: cannot finish DAMF: {error}");
            self.obj().post_error_message(gst::error_msg!(
                gst::ResourceError::Write,
                ("cannot finish DAMF presentation: {error}")
            ));
        }
        BaseSinkImplExt::parent_event(self, event)
    }
}

fn decode_f32le(bytes: &[u8]) -> Vec<f32> {
    let (chunks, remainder) = bytes.as_chunks::<4>();
    debug_assert!(remainder.is_empty());
    chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

fn decode_s24le(bytes: &[u8]) -> Vec<f32> {
    let (chunks, remainder) = bytes.as_chunks::<3>();
    debug_assert!(remainder.is_empty());
    chunks
        .iter()
        .map(|chunk| {
            let raw =
                u32::from(chunk[0]) | (u32::from(chunk[1]) << 8) | (u32::from(chunk[2]) << 16);
            let signed = if raw & 0x0080_0000 != 0 {
                (raw | 0xff00_0000) as i32
            } else {
                raw as i32
            };
            signed as f32 / 8_388_608.0
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use objectaudio_ir::{ObjectEvent, ObjectState, Transition};

    #[test]
    fn collects_snapshots_and_events_on_the_signed_timeline() {
        let mut state = State {
            rate: 48_000,
            channels: 3,
            ..State::default()
        };
        let payload = SpatialPayload::new(
            48_000,
            vec![
                ObjectState {
                    id: 1,
                    position: [-1.0, 0.0, 0.0],
                    gain: 0.5,
                    enabled: true,
                    size: Some(0.25),
                    spread: None,
                },
                ObjectState::new(2, [1.0, 1.0, 0.0]),
            ],
            vec![ObjectEvent {
                offset: 1536,
                transition: Transition::Position(512),
                state: ObjectState::new(2, [0.0, 1.0, 0.0]),
            }],
        )
        .with_snapshot_ramp(1536)
        .with_snapshot_offset(64);
        state.collect_payload(&payload, 1000).unwrap();

        let timeline = state.timeline.as_ref().unwrap();
        assert_eq!(timeline.object_count(), 2);
        assert!(!timeline.state_at(0, 1063).enabled);
        assert_eq!(timeline.state_at(0, 1064).gain, 0.0);
        assert_eq!(timeline.state_at(0, 1832).gain, 0.25);
        assert_eq!(timeline.state_at(0, 2600).gain, 0.5);
        assert_eq!(timeline.state_at(0, 2600).size, Some(0.25));
        assert_eq!(timeline.state_at(1, 2536).position, [1.0, 1.0, 0.0]);
        assert_eq!(timeline.state_at(1, 2792).position, [0.5, 1.0, 0.0]);
    }

    #[test]
    fn decodes_s24le_with_sign_extension() {
        let samples = [0i32, 8_388_607, -8_388_608, 4_194_304, -1];
        let mut bytes = Vec::new();
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes()[..3]);
        }
        let decoded = decode_s24le(&bytes);
        assert_eq!(decoded.len(), samples.len());
        for (actual, expected) in decoded.iter().zip(samples) {
            let expected = expected as f32 / 8_388_608.0;
            assert!((actual - expected).abs() < 1e-7);
        }
    }
}
