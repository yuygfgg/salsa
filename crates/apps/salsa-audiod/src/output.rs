//! Bounded rendered-PCM output.
//!
//! The decode/render pipeline hands buffers to a single writer thread through a
//! bounded channel. File and null writers are deterministic offline backends;
//! the physical backend uses `osxaudiosink` on macOS or a PulseAudio/ALSA sink
//! on Linux from a second bounded `appsrc`.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::clock::{CarrierClock, PlaybackClock, host_now_ns};
use crate::queue::BoundedAppSrc;
use anyhow::{Context, Result, bail};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use gst::prelude::*;
use gstreamer as gst;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use gstreamer_app::AppSrc;

#[derive(Debug, Clone)]
pub enum OutputConfig {
    Wav(PathBuf),
    Null,
    /// Collect interleaved f32 PCM in memory. Used by the comparison gate.
    Memory,
    /// A physical audio sink. macOS uses `osxaudiosink`, Linux uses ALSA.
    Physical {
        device: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioInfo {
    pub rate: u32,
    pub channels: u32,
}

impl AudioInfo {
    pub const fn bytes_per_frame(self) -> usize {
        self.channels as usize * 4
    }
}

#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub info: AudioInfo,
    pub caps: gst::Caps,
    pub clock: Option<CarrierClock>,
    pub pts: Option<gst::ClockTime>,
    pub duration: Option<gst::ClockTime>,
    pub data: Vec<u8>,
    pub discont: bool,
    /// Service session epoch. Chunks from an older epoch are discarded.
    pub epoch: u64,
}

impl PcmChunk {
    pub fn frames(&self) -> u64 {
        (self.data.len() / self.info.bytes_per_frame()) as u64
    }
}

/// Immutable identity for one pull thread. Stop and join that thread before
/// flushing the pipeline and starting a puller for the next session.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PcmSession {
    pub epoch: u64,
    pub clock: Option<CarrierClock>,
}

impl PcmSession {
    fn chunk(self, sample: &gst::Sample) -> Result<PcmChunk> {
        let mut chunk = sample_to_chunk(sample)?;
        chunk.epoch = self.epoch;
        chunk.clock = self.clock;
        Ok(chunk)
    }
}

#[derive(Debug, Default, Clone)]
pub struct RenderedPcm {
    pub rate: u32,
    pub channels: u32,
    pub samples: Vec<f32>,
    /// Output frame indices where a new session/discontinuity began.
    pub discontinuities: Vec<u64>,
}

impl RenderedPcm {
    pub fn frames(&self) -> u64 {
        self.samples.len() as u64 / self.channels.max(1) as u64
    }
}

pub struct OutputSink {
    inner: Inner,
    info: Option<AudioInfo>,
    caps: Option<gst::Caps>,
    frames: u64,
    discontinuity_count: u64,
    recoveries: u64,
    late_buffers: u64,
    max_lateness_ns: u64,
    memory: Option<RenderedPcm>,
}

enum Inner {
    Wav(WavWriter),
    Null,
    Memory,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Physical(PhysicalWriter),
}

impl OutputSink {
    pub fn new(config: &OutputConfig) -> Result<Self> {
        Self::with_timing(config, Arc::default(), 50_000_000, Duration::from_secs(2))
    }

