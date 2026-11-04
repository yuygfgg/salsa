//! `salsadamfsrc`: DAMF presentation -> raw object audio.

use std::sync::{Mutex, OnceLock};

use gst::glib;
use gstreamer as gst;
use gstreamer_audio as gst_audio;
use gstreamer_base as gst_base;

use gst_audio::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use objectaudio_damf::{DamfInfo, DamfSource};

const DEFAULT_BLOCK_FRAMES: usize = salsa_codec::EAC3_FRAME_SAMPLES as usize;
const MAX_CHANNELS: usize = 1024;

#[derive(Debug)]
struct State {
    location: String,
    source: Option<DamfSource>,
    info: Option<DamfInfo>,
    caps: Option<gst::Caps>,
    attach_meta: bool,
    bpf: usize,
    block_frames: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            location: String::new(),
            source: None,
            info: None,
            caps: None,
            attach_meta: false,
            bpf: 0,
            block_frames: DEFAULT_BLOCK_FRAMES,
        }
    }
}

#[derive(Default)]
pub struct DamfSrc {
    state: Mutex<State>,
}

fn build_caps(info: &DamfInfo) -> gst::Caps {
    let structure = gst::Structure::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("channels", info.output_channels as i32)
        .field("rate", info.sample_rate as i32)
        .field("channel-mask", gst::Bitmask(0))
        .build();
    let mut caps = gst::Caps::new_empty();
    {
        let caps = caps.get_mut().unwrap();
        caps.append_structure_full(
            structure.clone(),
            Some(gst::CapsFeatures::new([
                "memory:SystemMemory",
                objectaudio_meta::CAPS_FEATURE,
            ])),
        );
        caps.append_structure_full(
            structure.clone(),
            Some(gst::CapsFeatures::new([objectaudio_meta::CAPS_FEATURE])),
        );
        caps.append_structure(structure);
    }
    caps
}

fn broad_caps() -> gst::Caps {
    format!(
        "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
           format=(string)F32LE, layout=(string)interleaved, \
           channels=(int)[1,{MAX_CHANNELS}], rate=(int)[8000,192000], channel-mask=(bitmask)0; \
         audio/x-raw(meta:ObjectAudioMeta), \
           format=(string)F32LE, layout=(string)interleaved, \
           channels=(int)[1,{MAX_CHANNELS}], rate=(int)[8000,192000], channel-mask=(bitmask)0; \
         audio/x-raw, \
           format=(string)F32LE, layout=(string)interleaved, \
           channels=(int)[1,{MAX_CHANNELS}], rate=(int)[8000,192000], channel-mask=(bitmask)0"
    )
    .parse()
    .expect("static DAMF source caps")
}

impl DamfSrc {
    fn negotiated_caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        let caps = salsa_gst::lock(&self.state).caps.clone()?;
        let caps = match filter {
            Some(filter) => caps.intersect(filter),
            None => caps,
        };
        (!caps.is_empty()).then_some(caps)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for DamfSrc {
    const NAME: &'static str = "GstSalsaDamfSrc";
    type Type = super::DamfSrc;
    type ParentType = gst_base::BaseSrc;
}

impl ObjectImpl for DamfSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Path to the DAMF .atmos manifest")
                    .readwrite()
                    .build(),
            ]
        })
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        if pspec.name() == "location" {
            salsa_gst::lock(&self.state).location = value.get().unwrap_or_default();
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => salsa_gst::lock(&self.state).location.to_value(),
            _ => pspec.default_value().clone(),
        }
    }
}

impl GstObjectImpl for DamfSrc {}

impl ElementImpl for DamfSrc {
    salsa_gst::element_metadata!(
        "DAMF Source",
        "Source/Audio",
        "Reads a Dolby Atmos Master Format presentation as raw object audio"
    );

    salsa_gst::pad_templates!({
        vec![
            gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &broad_caps(),
            )
            .unwrap(),
        ]
    });
}

