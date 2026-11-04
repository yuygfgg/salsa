//! Service orchestration: carrier source -> depacketizer -> GStreamer
//! decode/render -> bounded PCM output.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use gstreamer as gst;
use iec61937::{
    AccessUnit, CARRIER_SAMPLE_RATE, CarrierStats, Depacketizer, EAC3_BURST_BYTES,
    EAC3_CARRIER_BYTES_PER_FRAME, Eac3Stats, TRUEHD_CARRIER_BYTES_PER_FRAME, TrueHdStats,
};
use serde::Serialize;

use crate::carrier::{CarrierChunk, CarrierInput};
use crate::clock::{CarrierClock, host_ticks_to_ns};
use crate::ledger::DelayLedger;
use crate::output::{self, OutputConfig, OutputSink, PcmChunk, PcmSession, RenderedPcm};
use crate::pipeline::{
    self, Codec, DecodeRenderPipeline, DirectPipeline, PipelineConfig, RenderMode,
};

#[derive(Debug, Clone)]
pub enum CarrierSource {
    /// Offline IEC 61937 capture (`.spdif` fixture or recorded carrier).
    File(PathBuf),
    /// Shared-memory ring written by the HAL plug-in.
    Shared(String),
    /// Pre-built chunks for lifecycle tests and diagnostic injection.
    Scripted(Vec<CarrierChunk>),
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub source: CarrierSource,
    /// Require this codec; `None` selects the first codec found.
    pub codec: Option<Codec>,
    pub sample_rate: u32,
    pub render: RenderMode,
    pub output: OutputConfig,
    /// Fixed scheduling allowance in nanoseconds.
    pub scheduling_allowance_ns: u64,
    /// Measured physical output latency in nanoseconds.
    pub physical_output_latency_ns: u64,
    /// Offline idle timeout before declaring a pipeline stall.
    pub idle_timeout: Duration,
    /// Live output stall deadline before a deadline failure is counted.
    pub deadline_timeout: Duration,
    /// Live output stall threshold for a recovery warning.
    pub hard_stall_timeout: Duration,
    /// Optional stop flag (SIGINT or tests).
    pub stop: Arc<AtomicBool>,
    /// Publish the computed contract to the HAL shared control block.
    pub declare_latency: bool,
}

impl ServiceConfig {
    pub fn new(source: CarrierSource) -> Self {
        Self {
            source,
            codec: None,
            sample_rate: salsa_codec::SAMPLE_RATE,
            render: RenderMode::speaker("7.1.4"),
            output: OutputConfig::Null,
            scheduling_allowance_ns: 50_000_000,
            physical_output_latency_ns: 0,
            idle_timeout: Duration::from_secs(60),
            deadline_timeout: Duration::from_secs(2),
            hard_stall_timeout: Duration::from_secs(10),
            stop: Arc::new(AtomicBool::new(false)),
            declare_latency: false,
        }
    }