    pub(crate) fn with_timing(
        config: &OutputConfig,
        stop: Arc<AtomicBool>,
        allowance_ns: u64,
        push_timeout: Duration,
    ) -> Result<Self> {
        let inner = match config {
            OutputConfig::Wav(path) => Inner::Wav(WavWriter::create(path)?),
            OutputConfig::Null => Inner::Null,
            OutputConfig::Memory => Inner::Memory,
            OutputConfig::Physical { device } => {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                {
                    Inner::Physical(PhysicalWriter::new(
                        device.clone(),
                        stop,
                        allowance_ns,
                        push_timeout,
                    )?)
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                {
                    let _ = (device, stop, allowance_ns, push_timeout);
                    bail!("the physical output is only available on macOS and Linux");
                }
            }
        };
        Ok(Self {
            inner,
            info: None,
            caps: None,
            frames: 0,
            discontinuity_count: 0,
            recoveries: 0,
            late_buffers: 0,
            max_lateness_ns: 0,
            memory: matches!(config, OutputConfig::Memory).then(RenderedPcm::default),
        })
    }

    pub fn write(&mut self, chunk: PcmChunk) -> Result<()> {
        let format_changed = self.caps.as_ref() != Some(&chunk.caps);
        if format_changed {
            self.configure(chunk.info, &chunk.caps)?;
        }
        if chunk.discont {
            self.discontinuity_count += 1;
        }
        let frames = chunk.frames();
        match &mut self.inner {
            Inner::Wav(writer) => writer.write(&chunk.data)?,
            Inner::Null => {}
            Inner::Memory => {
                let memory = self.memory.as_mut().expect("memory writer was requested");
                if chunk.discont {
                    memory.discontinuities.push(memory.frames());
                }
                let (samples, rest) = chunk.data.as_chunks::<4>();
                if !rest.is_empty() {
                    bail!("rendered PCM is not a multiple of one f32 sample");
                }
                memory
                    .samples
                    .extend(samples.iter().map(|bytes| f32::from_le_bytes(*bytes)));
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Inner::Physical(writer) => {
                // A full output queue means the physical device stopped
                // consuming. Rebuild the sink and drop the late buffer instead
                // of exiting the service.
                if writer.write(chunk)? {
                    self.recoveries += 1;
                    return Ok(());
                }
            }
        }
        self.frames += frames;
        Ok(())
    }

    /// Discard queued physical PCM and restart the output clock.
    ///
    /// The service calls this when the carrier session changes so pre-seek
    /// audio cannot leak into the new timeline.
    pub fn flush(&mut self) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Inner::Physical(writer) = &mut self.inner
            && writer.configured
        {
            let caps = self
                .caps
                .clone()
                .context("physical output has no negotiated caps")?;
            writer.restart(&caps)?;
        }
        Ok(())
    }

    fn configure(&mut self, info: AudioInfo, caps: &gst::Caps) -> Result<()> {
        self.info = Some(info);
        self.caps = Some(caps.clone());
        if let Inner::Memory = self.inner
            && let Some(memory) = self.memory.as_mut()
        {
            memory.rate = info.rate;
            memory.channels = info.channels;
        }
        if let Inner::Wav(writer) = &mut self.inner {
            writer.configure(info)?;
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Inner::Physical(writer) = &mut self.inner {
            if writer.configured {
                writer.restart(caps)?;
            } else {
                writer.configure(caps)?;
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<OutputStats> {
        match &mut self.inner {
            Inner::Wav(writer) => writer.finish()?,
            Inner::Null | Inner::Memory => {}
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Inner::Physical(writer) => writer.finish()?,
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Inner::Physical(writer) = &self.inner {
            let (late_buffers, max_lateness_ns) = writer.clock_stats();
            self.late_buffers = late_buffers;
            self.max_lateness_ns = max_lateness_ns;
        }
        Ok(OutputStats {
            frames: self.frames,
            info: self.info,
            discontinuities: self.discontinuity_count,
            recoveries: self.recoveries,
            late_buffers: self.late_buffers,
            max_lateness_ns: self.max_lateness_ns,
            memory: self.memory,
        })
    }
}

pub struct OutputStats {
    pub frames: u64,
    pub info: Option<AudioInfo>,
    /// Number of rendered chunks that followed a session reset/discontinuity.
    pub discontinuities: u64,
    /// Physical output stalls recovered by rebuilding the sink.
    pub recoveries: u64,
    /// Late buffers that were rebased instead of aborting playback.
    pub late_buffers: u64,
    /// Worst lateness observed by the output scheduler.
    pub max_lateness_ns: u64,
    pub memory: Option<RenderedPcm>,
}

/// Minimal IEEE-float WAV writer. The WAV container is not part of the codec
/// path; it exists so an offline run can be inspected with any audio tool.
struct WavWriter {
    file: Option<File>,
    data_bytes: u32,
    info: Option<AudioInfo>,
}

impl WavWriter {
    fn create(path: &PathBuf) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Self {
            file: Some(file),
            data_bytes: 0,
            info: None,
        })
    }

    fn configure(&mut self, info: AudioInfo) -> Result<()> {
        self.info = Some(info);
        let file = self.file.as_mut().expect("WAV file is open");
        let block_align = info.channels * 4;
        let byte_rate = info.rate * block_align;
        file.write_all(b"RIFF")?;
        file.write_all(&0u32.to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&3u16.to_le_bytes())?; // IEEE float
        file.write_all(&(info.channels as u16).to_le_bytes())?;
        file.write_all(&info.rate.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&(block_align as u16).to_le_bytes())?;
        file.write_all(&32u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&0u32.to_le_bytes())?;
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        let file = self.file.as_mut().expect("WAV file is open");
        file.write_all(data)?;
        self.data_bytes = self
            .data_bytes
            .checked_add(data.len() as u32)
            .context("WAV output exceeds 4 GiB")?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&(self.data_bytes + 36).to_le_bytes())?;
        file.seek(SeekFrom::Start(40))?;
        file.write_all(&self.data_bytes.to_le_bytes())?;
        file.flush()?;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PhysicalWriter {
    device: Option<String>,
    pipeline: gst::Pipeline,
    appsrc: BoundedAppSrc,
    capsfilter: gst::Element,
    clock: PlaybackClock,
    stop: Arc<AtomicBool>,
    allowance_ns: u64,
    push_timeout: Duration,
    accumulated_late_buffers: u64,
    accumulated_max_lateness_ns: u64,
    configured: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl PhysicalWriter {
    fn build(
        device: &Option<String>,
        stop: Arc<AtomicBool>,
    ) -> Result<(gst::Pipeline, BoundedAppSrc, gst::Element)> {
        let sink = physical_sink(device)?;
        Self::build_with_sink(device, stop, sink)
    }

    fn build_with_sink(
        device: &Option<String>,
        stop: Arc<AtomicBool>,
        sink: gst::Element,
    ) -> Result<(gst::Pipeline, BoundedAppSrc, gst::Element)> {
        let pipeline = gst::Pipeline::new();
        let appsrc = gst::ElementFactory::make("appsrc")
            .build()
            .context("appsrc is unavailable")?
            .downcast::<AppSrc>()
            .unwrap();
        appsrc.set_property("format", gst::Format::Time);
        appsrc.set_property("is-live", true);
        appsrc.set_property("do-timestamp", false);
        appsrc.set_property("stream-type", gstreamer_app::AppStreamType::Stream);
        let convert = gst::ElementFactory::make("audioconvert")
            .build()
            .context("audioconvert is unavailable")?;
        let resample = gst::ElementFactory::make("audioresample")
            .build()
            .context("audioresample is unavailable")?;
        let filter = gst::ElementFactory::make("capsfilter")
            .build()
            .context("capsfilter is unavailable")?;
        // On Linux, do not let the sink provide the pipeline clock. The
        // carrier is timestamped from CLOCK_MONOTONIC; using the
        // PulseAudio/PipeWire stream clock here made Bluetooth sinks drift
        // away from that timeline and fall silent. With `provide-clock=false`
        // GStreamer uses the system monotonic clock. Disable audio-clock
        // slaving as well so GStreamer does not try to resync against the
        // sink's buffered sample position; `sync=true` still honours the
        // carrier timestamps and preserves audio/video timing.
        #[cfg(target_os = "linux")]
        {
            sink.set_property("provide-clock", false);
            sink.set_property_from_str("slave-method", "none");
            sink.set_property("sync", true);
        }
        #[cfg(target_os = "macos")]
        sink.set_property("sync", true);
        configure_sink_device(&sink, device)?;
        pipeline.add_many([appsrc.upcast_ref(), &convert, &resample, &filter, &sink])?;
        gst::Element::link_many([appsrc.upcast_ref(), &convert, &resample, &filter, &sink])?;
        // PTS already includes the service's scheduling budget. Do not add a
        // second live-pipeline latency on top of that budget.
        pipeline.set_latency(gst::ClockTime::ZERO);
        let appsrc = BoundedAppSrc::new(appsrc, &pipeline, stop, 512 * 1024);
        Ok((pipeline, appsrc, filter))
    }

    fn new(
        device: Option<String>,
        stop: Arc<AtomicBool>,
        allowance_ns: u64,
        push_timeout: Duration,
    ) -> Result<Self> {
        let (pipeline, appsrc, capsfilter) = Self::build(&device, stop.clone())?;
        Ok(Self {
            device,
            pipeline,
            appsrc,
            capsfilter,
            clock: PlaybackClock::default(),
            stop,
            allowance_ns,
            push_timeout,
            accumulated_late_buffers: 0,
            accumulated_max_lateness_ns: 0,
            configured: false,
        })
    }

    fn restart(&mut self, caps: &gst::Caps) -> Result<()> {
        self.record_clock_stats();
        let _ = self.pipeline.set_state(gst::State::Null);
        let (pipeline, appsrc, capsfilter) = Self::build(&self.device, self.stop.clone())?;
        self.pipeline = pipeline;
        self.appsrc = appsrc;
        self.capsfilter = capsfilter;
        self.clock = PlaybackClock::default();
        self.configured = false;
        self.configure(caps)
    }

    fn configure(&mut self, caps: &gst::Caps) -> Result<()> {
        self.capsfilter.set_property("caps", caps);
        self.appsrc.source().set_caps(Some(caps));
        self.pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the physical output pipeline")?;
        self.configured = true;
        Ok(())
    }

    fn write(&mut self, chunk: PcmChunk) -> Result<bool> {
        if !self.configured {
            return Ok(false);
        }
        let caps = chunk.caps.clone();
        if let Err(error) = self.appsrc.check() {
            if self.stop.load(Ordering::Relaxed) {
                return Err(error);
            }
            eprintln!("salsa-audiod: physical output failed ({error:#}); reinitialising the sink");
            return match self.restart(&caps) {
                Ok(()) => Ok(true),
                Err(_) => Err(error),
            };
        }
        // Sample both clocks together. This also follows rate corrections made
        // by the device clock rather than assuming it runs at the host rate.
        let before = host_now_ns();
        let running = self
            .pipeline
            .current_running_time()
            .context("physical output has no running clock")?;
        let after = host_now_ns();
        let pts = self.clock.schedule(
            chunk
                .pts
                .context("rendered PCM has no presentation timestamp")?,
            chunk.clock,
            before + (after - before) / 2,
            running,
            self.allowance_ns,
        )?;
        let mut buffer = gst::Buffer::from_slice(chunk.data);
        {
            let buffer = buffer.get_mut().expect("fresh buffer is writable");
            buffer.set_pts(Some(pts));
            buffer.set_duration(chunk.duration);
            if chunk.discont {
                buffer.set_flags(gst::BufferFlags::DISCONT);
            }
        }
        match self
            .appsrc
            .push_with_timeout(buffer, Some(self.push_timeout))
        {
            Ok(true) => Ok(false),
            Ok(false) => {
                eprintln!("salsa-audiod: physical output queue stalled; reinitialising the sink");
                self.restart(&caps)?;
                Ok(true)
            }
            Err(error) => {
                if self.stop.load(Ordering::Relaxed) {
                    return Err(error);
                }
                eprintln!(
                    "salsa-audiod: physical output failed ({error:#}); reinitialising the sink"
                );
                match self.restart(&caps) {
                    Ok(()) => Ok(true),
                    Err(_) => Err(error),
                }
            }
        }
    }

    fn record_clock_stats(&mut self) {
        let (late, max) = self.clock.stats();
        self.accumulated_late_buffers = self.accumulated_late_buffers.saturating_add(late);
        self.accumulated_max_lateness_ns = self.accumulated_max_lateness_ns.max(max);
    }

    fn clock_stats(&self) -> (u64, u64) {
        let (late, max) = self.clock.stats();
        (
            self.accumulated_late_buffers.saturating_add(late),
            self.accumulated_max_lateness_ns.max(max),
        )
    }

    fn finish(&mut self) -> Result<()> {
        if !self.configured {
            return Ok(());
        }
        if self.stop.load(Ordering::Relaxed) {
            let _ = self.pipeline.set_state(gst::State::Null);
            return Ok(());
        }
        self.appsrc.end_of_stream()?;
        let bus = self.pipeline.bus().context("output pipeline has no bus")?;
        let deadline = std::time::Instant::now()
            + Duration::from_secs(10)
            + Duration::from_nanos(self.allowance_ns);
        loop {
            self.appsrc.check()?;
            if let Some(message) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(50),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) {
                match message.view() {
                    gst::MessageView::Eos(_) => return Ok(()),
                    gst::MessageView::Error(error) => {
                        bail!("physical output error: {}", error.error())
                    }
                    _ => {}
                }
            }
            if std::time::Instant::now() >= deadline {
                bail!("physical output timed out before EOS");
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for PhysicalWriter {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(target_os = "macos")]
fn physical_sink(_device: &Option<String>) -> Result<gst::Element> {
    gst::ElementFactory::make("osxaudiosink")
        .build()
        .context("osxaudiosink is unavailable")
}

#[cfg(target_os = "linux")]
fn physical_sink(device: &Option<String>) -> Result<gst::Element> {
    let (factory, _) = linux_sink_target(device);
    gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("{factory} is unavailable"))
}

#[cfg(target_os = "linux")]
fn linux_sink_target(device: &Option<String>) -> (&'static str, Option<String>) {
    match device.as_deref() {
        Some(value) if value.starts_with("pipewire:") => {
            ("pipewiresink", Some(value["pipewire:".len()..].to_string()))
        }
        Some(value) if value.starts_with("pulse:") => {
            ("pulsesink", Some(value["pulse:".len()..].to_string()))
        }
        Some(value) if value.starts_with("alsa:") => {
            ("alsasink", Some(value["alsa:".len()..].to_string()))
        }
        Some("system") | Some("default") | Some("") | None => {
            if pulse_server_available() {
                ("pulsesink", None)
            } else {
                ("alsasink", None)
            }
        }
        Some(value) => ("alsasink", Some(value.to_string())),
    }
}

#[cfg(target_os = "linux")]
fn pulse_server_available() -> bool {
    if std::env::var_os("PULSE_SERVER").is_some_and(|value| !value.is_empty()) {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| std::path::Path::new(&dir).join("pulse/native").exists())
        .unwrap_or(false)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn configure_sink_device(sink: &gst::Element, device: &Option<String>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if let Some(device) = device {
            // `device` is an integer property; the persistent UID is exposed as
            // `unique-id` by the osxaudio sink and provider.
            sink.set_property("unique-id", device);
        }
        let _ = sink;
    }
    #[cfg(target_os = "linux")]
    {
        let (factory, target) = linux_sink_target(device);
        if let Some(target) = target {
            if factory == "pipewiresink" {
                sink.set_property("target-object", target);
            } else {
                sink.set_property("device", target);
            }
        }
    }
    Ok(())
}

/// Pull rendered samples from an `appsink` until EOS/error and forward them to
/// the writer thread through a bounded channel.
pub(crate) fn pull_samples(
    appsink: &gstreamer_app::AppSink,
    sender: std::sync::mpsc::SyncSender<PcmChunk>,
    stopped: impl Fn() -> bool,
    is_live: bool,
    idle_timeout: Duration,
    progress: Option<&std::sync::atomic::AtomicU64>,
    session: PcmSession,
) -> Result<u64> {
    use std::sync::mpsc::TrySendError;
    let mut last_sample = std::time::Instant::now();
    let mut frames = 0u64;
    loop {
        if stopped() {
            break;
        }
        match appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            Some(sample) => {
                last_sample = std::time::Instant::now();
                let chunk = session.chunk(&sample)?;
                frames += chunk.frames();
                if let Some(progress) = progress {
                    progress.fetch_add(1, Ordering::Relaxed);
                }
                match sender.try_send(chunk) {
                    Ok(()) => {}
                    Err(TrySendError::Full(mut chunk)) => loop {
                        if stopped() {
                            break;
                        }
                        match sender.try_send(chunk) {
                            Ok(()) => break,
                            Err(TrySendError::Full(pending)) => {
                                chunk = pending;
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                bail!("output writer stopped")
                            }
                        }
                    },
                    Err(TrySendError::Disconnected(_)) => bail!("output writer stopped"),
                }
            }
            None => {
                if appsink.is_eos() {
                    break;
                }
                if !is_live && last_sample.elapsed() > idle_timeout {
                    bail!("decode/render pipeline timed out before EOS");
                }
            }
        }
    }
    Ok(frames)
}

pub fn sample_to_chunk(sample: &gst::Sample) -> Result<PcmChunk> {
    let caps = sample.caps().context("rendered sample has no caps")?;
    let info =
        gstreamer_audio::AudioInfo::from_caps(caps).context("invalid rendered audio caps")?;
    if info.format() != gstreamer_audio::AudioFormat::F32le
        || info.layout() != gstreamer_audio::AudioLayout::Interleaved
    {
        bail!("renderer must produce interleaved F32LE PCM");
    }
    let buffer = sample.buffer().context("rendered sample has no buffer")?;
    let map = buffer
        .map_readable()
        .context("cannot map rendered buffer")?;
    Ok(PcmChunk {
        info: AudioInfo {
            rate: info.rate(),
            channels: info.channels(),
        },
        caps: caps.to_owned(),
        clock: None,
        pts: buffer.pts(),
        duration: buffer.duration(),
        data: map.as_slice().to_vec(),
        discont: buffer.flags().contains(gst::BufferFlags::DISCONT),
        epoch: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer_app::AppSink;
    use std::sync::{atomic::AtomicU64, mpsc};

    struct PcmPipeline {
        pipeline: gst::Pipeline,
        source: AppSrc,
        sink: AppSink,
    }

    impl PcmPipeline {
        fn new() -> Self {
            gst::init().unwrap();
            let pipeline = gst::Pipeline::new();
            let caps: gst::Caps =
                "audio/x-raw,format=F32LE,layout=interleaved,channels=2,rate=48000"
                    .parse()
                    .unwrap();
            let source = AppSrc::builder()
                .format(gst::Format::Time)
                .stream_type(gstreamer_app::AppStreamType::Seekable)
                .caps(&caps)
                .build();
            source.set_callbacks(
                gstreamer_app::AppSrcCallbacks::builder()
                    .seek_data(|_, _| true)
                    .build(),
            );
            let sink = AppSink::builder().sync(false).max_buffers(8).build();
            pipeline
                .add_many([source.upcast_ref::<gst::Element>(), sink.upcast_ref()])
                .unwrap();
            source.link(&sink).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            Self {
                pipeline,
                source,
                sink,
            }
        }

        fn push(&self, value: f32) {
            let mut buffer = gst::Buffer::from_slice(value.to_le_bytes().repeat(64));
            let data = buffer.get_mut().unwrap();
            data.set_pts(gst::ClockTime::ZERO);
            data.set_flags(gst::BufferFlags::DISCONT);
            self.source.push_buffer(buffer).unwrap();
        }

        fn seek(&self) {
            self.pipeline
                .seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::ZERO,
                )
                .unwrap();
        }
    }

    impl Drop for PcmPipeline {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    #[test]
    fn pulled_sample_keeps_its_identity_across_consecutive_seeks() {
        let pipeline = PcmPipeline::new();
        let old_session = PcmSession {
            epoch: 1,
            clock: Some(CarrierClock {
                position: gst::ClockTime::ZERO,
                host_ns: 111,
            }),
        };
        pipeline.push(0.25);
        let old_sample = pipeline
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(2))
            .unwrap();
        let (resume, paused) = mpsc::channel();
        let old_puller = std::thread::spawn(move || {
            // Hold the old sample outside appsink until both seeks finish.
            paused.recv_timeout(Duration::from_secs(2)).unwrap();
            old_session.chunk(&old_sample).unwrap()
        });
        pipeline.seek();
        pipeline.seek();
        let new_session = PcmSession {
            epoch: 3,
            clock: Some(CarrierClock {
                position: gst::ClockTime::ZERO,
                host_ns: 333,
            }),
        };
        pipeline.push(0.75);
        let sample = pipeline
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(2))
            .unwrap();
        let current = new_session.chunk(&sample).unwrap();
        resume.send(()).unwrap();
        let old = old_puller.join().unwrap();

        assert_eq!(old.epoch, 1);
        assert_eq!(old.clock.unwrap().host_ns, 111);
        assert_eq!(f32::from_le_bytes(old.data[..4].try_into().unwrap()), 0.25);
        assert_eq!(current.epoch, 3);
        assert_eq!(current.clock.unwrap().host_ns, 333);
        assert_eq!(
            f32::from_le_bytes(current.data[..4].try_into().unwrap()),
            0.75
        );
    }

    #[test]
    fn puller_handoff_cancels_output_backpressure_and_reuses_the_pipeline() {
        let pipeline = PcmPipeline::new();
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let worker_stop = stop.clone();
        let worker_progress = progress.clone();
        let sink = pipeline.sink.clone();
        let old_sender = sender.clone();
        let (done, completed) = mpsc::channel();
        let old_puller = std::thread::spawn(move || {
            let result = pull_samples(
                &sink,
                old_sender,
                || worker_stop.load(Ordering::Relaxed),
                false,
                Duration::from_secs(2),
                Some(&worker_progress),
                PcmSession {
                    epoch: 1,
                    clock: None,
                },
            );
            done.send(result).unwrap();
        });
        pipeline.push(0.25);
        pipeline.push(0.5);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while progress.load(Ordering::Relaxed) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "puller did not fill the queue"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // The second chunk cannot enter the full writer queue. Stopping the
        // puller must still complete before the native seek starts.
        stop.store(true, Ordering::Relaxed);
        completed
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        old_puller.join().unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap().epoch,
            1
        );
        assert!(receiver.try_recv().is_err());

        pipeline.seek();
        let sink = pipeline.sink.clone();
        let new_puller = std::thread::spawn(move || {
            pull_samples(
                &sink,
                sender,
                || false,
                false,
                Duration::from_secs(2),
                None,
                PcmSession {
                    epoch: 2,
                    clock: None,
                },
            )
        });
        pipeline.push(0.75);
        pipeline.source.end_of_stream().unwrap();
        let current = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(current.epoch, 2);
        assert_eq!(
            f32::from_le_bytes(current.data[..4].try_into().unwrap()),
            0.75
        );
        new_puller.join().unwrap().unwrap();
    }

    #[test]
    fn pcm_chunk_preserves_the_channel_layout_caps() {
        gst::init().unwrap();
        for (channels, mask) in [(6, 0x3fu64), (8, 0xc3f), (12, 0x33c3f)] {
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 48000i32)
                .field("channels", channels)
                .field("channel-mask", gst::Bitmask(mask))
                .build();
            let buffer = gst::Buffer::with_size(channels as usize * 4 * 16).unwrap();
            let sample = gst::Sample::builder().caps(&caps).buffer(&buffer).build();
            let chunk = sample_to_chunk(&sample).unwrap();
            assert_eq!(chunk.info.channels, channels as u32);
            assert_eq!(chunk.caps, caps);
            let info = gstreamer_audio::AudioInfo::from_caps(&chunk.caps).unwrap();
            assert_eq!(info.channels(), channels as u32);
            assert_eq!(
                chunk
                    .caps
                    .structure(0)
                    .unwrap()
                    .get::<gst::Bitmask>("channel-mask")
                    .unwrap()
                    .0,
                mask
            );
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod coreaudio_tests {
    use super::*;
    use gstreamer_app::AppSink;

    #[test]
    fn physical_output_negotiates_the_full_multichannel_layout() {
        gst::init().unwrap();
        for (channels, mask) in [(6, 0x3fu64), (8, 0xc3f), (12, 0x33c3f)] {
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", 48000i32)
                .field("channels", channels)
                .field("channel-mask", gst::Bitmask(mask))
                .build();
            let mut buffer = gst::Buffer::with_size(channels as usize * 4 * 128).unwrap();
            buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
            let sample = gst::Sample::builder().caps(&caps).buffer(&buffer).build();
            let chunk = sample_to_chunk(&sample).unwrap();
            assert_eq!(chunk.caps, caps);
            let sink = AppSink::builder().build();
            let stop = Arc::default();
            let (pipeline, appsrc, capsfilter) =
                PhysicalWriter::build_with_sink(&None, Arc::clone(&stop), sink.clone().upcast())
                    .unwrap();
            let mut writer = PhysicalWriter {
                device: None,
                pipeline,
                appsrc,
                capsfilter,
                clock: PlaybackClock::default(),
                stop,
                allowance_ns: 50_000_000,
                push_timeout: Duration::from_secs(2),
                accumulated_late_buffers: 0,
                accumulated_max_lateness_ns: 0,
                configured: false,
            };
            writer.configure(&caps).unwrap();
            writer.write(chunk).unwrap();
            let output = sink
                .try_pull_sample(gst::ClockTime::from_seconds(2))
                .expect("negotiated PCM");
            let info = gstreamer_audio::AudioInfo::from_caps(output.caps().unwrap()).unwrap();
            assert_eq!(info.channels(), channels as u32);
            assert_eq!(
                output
                    .caps()
                    .unwrap()
                    .structure(0)
                    .unwrap()
                    .get::<gst::Bitmask>("channel-mask")
                    .unwrap()
                    .0,
                mask
            );
        }
    }
}