impl BaseSrcImpl for DamfSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let location = salsa_gst::lock(&self.state).location.clone();
        if location.is_empty() {
            return Err(gst::error_msg!(
                gst::ResourceError::NotFound,
                ("DAMF source has no location")
            ));
        }
        let source = DamfSource::open(&location).map_err(|error| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ("cannot open DAMF presentation {location}: {error}")
            )
        })?;
        let info = source.info().clone();
        for warning in &info.warnings {
            gst::warning!(gst::CAT_RUST, "salsadamfsrc: {warning}");
        }
        let caps = build_caps(&info);
        self.obj().set_format(gst::Format::Time);
        let block_bytes = info
            .output_channels
            .saturating_mul(DEFAULT_BLOCK_FRAMES)
            .saturating_mul(4);
        self.obj()
            .set_blocksize(u32::try_from(block_bytes).unwrap_or(u32::MAX));

        let mut state = salsa_gst::lock(&self.state);
        state.source = Some(source);
        state.info = Some(info);
        state.caps = Some(caps);
        state.attach_meta = false;
        state.bpf = 0;
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = salsa_gst::lock(&self.state);
        state.source = None;
        state.info = None;
        state.caps = None;
        state.attach_meta = false;
        state.bpf = 0;
        Ok(())
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn do_seek(&self, segment: &mut gst::Segment) -> bool {
        if (segment.rate() - 1.0).abs() > f64::EPSILON {
            return false;
        }
        let frame = {
            let state = salsa_gst::lock(&self.state);
            let Some(info) = state.info.as_ref() else {
                return false;
            };
            let bpf = info.output_channels.saturating_mul(4);
            if bpf == 0 {
                return false;
            }
            match (segment.format(), segment.start()) {
                (gst::Format::Time, gst::GenericFormattedValue::Time(Some(time))) => {
                    ((u128::from(time.nseconds()) * u128::from(info.sample_rate)) / 1_000_000_000)
                        as u64
                }
                (gst::Format::Bytes, gst::GenericFormattedValue::Bytes(Some(bytes))) => {
                    u64::from(bytes) / bpf as u64
                }
                _ => return false,
            }
        };

        let mut state = salsa_gst::lock(&self.state);
        let Some(source) = state.source.as_mut() else {
            return false;
        };
        match source.seek(frame) {
            Ok(()) => true,
            Err(error) => {
                gst::error!(gst::CAT_RUST, "salsadamfsrc: seek failed: {error}");
                false
            }
        }
    }

    fn size(&self) -> Option<u64> {
        let state = salsa_gst::lock(&self.state);
        let info = state.info.as_ref()?;
        let frames = info.total_frames?;
        Some(frames * info.output_channels as u64 * 4)
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Duration(duration) = query.view_mut()
            && duration.format() == gst::Format::Time
        {
            let state = salsa_gst::lock(&self.state);
            if let Some(info) = state.info.as_ref()
                && let Some(frames) = info.total_frames
            {
                let nanos =
                    (u128::from(frames) * 1_000_000_000 / u128::from(info.sample_rate)) as u64;
                duration.set(gst::ClockTime::from_nseconds(nanos));
                return true;
            }
        }
        BaseSrcImplExt::parent_query(self, query)
    }

    fn caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        self.negotiated_caps(filter)
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let audio_info = gst_audio::AudioInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(gst::CAT_RUST, "invalid salsadamfsrc caps"))?;
        let mut state = salsa_gst::lock(&self.state);
        if let Some(info) = state.info.as_ref()
            && (audio_info.channels() != info.output_channels as u32
                || audio_info.rate() != info.sample_rate)
        {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsadamfsrc caps do not match the DAMF presentation"
            ));
        }
        state.attach_meta = objectaudio_meta::has_feature(caps);
        state.bpf = audio_info.channels() as usize * 4;
        Ok(())
    }

    fn create(
        &self,
        _offset: u64,
        _buffer: Option<&mut gst::BufferRef>,
        _length: u32,
    ) -> Result<CreateSuccess, gst::FlowError> {
        let (block, attach_meta, bpf) = {
            let mut state = salsa_gst::lock(&self.state);
            let block_frames = state.block_frames;
            let Some(source) = state.source.as_mut() else {
                return Err(gst::FlowError::NotNegotiated);
            };
            let block = source.read_block(block_frames).map_err(|error| {
                gst::error!(gst::CAT_RUST, "salsadamfsrc: read failed: {error}");
                gst::FlowError::Error
            })?;
            let Some(block) = block else {
                return Err(gst::FlowError::Eos);
            };
            (block, state.attach_meta, state.bpf)
        };

        let mut bytes = Vec::with_capacity(block.pcm.len() * 4);
        for sample in &block.pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let mut buffer = gst::Buffer::from_slice(bytes);
        {
            let buffer = buffer.get_mut().expect("fresh buffer is writable");
            let frame_time = |samples: u64| {
                gst::ClockTime::from_nseconds(
                    (u128::from(samples) * 1_000_000_000 / u128::from(block.sample_rate)) as u64,
                )
            };
            buffer.set_pts(frame_time(block.start_sample));
            buffer.set_duration(
                frame_time(block.start_sample + block.frames as u64)
                    - frame_time(block.start_sample),
            );
            buffer.set_offset(block.start_sample);
            buffer.set_offset_end(block.start_sample + block.frames as u64);
            if attach_meta {
                objectaudio_meta::add_spatial(buffer, &block.payload, bpf).map_err(|error| {
                    gst::error!(gst::CAT_RUST, "salsadamfsrc: cannot attach meta: {error}");
                    gst::FlowError::Error
                })?;
            }
        }
        Ok(CreateSuccess::NewBuffer(buffer))
    }
}
