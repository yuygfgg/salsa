//! Bounded rendered-PCM output.
//!
//! The decode/render pipeline hands buffers to a single writer thread through a
//! bounded channel. File and null writers are deterministic offline backends;
//! the CoreAudio backend uses `osxaudiosink` (which owns the device clock and
//! scheduling) from a second bounded `appsrc`.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use crate::clock::CarrierClock;
#[cfg(target_os = "macos")]
use crate::clock::{PlaybackClock, host_now_ns};
use crate::queue::BoundedAppSrc;
use anyhow::{Context, Result, bail};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSrc;

#[derive(Debug, Clone)]
pub enum OutputConfig {
    Wav(PathBuf),
    Null,
    /// Collect interleaved f32 PCM in memory. Used by the comparison gate.
    Memory,
    CoreAudio {
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
}

impl PcmChunk {
    pub fn frames(&self) -> u64 {
        (self.data.len() / self.info.bytes_per_frame()) as u64
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
    memory: Option<RenderedPcm>,
}

enum Inner {
    Wav(WavWriter),
    Null,
    Memory,
    #[cfg(target_os = "macos")]
    CoreAudio(CoreAudioWriter),
}

impl OutputSink {
    pub fn new(config: &OutputConfig) -> Result<Self> {
        Self::with_timing(config, Arc::default(), 50_000_000)
    }

    pub(crate) fn with_timing(
        config: &OutputConfig,
        stop: Arc<AtomicBool>,
        allowance_ns: u64,
    ) -> Result<Self> {
        let inner = match config {
            OutputConfig::Wav(path) => Inner::Wav(WavWriter::create(path)?),
            OutputConfig::Null => Inner::Null,
            OutputConfig::Memory => Inner::Memory,
            OutputConfig::CoreAudio { device } => {
                #[cfg(target_os = "macos")]
                {
                    Inner::CoreAudio(CoreAudioWriter::new(device.clone(), stop, allowance_ns)?)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (device, stop, allowance_ns);
                    bail!("the CoreAudio output is only available on macOS");
                }
            }
        };
        Ok(Self {
            inner,
            info: None,
            caps: None,
            frames: 0,
            discontinuity_count: 0,
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
            #[cfg(target_os = "macos")]
            if !format_changed && let Inner::CoreAudio(writer) = &mut self.inner {
                // A new playback session may follow a system-output change or
                // a removed device. Reopen the physical output so the service
                // follows the current default and does not keep a dead sink.
                writer.restart(&chunk.caps)?;
            }
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
            #[cfg(target_os = "macos")]
            Inner::CoreAudio(writer) => writer.write(chunk)?,
        }
        self.frames += frames;
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
        #[cfg(target_os = "macos")]
        if let Inner::CoreAudio(writer) = &mut self.inner {
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
            #[cfg(target_os = "macos")]
            Inner::CoreAudio(writer) => writer.finish()?,
        }
        Ok(OutputStats {
            frames: self.frames,
            info: self.info,
            discontinuities: self.discontinuity_count,
            memory: self.memory,
        })
    }
}

pub struct OutputStats {
    pub frames: u64,
    pub info: Option<AudioInfo>,
    /// Number of rendered chunks that followed a session reset/discontinuity.
    pub discontinuities: u64,
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

#[cfg(target_os = "macos")]
struct CoreAudioWriter {
    device: Option<String>,
    pipeline: gst::Pipeline,
    appsrc: BoundedAppSrc,
    capsfilter: gst::Element,
    clock: PlaybackClock,
    stop: Arc<AtomicBool>,
    allowance_ns: u64,
    configured: bool,
}

#[cfg(target_os = "macos")]
impl CoreAudioWriter {
    fn build(
        device: &Option<String>,
        stop: Arc<AtomicBool>,
    ) -> Result<(gst::Pipeline, BoundedAppSrc, gst::Element)> {
        let sink = gst::ElementFactory::make("osxaudiosink")
            .build()
            .context("osxaudiosink is unavailable")?;
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
        sink.set_property("sync", true);
        if let Some(device) = device {
            // `device` is an integer property; the persistent UID is exposed as
            // `unique-id` by the osxaudio sink and provider.
            sink.set_property("unique-id", device);
        }
        pipeline.add_many([appsrc.upcast_ref(), &convert, &resample, &filter, &sink])?;
        gst::Element::link_many([appsrc.upcast_ref(), &convert, &resample, &filter, &sink])?;
        // PTS already includes the service's scheduling budget. Do not add a
        // second live-pipeline latency on top of that budget.
        pipeline.set_latency(gst::ClockTime::ZERO);
        let appsrc = BoundedAppSrc::new(appsrc, &pipeline, stop, 512 * 1024);
        Ok((pipeline, appsrc, filter))
    }

    fn new(device: Option<String>, stop: Arc<AtomicBool>, allowance_ns: u64) -> Result<Self> {
        let (pipeline, appsrc, capsfilter) = Self::build(&device, stop.clone())?;
        Ok(Self {
            device,
            pipeline,
            appsrc,
            capsfilter,
            clock: PlaybackClock::default(),
            stop,
            allowance_ns,
            configured: false,
        })
    }

    fn restart(&mut self, caps: &gst::Caps) -> Result<()> {
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
            .context("cannot start the CoreAudio output pipeline")?;
        self.configured = true;
        Ok(())
    }

    fn write(&mut self, chunk: PcmChunk) -> Result<()> {
        if !self.configured {
            return Ok(());
        }
        self.appsrc.check()?;
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
        self.appsrc
            .push(buffer)
            .context("CoreAudio output rejected PCM")
    }

    fn finish(&mut self) -> Result<()> {
        if !self.configured {
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
                        bail!("CoreAudio output error: {}", error.error())
                    }
                    _ => {}
                }
            }
            if std::time::Instant::now() >= deadline {
                bail!("CoreAudio output timed out before EOS");
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for CoreAudioWriter {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Pull rendered samples from an `appsink` until EOS/error and forward them to
/// the writer thread through a bounded channel.
pub fn pull_samples(
    appsink: &gstreamer_app::AppSink,
    sender: std::sync::mpsc::SyncSender<PcmChunk>,
    stopped: impl Fn() -> bool,
    is_live: bool,
    idle_timeout: Duration,
    progress: Option<&std::sync::atomic::AtomicU64>,
    clock: Option<CarrierClock>,
) -> Result<u64> {
    use std::sync::atomic::Ordering;
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
                let mut chunk = sample_to_chunk(&sample)?;
                chunk.clock = clock;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
                CoreAudioWriter::build_with_sink(&None, Arc::clone(&stop), sink.clone().upcast())
                    .unwrap();
            let mut writer = CoreAudioWriter {
                device: None,
                pipeline,
                appsrc,
                capsfilter,
                clock: PlaybackClock::default(),
                stop,
                allowance_ns: 50_000_000,
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
