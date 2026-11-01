//! Codec detection, output configuration and GStreamer pipeline construction.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gstreamer as gst;
use gstreamer::prelude::*;
use objectaudio_ir::STANDARD_LAYOUTS;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Eac3,
    TrueHd,
    Damf,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Self::Eac3 => "E-AC-3 JOC",
            Self::TrueHd => "TrueHD Atmos",
            Self::Damf => "DAMF Master",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Eac3 => "EC3",
            Self::TrueHd => "TrueHD",
            Self::Damf => "DAMF",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    #[default]
    Speaker,
    Binaural,
}

impl OutputMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Speaker => "Speaker",
            Self::Binaural => "Binaural",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinauralConfig {
    pub model: Option<PathBuf>,
    pub filterbank: u32,
    pub profile: String,
}

impl Default for BinauralConfig {
    fn default() -> Self {
        Self {
            model: None,
            filterbank: 1,
            profile: "mid".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSelection {
    #[default]
    Auto,
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputConfig {
    pub mode: OutputMode,
    pub layout: String,
    pub binaural: BinauralConfig,
    pub device: DeviceSelection,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mode: OutputMode::Speaker,
            layout: "7.1.4".to_string(),
            binaural: BinauralConfig::default(),
            device: DeviceSelection::Auto,
        }
    }
}

#[derive(Clone)]
pub struct DeviceInfo {
    pub display_name: String,
    pub max_channels: Option<u32>,
    pub is_default: bool,
    pub device: gst::Device,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("cannot read {path}: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("cannot detect a supported codec in {0}")]
    UnsupportedCodec(PathBuf),
    #[error("GStreamer element '{factory}' is unavailable: {reason}")]
    MissingElement { factory: String, reason: String },
    #[error("binaural playback needs a Rosella .personalized_headphone model")]
    ModelRequired,
    #[error("the selected audio device is no longer available: {0}")]
    DeviceMissing(String),
    #[error("cannot build the playback pipeline: {0}")]
    Build(String),
}

impl From<PipelineError> for String {
    fn from(error: PipelineError) -> Self {
        error.to_string()
    }
}

pub fn detect_codec(path: &Path) -> Result<Codec, PipelineError> {
    let mut file = File::open(path).map_err(|error| PipelineError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut bytes = vec![0u8; 64 * 1024];
    let read = file.read(&mut bytes).map_err(|error| PipelineError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let bytes = &bytes[..read];
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    if contains_truehd_sync(bytes) {
        return Ok(Codec::TrueHd);
    }
    if find_eac3_sync(bytes).is_some() {
        return Ok(Codec::Eac3);
    }
    match extension.as_deref() {
        Some("ec3" | "eac3") => Ok(Codec::Eac3),
        Some("mlp" | "thd" | "truehd") => Ok(Codec::TrueHd),
        Some("atmos" | "damf") => Ok(Codec::Damf),
        _ => Err(PipelineError::UnsupportedCodec(path.to_path_buf())),
    }
}

fn find_eac3_sync(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(2)
        .enumerate()
        .find(|(offset, pair)| offset % 2 == 0 && *pair == [0x0B, 0x77])
        .map(|(offset, _)| offset)
}

fn contains_truehd_sync(bytes: &[u8]) -> bool {
    bytes
        .windows(4)
        .any(|window| window == [0xF8, 0x72, 0x6F, 0xBA])
}

pub fn enumerate_devices() -> Vec<DeviceInfo> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Sink"), None);
    if monitor.start().is_err() {
        return Vec::new();
    }
    let mut devices = Vec::new();
    for device in monitor.devices().into_iter() {
        let display_name = device.display_name().to_string();
        let max_channels = max_channels_for_device(&device);
        let is_default = device
            .properties()
            .and_then(|properties| properties.get::<bool>("is-default").ok())
            .unwrap_or(false);
        devices.push(DeviceInfo {
            display_name,
            max_channels,
            is_default,
            device,
        });
    }
    monitor.stop();
    if !devices.iter().any(|info| info.is_default)
        && let Some(first) = devices.first_mut()
    {
        first.is_default = true;
    }
    devices
}

fn max_channels_for_device(device: &gst::Device) -> Option<u32> {
    let device_max = device.caps().as_ref().and_then(max_channels_from_caps);
    let element_max = device.create_element(None).ok().and_then(|element| {
        let pad = element.static_pad("sink")?;
        let caps = pad.query_caps(None);
        max_channels_from_caps(&caps)
    });
    match (device_max, element_max) {
        (Some(device), Some(element)) => Some(device.min(element)),
        (Some(device), None) => Some(device),
        (None, Some(element)) => Some(element),
        (None, None) => None,
    }
}

fn max_channels_from_caps(caps: &gst::Caps) -> Option<u32> {
    let mut maximum: Option<u32> = None;
    for structure in caps.iter() {
        if let Ok(channels) = structure.get::<i32>("channels") {
            maximum = maximum.max(Some(channels.max(0) as u32));
        } else if let Ok(range) = structure.get::<gst::IntRange<i32>>("channels") {
            maximum = maximum.max(Some(range.max().max(0) as u32));
        }
    }
    maximum
}

pub fn layout_channel_count(name: &str) -> Option<usize> {
    STANDARD_LAYOUTS
        .iter()
        .find(|layout| layout.name == name)
        .map(|layout| layout.channels.len())
}

pub fn layout_names() -> Vec<&'static str> {
    STANDARD_LAYOUTS.iter().map(|layout| layout.name).collect()
}

/// Resolve the layout that the selected device can actually carry.
///
/// Returns the effective layout and an optional warning when the requested
/// layout had to be reduced. This is an object-rendered fallback, never a
/// core/downmix substream.
pub fn resolve_layout(
    requested: &str,
    device: &DeviceSelection,
    devices: &[DeviceInfo],
) -> (String, Option<String>) {
    let max_channels = match device {
        DeviceSelection::Named(name) => devices
            .iter()
            .find(|info| &info.display_name == name)
            .and_then(|info| info.max_channels),
        DeviceSelection::Auto => devices
            .iter()
            .find(|info| info.is_default)
            .or_else(|| devices.first())
            .and_then(|info| info.max_channels),
    };
    resolve_layout_for_channels(requested, max_channels)
}

pub fn resolve_layout_for_channels(
    requested: &str,
    max_channels: Option<u32>,
) -> (String, Option<String>) {
    let Some(requested_channels) = layout_channel_count(requested) else {
        return (
            "7.1.4".to_string(),
            Some(format!("Unknown layout '{requested}'; using 7.1.4.")),
        );
    };

    let Some(max_channels) = max_channels else {
        return (requested.to_string(), None);
    };
    if requested_channels as u32 <= max_channels {
        return (requested.to_string(), None);
    }

    let preferred = ["2.0", "5.1", "7.1", "5.1.4", "7.1.4", "9.1.4", "9.1.6"];
    let fallback = preferred
        .iter()
        .filter_map(|name| layout_channel_count(name).map(|channels| (*name, channels)))
        .filter(|(_, channels)| *channels as u32 <= max_channels)
        .max_by_key(|(_, channels)| *channels)
        .map(|(name, _)| name)
        .unwrap_or("2.0");

    let warning = if max_channels <= 2 {
        format!(
            "This device is stereo. Playing {fallback} (object-rendered); \
             connect a {requested} device to hear {requested}."
        )
    } else {
        format!(
            "This device carries {max_channels} channels. Playing {fallback} \
             (object-rendered); connect a {requested} device to hear {requested}."
        )
    };
    (fallback.to_string(), Some(warning))
}

pub struct PipelineParts {
    pub pipeline: gst::Pipeline,
    /// Parser element for codec streams; `None` for a DAMF source.
    pub parser: Option<gst::Element>,
    /// Element carrying the object-audio src pad. For DAMF this is the source.
    pub decoder: gst::Element,
    pub volume: gst::Element,
    pub codec: Codec,
}

fn element(factory: &str) -> Result<gst::Element, PipelineError> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|error| PipelineError::MissingElement {
            factory: factory.to_string(),
            reason: error.to_string(),
        })
}

pub fn build_pipeline(
    path: &Path,
    config: &OutputConfig,
    devices: &[DeviceInfo],
) -> Result<PipelineParts, PipelineError> {
    let codec = detect_codec(path)?;
    let pipeline = gst::Pipeline::new();

    let mut chain = Vec::new();
    let (parser, decoder) = match codec {
        Codec::Damf => {
            let source = element("salsadamfsrc")?;
            source.set_property("location", path.to_string_lossy().to_string());
            chain.push(source.clone());
            (None, source)
        }
        Codec::Eac3 | Codec::TrueHd => {
            let filesrc = element("filesrc")?;
            filesrc.set_property("location", path.to_string_lossy().to_string());
            filesrc.set_property("blocksize", 1_048_576u32);
            let (parser_name, decoder_name) = match codec {
                Codec::Eac3 => ("salsaeac3jocparse", "salsaeac3jocdec"),
                Codec::TrueHd => ("salsatruehdparse", "salsatruehdatmosdec"),
                Codec::Damf => unreachable!(),
            };
            let parser = element(parser_name)?;
            let decoder = element(decoder_name)?;
            chain.push(filesrc);
            chain.push(parser.clone());
            chain.push(decoder.clone());
            (Some(parser), decoder)
        }
    };

    let renderer = match config.mode {
        OutputMode::Speaker => {
            let renderer = element("salsaoar")?;
            renderer.set_property("layout", config.layout.as_str());
            renderer
        }
        OutputMode::Binaural => {
            let Some(model) = config.binaural.model.as_ref() else {
                return Err(PipelineError::ModelRequired);
            };
            let renderer = element("salsahrtfrender")?;
            renderer.set_property(
                "personalized-headphone",
                model.to_string_lossy().to_string(),
            );
            renderer.set_property("filterbank", config.binaural.filterbank);
            renderer.set_property("profile", config.binaural.profile.as_str());
            renderer
        }
    };

    let convert = element("audioconvert")?;
    let resample = element("audioresample")?;
    let capsfilter = element("capsfilter")?;
    let caps = match config.mode {
        OutputMode::Speaker => {
            let channels = layout_channel_count(&config.layout).unwrap_or(2);
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("channels", channels as i32)
                .build()
        }
        OutputMode::Binaural => gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("channels", 2i32)
            .build(),
    };
    capsfilter.set_property("caps", caps);

    let volume = element("volume")?;
    volume.set_property("volume", 1.0f64);

    let sink = if let Some(factory) = std::env::var_os("SALSA_SINK_OVERRIDE") {
        let factory = factory.to_string_lossy().into_owned();
        let sink = element(&factory)?;
        if factory == "fakesink" {
            let sync = std::env::var("SALSA_SINK_SYNC").as_deref() == Ok("true");
            sink.set_property("sync", sync);
        }
        sink
    } else {
        match &config.device {
            DeviceSelection::Auto => element("autoaudiosink")?,
            DeviceSelection::Named(name) => {
                let info = devices
                    .iter()
                    .find(|info| &info.display_name == name)
                    .ok_or_else(|| PipelineError::DeviceMissing(name.clone()))?;
                info.device
                    .create_element(Some("sink"))
                    .map_err(|error| PipelineError::Build(error.to_string()))?
            }
        }
    };

    chain.push(renderer);
    chain.push(convert);
    chain.push(resample);
    chain.push(capsfilter);
    chain.push(volume.clone());
    chain.push(sink);

    pipeline
        .add_many(chain.iter())
        .map_err(|error| PipelineError::Build(error.to_string()))?;
    gst::Element::link_many(chain.iter())
        .map_err(|error| PipelineError::Build(error.to_string()))?;

    Ok(PipelineParts {
        pipeline,
        parser,
        decoder,
        volume,
        codec,
    })
}

/// Scan a stream with the parser only and sum the timestamps/durations of the
/// parser output. This is the reliable fallback for raw TrueHD, where neither
/// the pipeline nor the discoverer knows the duration.
pub fn scan_duration(
    path: &Path,
    codec: Codec,
    cancel: &Arc<AtomicBool>,
) -> Option<gst::ClockTime> {
    let pipeline = gst::Pipeline::new();
    let (filesrc, feeder) = match codec {
        Codec::Damf => {
            let source = element("salsadamfsrc").ok()?;
            source.set_property("location", path.to_string_lossy().to_string());
            (None, source)
        }
        Codec::Eac3 | Codec::TrueHd => {
            let filesrc = element("filesrc").ok()?;
            filesrc.set_property("location", path.to_string_lossy().to_string());
            filesrc.set_property("blocksize", 8_388_608u32);
            let parser_name = match codec {
                Codec::Eac3 => "salsaeac3jocparse",
                Codec::TrueHd => "salsatruehdparse",
                Codec::Damf => unreachable!(),
            };
            let parser = element(parser_name).ok()?;
            pipeline.add_many([&filesrc, &parser]).ok()?;
            gst::Element::link_many([&filesrc, &parser]).ok()?;
            (Some(filesrc), parser)
        }
    };
    let sink = element("fakesink").ok()?;
    sink.set_property("sync", false);
    pipeline.add(&sink).ok()?;
    if filesrc.is_none() {
        pipeline.add(&feeder).ok()?;
    }
    gst::Element::link_many([&feeder, &sink]).ok()?;

    let total_ns = Arc::new(AtomicU64::new(0));
    let last_end_ns = Arc::new(AtomicU64::new(0));
    let total = total_ns.clone();
    let last_end = last_end_ns.clone();
    let pad = feeder.static_pad("src")?;
    let probe_id = pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(buffer) = info.buffer() {
            let duration = buffer.duration().map(|value| value.nseconds()).unwrap_or(0);
            total.fetch_add(duration, Ordering::Relaxed);
            if let Some(pts) = buffer.pts() {
                last_end.fetch_max(pts.nseconds().saturating_add(duration), Ordering::Relaxed);
            }
        }
        gst::PadProbeReturn::Ok
    })?;

    let bus = pipeline.bus()?;
    pipeline.set_state(gst::State::Playing).ok()?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(200),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            match message.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(_) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    pad.remove_probe(probe_id);
                    return None;
                }
                _ => {}
            }
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    pad.remove_probe(probe_id);
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let total = total_ns
        .load(Ordering::Relaxed)
        .max(last_end_ns.load(Ordering::Relaxed));
    (total > 0).then(|| gst::ClockTime::from_nseconds(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_without_a_warning_when_the_device_is_wide_enough() {
        assert_eq!(
            resolve_layout_for_channels("7.1.4", Some(12)),
            ("7.1.4".to_string(), None)
        );
        assert_eq!(
            resolve_layout_for_channels("7.1.4", None),
            ("7.1.4".to_string(), None)
        );
    }

    #[test]
    fn falls_back_to_the_largest_layout_the_device_carries() {
        let (layout, warning) = resolve_layout_for_channels("7.1.4", Some(2));
        assert_eq!(layout, "2.0");
        assert!(warning.unwrap().contains("stereo"));

        let (layout, warning) = resolve_layout_for_channels("7.1.4", Some(8));
        assert_eq!(layout, "7.1");
        assert!(warning.is_some());

        let (layout, warning) = resolve_layout_for_channels("9.1.6", Some(12));
        assert_eq!(layout, "7.1.4");
        assert!(warning.is_some());
    }

    #[test]
    fn an_unknown_layout_uses_the_perspective_default() {
        let (layout, warning) = resolve_layout_for_channels("nope", Some(12));
        assert_eq!(layout, "7.1.4");
        assert!(warning.unwrap().contains("Unknown layout"));
    }

    #[test]
    fn detects_codecs_by_magic_bytes_and_extension() {
        let directory = std::env::temp_dir();
        let ec3 = directory.join("salsa-gui-detect.ec3");
        let mlp = directory.join("salsa-gui-detect.mlp");
        let atmos = directory.join("salsa-gui-detect.atmos");
        std::fs::write(&ec3, [0x0B, 0x77, 0, 0]).unwrap();
        std::fs::write(&mlp, [0, 0, 0xF8, 0x72, 0x6F, 0xBA]).unwrap();
        std::fs::write(&atmos, b"version: 0.5.1\npresentations: []\n").unwrap();
        assert_eq!(detect_codec(&ec3).unwrap(), Codec::Eac3);
        assert_eq!(detect_codec(&mlp).unwrap(), Codec::TrueHd);
        assert_eq!(detect_codec(&atmos).unwrap(), Codec::Damf);
        let _ = std::fs::remove_file(ec3);
        let _ = std::fs::remove_file(mlp);
        let _ = std::fs::remove_file(atmos);
    }
}