    pub fn live(&self) -> bool {
        matches!(self.source, CarrierSource::Shared(_))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TransportReport {
    pub carrier_bursts: u64,
    pub carrier_null_bursts: u64,
    pub carrier_unsupported_bursts: u64,
    pub carrier_malformed_bursts: u64,
    pub carrier_resynced_bytes: u64,
    pub eac3_frames: u64,
    pub eac3_corrupt_frames: u64,
    pub eac3_resynced_bytes: u64,
    pub truehd_access_units: u64,
    pub truehd_mat_frames: u64,
    pub truehd_malformed_mat_frames: u64,
    pub truehd_padding_bytes: u64,
    pub truehd_resets: u64,
    pub skipped_slots: u64,
}

impl TransportReport {
    fn new(
        carrier: CarrierStats,
        eac3: Eac3Stats,
        truehd: TrueHdStats,
        skipped_slots: u64,
    ) -> Self {
        Self {
            carrier_bursts: carrier.bursts,
            carrier_null_bursts: carrier.null_bursts,
            carrier_unsupported_bursts: carrier.unsupported_bursts,
            carrier_malformed_bursts: carrier.malformed_bursts,
            carrier_resynced_bytes: carrier.resynced_bytes,
            eac3_frames: eac3.frames,
            eac3_corrupt_frames: eac3.corrupt_frames,
            eac3_resynced_bytes: eac3.resynced_bytes,
            truehd_access_units: truehd.access_units,
            truehd_mat_frames: truehd.mat_frames,
            truehd_malformed_mat_frames: truehd.malformed_mat_frames,
            truehd_padding_bytes: truehd.padding_bytes,
            truehd_resets: truehd.resets,
            skipped_slots,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComparisonReport {
    pub codec: String,
    pub render: String,
    pub carrier_frames: u64,
    pub direct_frames: u64,
    pub compared_frames: u64,
    pub max_abs_diff: f32,
    pub first_mismatch_frame: Option<u64>,
    pub rms_diff: f64,
    pub tolerance: f32,
    pub passed: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LifecycleReport {
    /// Carrier chunks consumed from the input.
    pub input_chunks: u64,
    /// Distinct HAL session generations observed.
    pub sessions: u64,
    /// Resets caused by a new HAL session generation.
    pub session_resets: u64,
    /// Resets requested by a carrier discontinuity/drop flag.
    pub carrier_discontinuities: u64,
    /// Rendered chunks flagged as a new timeline (initial start and resets).
    pub output_discontinuities: u64,
    /// Live-output stalls longer than the deadline allowance.
    pub deadline_failures: u64,
    /// Longest observed output stall.
    pub max_output_stall_ms: f64,
    /// Late buffers rebased instead of aborting playback.
    pub scheduling_reanchors: u64,
    /// Worst scheduling lateness observed.
    pub max_scheduling_lateness_ms: f64,
    /// Physical output stalls recovered by rebuilding the sink.
    pub output_recoveries: u64,
    /// Carrier frames the HAL dropped because the ring was full.
    pub carrier_dropped_frames: u64,
    /// Ring slots the reader skipped after a producer reset/overflow.
    pub skipped_slots: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderReport {
    pub codec: String,
    pub render: String,
    pub sample_rate: u32,
    pub output: String,
    pub frames: u64,
    /// Decoded source frames before any renderer tail.
    pub source_frames: u64,
    pub duration_seconds: f64,
    pub wall_seconds: f64,
    pub ledger: DelayLedger,
    pub transport: TransportReport,
    pub lifecycle: LifecycleReport,
}

#[derive(Debug)]
pub struct RenderOutcome {
    pub report: RenderReport,
    pub pcm: Option<RenderedPcm>,
}

#[derive(Debug, Default)]
struct Timeline {
    base_carrier_sample: Option<u64>,
    truehd_anchor: Option<u64>,
    clock: Option<CarrierClock>,
}

impl Timeline {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn begin_segment(&mut self, start_sample: Option<u64>, host_time: Option<u64>) {
        if self.base_carrier_sample.is_none() {
            self.clock = start_sample
                .zip(host_time)
                .map(|(sample, host)| CarrierClock {
                    position: gst::ClockTime::from_nseconds(
                        (u128::from(sample) * 1_000_000_000 / u128::from(CARRIER_SAMPLE_RATE))
                            as u64,
                    ),
                    host_ns: host_ticks_to_ns(host),
                });
        }
        self.base_carrier_sample
            .get_or_insert(start_sample.unwrap_or(0));
    }

    fn pts(&mut self, unit: &AccessUnit, rate: u32) -> gst::ClockTime {
        let base = self.base_carrier_sample.unwrap_or(0);
        let samples = match unit {
            AccessUnit::Eac3(unit) => {
                let burst = unit.header.carrier_offset / EAC3_BURST_BYTES as u64;
                let carrier =
                    base + burst * (EAC3_BURST_BYTES as u64 / EAC3_CARRIER_BYTES_PER_FRAME as u64);
                carrier * rate as u64 / u64::from(CARRIER_SAMPLE_RATE)
            }
            AccessUnit::TrueHd(unit) => {
                let anchor = *self.truehd_anchor.get_or_insert_with(|| {
                    let carrier =
                        base + unit.header.carrier_offset / TRUEHD_CARRIER_BYTES_PER_FRAME as u64;
                    carrier * rate as u64 / u64::from(CARRIER_SAMPLE_RATE)
                });
                anchor + unit.sample_offset
            }
        };
        gst::ClockTime::from_nseconds(
            samples.saturating_mul(1_000_000_000) / u64::from(rate.max(1)),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetKind {
    None,
    /// The HAL started a new output session.
    Session,
    /// The carrier itself signalled a discontinuity or drop.
    Discontinuity,
}

impl ResetKind {
    const fn is_reset(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Debug, Default)]
struct SessionTracker {
    generation: Option<u64>,
    sessions: u64,
    session_resets: u64,
    carrier_discontinuities: u64,
    input_chunks: u64,
}

impl SessionTracker {
    fn observe(&mut self, chunk: &CarrierChunk) -> ResetKind {
        self.input_chunks += 1;
        match self.generation {
            None => {
                self.generation = Some(chunk.generation);
                self.sessions = 1;
            }
            Some(generation) if generation != chunk.generation => {
                self.generation = Some(chunk.generation);
                self.sessions += 1;
                self.session_resets += 1;
                return ResetKind::Session;
            }
            _ => {}
        }
        if chunk.is_discontinuity() {
            self.carrier_discontinuities += 1;
            return ResetKind::Discontinuity;
        }
        ResetKind::None
    }
}

#[derive(Debug, Default)]
struct Progress {
    input: AtomicU64,
    output: AtomicU64,
    deadline_failures: AtomicU64,
    max_stall_ns: AtomicU64,
    fatal: Mutex<Option<anyhow::Error>>,
}

impl Progress {
    fn fail(&self, error: anyhow::Error, stop: &AtomicBool) {
        if let Ok(mut fatal) = self.fatal.lock()
            && fatal.is_none()
        {
            *fatal = Some(error);
        }
        stop.store(true, Ordering::Relaxed);
    }

    fn complete<T>(&self, result: Result<T>, stop: &AtomicBool) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.fail(error, stop);
                None
            }
        }
    }

    fn take_fatal(&self) -> Option<anyhow::Error> {
        self.fatal.lock().ok().and_then(|mut fatal| fatal.take())
    }
}

/// Count only active input time. A pause gives resumed output a fresh deadline.
struct StallWatch {
    input: u64,
    output: u64,
    input_changed: Instant,
    output_changed: Instant,
    counted: bool,
}

impl StallWatch {
    fn new(now: Instant) -> Self {
        Self {
            input: 0,
            output: 0,
            input_changed: now,
            output_changed: now,
            counted: false,
        }
    }

    fn observe(
        &mut self,
        now: Instant,
        input: u64,
        output: u64,
        deadline: Duration,
    ) -> Option<Duration> {
        if input != self.input {
            if self.input == 0 || now.duration_since(self.input_changed) > deadline {
                self.output_changed = now;
                self.counted = false;
            }
            self.input = input;
            self.input_changed = now;
        }
        if output != self.output {
            self.output = output;
            self.output_changed = now;
            self.counted = false;
        }
        if input == 0 || now.duration_since(self.input_changed) > deadline {
            return None;
        }
        Some(now.duration_since(self.output_changed))
    }
}

fn spawn_stall_monitor(
    progress: Arc<Progress>,
    stop: Arc<AtomicBool>,
    monitor_stop: Arc<AtomicBool>,
    deadline: Duration,
    hard: Duration,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let interval = Duration::from_millis(100);
        let resume_after_idle = deadline.min(interval);
        let mut watch = StallWatch::new(Instant::now());
        let mut warned = false;
        while !stop.load(Ordering::Relaxed) && !monitor_stop.load(Ordering::Relaxed) {
            thread::sleep(interval);
            let input = progress.input.load(Ordering::Relaxed);
            let output = progress.output.load(Ordering::Relaxed);
            let Some(stall) = watch.observe(Instant::now(), input, output, resume_after_idle)
            else {
                continue;
            };
            if stall > deadline {
                if !watch.counted {
                    progress.deadline_failures.fetch_add(1, Ordering::Relaxed);
                    watch.counted = true;
                }
                let stall_ns = stall.as_nanos().min(u128::from(u64::MAX)) as u64;
                progress.max_stall_ns.fetch_max(stall_ns, Ordering::Relaxed);
            }
            if stall > hard && !warned {
                // The output writer owns recovery: it bounds each appsrc push
                // and rebuilds the sink on a stall. Do not kill the whole
                // service (and its carrier ring position) here.
                eprintln!(
                    "salsa-audiod: physical output stalled for {:.1} s; waiting for recovery",
                    stall.as_secs_f64()
                );
                warned = true;
            }
            if stall <= deadline {
                warned = false;
            }
        }
    })
}

struct PullSession {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Option<u64>>>,
}

impl PullSession {
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn join(&mut self) -> Result<Option<u64>> {
        self.handle
            .take()
            .context("output pull session was already joined")?
            .join()
            .map_err(|_| anyhow::anyhow!("output pull thread panicked"))
    }
}

fn start_puller(
    pipeline: &DecodeRenderPipeline,
    sender: mpsc::SyncSender<PcmChunk>,
    progress: Arc<Progress>,
    service: &ServiceConfig,
    session: PcmSession,
) -> PullSession {
    let stop = Arc::new(AtomicBool::new(false));
    let appsink = pipeline.appsink().clone();
    let gst_pipeline = pipeline.pipeline().clone();
    let session_stop = stop.clone();
    let service_stop = service.stop.clone();
    let idle_timeout = service.idle_timeout;
    let is_live = service.live();
    let handle = thread::spawn(move || {
        let result = output::pull_samples(
            &appsink,
            sender,
            || session_stop.load(Ordering::Relaxed) || service_stop.load(Ordering::Relaxed),
            is_live,
            idle_timeout,
            None,
            session,
        );
        let result = progress.complete(result, &service_stop);
        if result.is_none() {
            session_stop.store(true, Ordering::Relaxed);
            let _ = gst_pipeline.set_state(gst::State::Null);
        }
        result
    });
    PullSession {
        stop,
        handle: Some(handle),
    }
}

fn unit_codec(unit: &AccessUnit) -> Codec {
    match unit {
        AccessUnit::Eac3(_) => Codec::Eac3,
        AccessUnit::TrueHd(_) => Codec::TrueHd,
    }
}

fn output_label(config: &OutputConfig) -> String {
    match config {
        OutputConfig::Wav(path) => format!("wav:{}", path.display()),
        OutputConfig::Null => "null".to_string(),
        OutputConfig::Memory => "memory".to_string(),
        OutputConfig::CoreAudio { device } => match device {
            Some(device) => format!("coreaudio:{device}"),
            None => "coreaudio".to_string(),
        },
    }
}

/// Render a carrier source through the Salsa processing path.
pub fn render(config: ServiceConfig) -> Result<RenderOutcome> {
    pipeline::init()?;
    let started = Instant::now();

    let mut input = match &config.source {
        CarrierSource::File(path) => CarrierInput::open_file(path)?,
        CarrierSource::Shared(name) => CarrierInput::open_shared(name, config.stop.clone())?,
        CarrierSource::Scripted(chunks) => CarrierInput::open_scripted(chunks.clone()),
    };
    let is_live = input.is_live();

    let mut depacketizer = Depacketizer::new(config.sample_rate);
    let mut timeline = Timeline::default();
    let mut sessions = SessionTracker::default();
    let mut codec = config.codec;
    let mut units = Vec::new();

    // Read until the first codec access unit; that selects the pipeline.
    while units.is_empty() {
        let Some(chunk) = input
            .next_chunk()
            .context("while waiting for the first carrier block")?
        else {
            bail!("carrier ended before a supported E-AC-3 or TrueHD access unit was recovered");
        };
        let (new_units, _) = feed_chunk(&mut depacketizer, &mut timeline, &mut sessions, chunk)?;
        if let Some(first) = new_units.first() {
            let detected = unit_codec(first);
            if let Some(expected) = codec
                && expected != detected
            {
                bail!(
                    "carrier carries {} but {} was requested",
                    detected.label(),
                    expected.label()
                );
            }
            codec = Some(detected);
        }
        units = new_units;
    }
    let codec = codec.expect("codec is selected by the first access unit");

    let mut ledger = DelayLedger::new(
        codec,
        &config.render,
        config.sample_rate,
        config.scheduling_allowance_ns,
        config.physical_output_latency_ns,
    );
    if config.declare_latency
        && let CarrierInput::Shared(shared) = &input
    {
        shared.declare_latency(ledger.declared_carrier_frames());
    }

    let pipeline_config = PipelineConfig {
        codec,
        render: config.render.clone(),
        live: is_live,
    };

    // Open the output before launching workers so creation errors return
    // directly. Later worker errors share the same cancellation path.
    let mut sink = OutputSink::with_timing(
        &config.output,
        config.stop.clone(),
        ledger.scheduling_allowance_ns,
        config.deadline_timeout,
    )?;
    let (sender, receiver) = mpsc::sync_channel::<PcmChunk>(8);
    let progress = Arc::new(Progress::default());
    progress
        .input
        .store(sessions.input_chunks, Ordering::Relaxed);
    let session_epoch = Arc::new(AtomicU64::new(0));
    let mut pipeline = DecodeRenderPipeline::with_stop(&pipeline_config, config.stop.clone())?;
    pipeline.play()?;
    let mut puller = start_puller(
        &pipeline,
        sender.clone(),
        progress.clone(),
        &config,
        PcmSession {
            epoch: 0,
            clock: timeline.clock,
        },
    );

    let writer_progress = progress.clone();
    let writer_stop = config.stop.clone();
    let writer_epoch = session_epoch.clone();
    let writer = thread::spawn(move || {
        let result = (|| -> Result<output::OutputStats> {
            let mut output_epoch = writer_epoch.load(Ordering::Relaxed);
            while !writer_stop.load(Ordering::Relaxed) {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => {
                        let target = writer_epoch.load(Ordering::Relaxed);
                        if target != output_epoch {
                            output_epoch = target;
                            sink.flush()?;
                        }
                        if chunk.epoch != target {
                            // Pre-reset PCM still queued behind the reset.
                            continue;
                        }
                        sink.write(chunk)?;
                        writer_progress.output.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            sink.finish()
        })();
        // Record the cause before dropping the receiver. A disconnected
        // channel must not replace the original I/O or device error.
        writer_progress.complete(result, &writer_stop)
    });

    let monitor_stop = Arc::new(AtomicBool::new(false));
    let monitor = is_live.then(|| {
        spawn_stall_monitor(
            progress.clone(),
            config.stop.clone(),
            monitor_stop.clone(),
            config.deadline_timeout,
            config.hard_stall_timeout,
        )
    });

    let mut feeder_error = None;
    let mut pushed_any = false;
    let mut source = SourceCounter::default();

    let result = (|| -> Result<()> {
        for unit in units.drain(..) {
            let discont = !pushed_any;
            let start = push_unit(
                &mut pipeline,
                &mut timeline,
                config.sample_rate,
                &unit,
                discont,
            )?;
            source.observe(codec, start);
            pushed_any = true;
        }
        loop {
            if config.stop.load(Ordering::Relaxed) {
                break;
            }
            let Some(chunk) = input.next_chunk()? else {
                break;
            };
            progress.input.fetch_add(1, Ordering::Relaxed);
            let (new_units, reset) =
                feed_chunk(&mut depacketizer, &mut timeline, &mut sessions, chunk)?;
            if reset.is_reset() {
                // Retire the old consumer before flushing. Any sample it has
                // already pulled keeps its immutable session identity, and it
                // cannot consume PCM from the next session.
                puller.stop();
                puller.join()?;
                let rendered_before = progress.output.load(Ordering::Relaxed) > 0;
                let seeked = rendered_before
                    && match pipeline.seek_to_start() {
                        Ok(()) => true,
                        Err(error) => {
                            eprintln!(
                                "salsa-audiod: decode/render seek failed ({error:#}); rebuilding"
                            );
                            false
                        }
                    };
                if !seeked {
                    // A seek during pipeline startup has no negotiated output
                    // to flush and a native seek can race caps negotiation.
                    // Rebuild only in this rare case; normal seeks stay native.
                    pipeline.stop();
                    pipeline =
                        DecodeRenderPipeline::with_stop(&pipeline_config, config.stop.clone())?;
                    pipeline.play()?;
                }
                let epoch = session_epoch
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                puller = start_puller(
                    &pipeline,
                    sender.clone(),
                    progress.clone(),
                    &config,
                    PcmSession {
                        epoch,
                        clock: timeline.clock,
                    },
                );
                pushed_any = false;
            }
            let mut discont = !pushed_any || reset.is_reset();
            for unit in new_units {
                let detected = unit_codec(&unit);
                if detected != codec {
                    bail!(
                        "carrier switched from {} to {} mid-session",
                        codec.label(),
                        detected.label()
                    );
                }
                let start = push_unit(
                    &mut pipeline,
                    &mut timeline,
                    config.sample_rate,
                    &unit,
                    discont,
                )?;
                discont = false;
                source.observe(codec, start);
                pushed_any = true;
            }
        }
        pipeline.end_of_stream()?;
        Ok(())
    })();

    if let Err(error) = result {
        feeder_error = Some(error);
        config.stop.store(true, Ordering::Relaxed);
    }

    // Wait for EOS or a pipeline error before stopping the pipeline. The puller
    // drains the remaining rendered buffers on EOS.
    let mut clean_eos = false;
    if feeder_error.is_none() && !config.stop.load(Ordering::Relaxed) {
        let bus = pipeline
            .pipeline()
            .bus()
            .context("decode/render pipeline has no bus")?;
        loop {
            match bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(100),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) {
                Some(message) => {
                    if let gst::MessageView::Error(error) = message.view() {
                        feeder_error = Some(anyhow::anyhow!(
                            "decode/render pipeline error: {} ({})",
                            error.error(),
                            error.debug().unwrap_or_default()
                        ));
                    }
                    clean_eos = matches!(message.view(), gst::MessageView::Eos(_));
                    break;
                }
                None => {
                    if config.stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
        }
    }

    if let Some((live, min, max)) = pipeline.queried_latency() {
        let _ = live;
        ledger.set_query(min, max);
    }
    monitor_stop.store(true, Ordering::Relaxed);
    if !clean_eos {
        config.stop.store(true, Ordering::Relaxed);
        puller.stop();
        pipeline.stop();
    }
    drop(sender);

    let pulled = puller.join();
    pipeline.stop();
    let stats = writer
        .join()
        .map_err(|_| anyhow::anyhow!("output writer thread panicked"))?;
    if let Some(monitor) = monitor {
        let _ = monitor.join();
    }
    if let Some(error) = progress.take_fatal() {
        return Err(error);
    }
    if let Some(error) = feeder_error {
        return Err(error);
    }
    let pulled_frames = pulled?.context("output puller failed without an error")?;
    let stats = stats.context("output writer failed without an error")?;

    let carrier_stats = depacketizer.carrier_stats();
    let eac3_stats = depacketizer.eac3_stats();
    let truehd_stats = depacketizer.truehd_stats();
    let (skipped_slots, carrier_dropped_frames) = match &input {
        CarrierInput::Shared(shared) => (shared.skipped_slots(), shared.dropped_frames()),
        CarrierInput::File(_) | CarrierInput::Scripted(_) => (0, 0),
    };
    let wall_seconds = started.elapsed().as_secs_f64();
    let frames = stats.frames.max(pulled_frames);
    let duration_seconds = stats
        .info
        .map(|info| frames as f64 / f64::from(info.rate))
        .unwrap_or(0.0);

    Ok(RenderOutcome {
        report: RenderReport {
            codec: codec.label().to_string(),
            render: config.render.label().to_string(),
            sample_rate: config.sample_rate,
            output: output_label(&config.output),
            frames,
            source_frames: source.finish(),
            duration_seconds,
            wall_seconds,
            ledger,
            transport: TransportReport::new(carrier_stats, eac3_stats, truehd_stats, skipped_slots),
            lifecycle: LifecycleReport {
                input_chunks: sessions.input_chunks,
                sessions: sessions.sessions,
                session_resets: sessions.session_resets,
                carrier_discontinuities: sessions.carrier_discontinuities,
                output_discontinuities: stats.discontinuities,
                deadline_failures: progress.deadline_failures.load(Ordering::Relaxed),
                max_output_stall_ms: progress.max_stall_ns.load(Ordering::Relaxed) as f64 / 1e6,
                scheduling_reanchors: stats.late_buffers,
                max_scheduling_lateness_ms: stats.max_lateness_ns as f64 / 1e6,
                output_recoveries: stats.recoveries,
                carrier_dropped_frames,
                skipped_slots,
            },
        },
        pcm: stats.memory,
    })
}

fn feed_chunk(
    depacketizer: &mut Depacketizer,
    timeline: &mut Timeline,
    sessions: &mut SessionTracker,
    chunk: CarrierChunk,
) -> Result<(Vec<AccessUnit>, ResetKind)> {
    let reset = sessions.observe(&chunk);
    if reset.is_reset() {
        depacketizer.reset();
        timeline.reset();
    }
    timeline.begin_segment(chunk.start_sample, chunk.host_time);
    Ok((depacketizer.push(&chunk.bytes), reset))
}

fn push_unit(
    pipeline: &mut DecodeRenderPipeline,
    timeline: &mut Timeline,
    rate: u32,
    unit: &AccessUnit,
    discont: bool,
) -> Result<u64> {
    let pts = timeline.pts(unit, rate);
    let start = pts.nseconds().saturating_mul(u64::from(rate)) / 1_000_000_000;
    pipeline.push_access_unit(unit.bytes(), pts, discont)?;
    Ok(start)
}

/// Track decoded source duration without relying on the renderer output length
/// (binaural adds a room tail, and a truncated carrier is shorter than the raw
/// fixture).
#[derive(Debug, Default)]
struct SourceCounter {
    prev_truehd_start: Option<u64>,
    last_truehd_delta: u64,
    end: u64,
}

impl SourceCounter {
    fn observe(&mut self, codec: Codec, start: u64) {
        match codec {
            Codec::Eac3 => {
                // One E-AC-3 audio frame per carrier burst: 6 x 256 samples.
                // `start` is an absolute device position in live mode, so count
                // frames instead of using it as a duration.
                self.end = self
                    .end
                    .saturating_add(u64::from(salsa_codec::EAC3_FRAME_SAMPLES));
            }
            Codec::TrueHd => {
                if let Some(previous) = self.prev_truehd_start {
                    let delta = start.saturating_sub(previous);
                    if delta > 0 {
                        self.last_truehd_delta = delta;
                        self.end = self.end.max(previous.saturating_add(delta));
                    }
                }
                self.prev_truehd_start = Some(start);
                if self.last_truehd_delta > 0 {
                    self.end = self.end.max(start.saturating_add(self.last_truehd_delta));
                }
            }
        }
    }

    fn finish(&self) -> u64 {
        self.end
    }
}

/// Render the carrier and a direct raw-codec file through the same pipeline and
/// compare the rendered PCM against the same processing configuration.
pub fn compare(
    mut config: ServiceConfig,
    direct_path: impl AsRef<std::path::Path>,
    tolerance: f32,
) -> Result<(RenderReport, ComparisonReport)> {
    let render_mode = config.render.clone();
    let idle_timeout = config.idle_timeout;
    config.output = OutputConfig::Memory;
    let outcome = render(config)?;
    let carrier = outcome.pcm.context("carrier render did not return PCM")?;
    let codec = match outcome.report.codec.as_str() {
        "eac3" => Codec::Eac3,
        _ => Codec::TrueHd,
    };
    let direct = render_direct(direct_path, codec, &render_mode, idle_timeout)?;
    let guard_frames = if render_mode.is_binaural() {
        u64::from(outcome.report.ledger.renderer_signal_samples) + 4096
    } else {
        0
    };
    let mut comparison = compare_pcm(
        &carrier,
        &direct,
        outcome.report.source_frames,
        guard_frames,
        tolerance,
    )?;
    comparison.codec = outcome.report.codec.clone();
    comparison.render = outcome.report.render.clone();
    Ok((outcome.report, comparison))
}

fn compare_pcm(
    carrier: &RenderedPcm,
    direct: &RenderedPcm,
    source_frames: u64,
    guard_frames: u64,
    tolerance: f32,
) -> Result<ComparisonReport> {
    if carrier.channels != direct.channels {
        bail!(
            "renderer produced {} channels from the carrier and {} from the direct stream",
            carrier.channels,
            direct.channels
        );
    }
    let channels = carrier.channels.max(1) as usize;
    let limit = if source_frames == 0 {
        carrier.frames().min(direct.frames())
    } else {
        source_frames
            .saturating_sub(guard_frames)
            .min(carrier.frames())
            .min(direct.frames())
    };
    let compared_frames = limit as usize;
    let compared_samples = compared_frames * channels;
    let mut max_abs_diff = 0.0f32;
    let mut first_mismatch_frame = None;
    let mut sum_sq = 0.0f64;
    for index in 0..compared_samples {
        let diff = carrier.samples[index] - direct.samples[index];
        let magnitude = diff.abs();
        if magnitude > tolerance && first_mismatch_frame.is_none() {
            first_mismatch_frame = Some((index / channels) as u64);
        }
        max_abs_diff = max_abs_diff.max(magnitude);
        sum_sq += f64::from(diff) * f64::from(diff);
    }
    let rms_diff = if compared_samples == 0 {
        0.0
    } else {
        (sum_sq / compared_samples as f64).sqrt()
    };
    Ok(ComparisonReport {
        codec: String::new(),
        render: String::new(),
        carrier_frames: carrier.frames(),
        direct_frames: direct.frames(),
        compared_frames: compared_frames as u64,
        max_abs_diff,
        first_mismatch_frame,
        rms_diff,
        tolerance,
        passed: max_abs_diff <= tolerance,
    })
}

/// Direct GStreamer rendering from a raw codec file. It uses the same parser,
/// decoder and renderer as the carrier path but skips IEC 61937/MAT recovery.
pub fn render_direct(
    path: impl AsRef<std::path::Path>,
    codec: Codec,
    render: &RenderMode,
    idle_timeout: Duration,
) -> Result<RenderedPcm> {
    pipeline::init()?;
    let source = DirectPipeline::new(path, codec, render)?;
    source.play()?;

    let (sender, receiver) = mpsc::sync_channel::<PcmChunk>(8);
    let writer = thread::spawn(move || -> Result<output::OutputStats> {
        let mut sink = OutputSink::new(&OutputConfig::Memory)?;
        while let Ok(chunk) = receiver.recv() {
            sink.write(chunk)?;
        }
        sink.finish()
    });
    let stop = AtomicBool::new(false);
    let pulled = output::pull_samples(
        source.appsink(),
        sender,
        || stop.load(Ordering::Relaxed),
        false,
        idle_timeout,
        None,
        PcmSession::default(),
    );
    source.stop();
    let stats = writer
        .join()
        .map_err(|_| anyhow::anyhow!("direct output writer thread panicked"))??;
    pulled?;
    Ok(stats.memory.expect("memory output requested"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carrier::{FLAG_DISCONTINUITY, FLAG_DROPPED};

    fn chunk(generation: u64, start_sample: u64, flags: u32) -> CarrierChunk {
        CarrierChunk {
            bytes: vec![0; 16],
            start_sample: Some(start_sample),
            host_time: Some(0),
            bytes_per_frame: Some(4),
            generation,
            flags,
        }
    }

    #[test]
    fn session_tracker_resets_on_generation_and_flags() {
        let mut tracker = SessionTracker::default();

        assert_eq!(tracker.observe(&chunk(0, 0, 0)), ResetKind::None);
        assert_eq!(tracker.sessions, 1);
        assert_eq!(tracker.session_resets, 0);

        assert_eq!(tracker.observe(&chunk(0, 1, 0)), ResetKind::None);

        assert_eq!(
            tracker.observe(&chunk(0, 2, FLAG_DISCONTINUITY)),
            ResetKind::Discontinuity
        );
        assert_eq!(tracker.carrier_discontinuities, 1);
        assert_eq!(tracker.session_resets, 0);

        assert_eq!(
            tracker.observe(&chunk(0, 3, FLAG_DROPPED)),
            ResetKind::Discontinuity
        );
        assert_eq!(tracker.carrier_discontinuities, 2);

        assert_eq!(
            tracker.observe(&chunk(1, 4, 0)),
            ResetKind::Session,
            "a HAL session generation change is a hard reset"
        );
        assert_eq!(tracker.sessions, 2);
        assert_eq!(tracker.session_resets, 1);

        assert_eq!(tracker.observe(&chunk(1, 5, 0)), ResetKind::None);
        assert_eq!(tracker.input_chunks, 6);
    }

    #[test]
    fn timeline_anchor_restarts_after_reset() {
        let mut timeline = Timeline::default();
        timeline.begin_segment(Some(100), Some(0));
        assert_eq!(timeline.base_carrier_sample, Some(100));
        timeline.reset();
        timeline.begin_segment(Some(900), Some(0));
        assert_eq!(timeline.base_carrier_sample, Some(900));
        assert!(timeline.truehd_anchor.is_none());
    }

    #[test]
    fn pause_does_not_consume_the_resumed_output_deadline() {
        let start = Instant::now();
        let deadline = Duration::from_millis(100);
        let mut watch = StallWatch::new(start);
        assert_eq!(watch.observe(start, 1, 1, deadline), Some(Duration::ZERO));
        assert_eq!(
            watch.observe(start + Duration::from_secs(20), 1, 1, deadline),
            None
        );
        assert_eq!(
            watch.observe(start + Duration::from_secs(21), 2, 1, deadline),
            Some(Duration::ZERO)
        );
        for tick in 1..=10 {
            let stall = watch
                .observe(
                    start + Duration::from_secs(21) + Duration::from_millis(tick * 50),
                    2 + tick,
                    1,
                    deadline,
                )
                .unwrap();
            assert_eq!(stall, Duration::from_millis(tick * 50));
        }
        assert_eq!(
            watch.observe(start + Duration::from_millis(21_550), 13, 2, deadline),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn short_pause_resets_output_wait_after_input_silence() {
        let start = Instant::now();
        let deadline = Duration::from_millis(100);
        let resume_after_idle = deadline.min(Duration::from_millis(100));
        let mut watch = StallWatch::new(start);
        assert_eq!(
            watch.observe(start, 1, 1, resume_after_idle),
            Some(Duration::ZERO)
        );
        assert_eq!(
            watch.observe(start + Duration::from_millis(90), 1, 1, resume_after_idle),
            Some(Duration::from_millis(90))
        );
        assert_eq!(
            watch.observe(start + Duration::from_millis(140), 2, 1, resume_after_idle),
            Some(Duration::ZERO),
            "the input pause must give output a fresh deadline"
        );
    }

    #[test]
    fn worker_failure_preserves_the_first_error_and_cancels_waits() {
        let progress = Progress::default();
        let stop = AtomicBool::new(false);
        let failure =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "output write failed");
        progress.fail(failure.into(), &stop);
        progress.fail(anyhow::anyhow!("output writer stopped"), &stop);
        assert!(stop.load(Ordering::Relaxed));
        let error = progress.take_fatal().unwrap();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
