//! `salsatruehdatmosdec`: GstAudioDecoder that emits raw object audio.

use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use gst::glib;
use gst::prelude::*;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_audio as gst_audio;

use objectaudio_decode::{
    TRUEHD_RATES, negotiate_attach_meta, object_audio_src_caps, object_audio_template_caps,
    push_pcm,
};
use objectaudio_ir::{SpatialDecoder, SpatialFrame};
use truehd_atmos::{
    MAX_CHANNELS, SpatialSnapshot, TrueHdAccessUnitParser, TrueHdDecoder, TrueHdFrame,
};

/// Output buffers are kept at a multiple of the speaker renderer's block size.
const BLOCK_SAMPLES: usize = objectaudio_ir::METADATA_BLOCK_SAMPLES;

/// Flush a buffer every this many samples even when no OAMD update arrives.
///
/// 1216 = 38 * 32. The observed OAMD update interval on the reference master is
/// 1520..1560 samples, so a chunk never holds more than one update.
const CHUNK_SAMPLES: usize = 38 * BLOCK_SAMPLES;

/// Stack for the parse/decode worker. The TrueHD parser and MLP decoder use
/// large stack frames, more than the GStreamer streaming thread provides in
/// debug builds.
const WORKER_STACK: usize = 16 * 1024 * 1024;

struct Decoded {
    spatial: SpatialFrame,
    snapshot: Option<SpatialSnapshot>,
}

enum Command {
    Push(Vec<u8>),
    Reset,
    Exit,
}

struct Worker {
    commands: Sender<Command>,
    results: Receiver<Result<Vec<Decoded>, String>>,
    join: Option<JoinHandle<()>>,
}

fn decode_frames(
    decoder: &mut TrueHdDecoder,
    frames: &[TrueHdFrame],
) -> Result<Vec<Decoded>, String> {
    let mut decoded = Vec::with_capacity(frames.len());
    for frame in frames {
        let spatial = decoder.decode(frame).map_err(|error| error.to_string())?;
        let snapshot = decoder.take_snapshot();
        decoded.push(Decoded { spatial, snapshot });
    }
    Ok(decoded)
}

impl Worker {
    fn spawn() -> std::io::Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("salsa-truehd".into())
            .stack_size(WORKER_STACK)
            .spawn(move || {
                let mut parser = TrueHdAccessUnitParser::new();
                let mut decoder = TrueHdDecoder::new();
                let mut index = 0u64;
                let mut offset = 0u64;
                while let Ok(command) = command_rx.recv() {
                    match command {
                        Command::Push(data) => {
                            let result = parser
                                .parse(&data, index, offset)
                                .map_err(|error| error.to_string())
                                .and_then(|frame| decode_frames(&mut decoder, &[frame]));
                            index = index.saturating_add(1);
                            offset = offset.saturating_add(data.len() as u64);
                            if result_tx.send(result).is_err() {
                                break;
                            }
                        }
                        Command::Reset => {
                            parser = TrueHdAccessUnitParser::new();
                            decoder.reset();
                            index = 0;
                            offset = 0;
                            let _ = result_tx.send(Ok(Vec::new()));
                        }
                        Command::Exit => break,
                    }
                }
            })?;
        Ok(Self {
            commands: command_tx,
            results: result_rx,
            join: Some(join),
        })
    }

    fn push(&self, data: Vec<u8>) -> Result<Vec<Decoded>, String> {
        self.commands
            .send(Command::Push(data))
            .map_err(|_| "the TrueHD worker has stopped".to_string())?;
        match self.results.recv() {
            Ok(Ok(decoded)) => Ok(decoded),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("the TrueHD worker has stopped".to_string()),
        }
    }

    fn reset(&self) -> Result<(), String> {
        self.commands
            .send(Command::Reset)
            .map_err(|_| "the TrueHD worker has stopped".to_string())?;
        match self.results.recv() {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("the TrueHD worker has stopped".to_string()),
        }
    }

    fn shutdown(mut self) {
        let _ = self.commands.send(Command::Exit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct State {
    worker: Option<Worker>,
    rate: i32,
    channels: i32,
    caps_set: bool,
    attach_meta: bool,
    pending: Vec<f32>,
    scene: Option<objectaudio_ir::SceneTimeline>,
    received: i64,
    emitted: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            worker: None,
            rate: 48_000,
            channels: 0,
            caps_set: false,
            attach_meta: false,
            pending: Vec::new(),
            scene: None,
            received: 0,
            emitted: 0,
        }
    }
}

impl State {
    fn pending_samples(&self) -> usize {
        if self.channels > 0 {
            self.pending.len() / self.channels as usize
        } else {
            0
        }
    }

    fn clear_stream(&mut self) {
        self.pending.clear();
        self.scene = None;
        self.received = 0;
        self.emitted = 0;
        self.caps_set = false;
        self.channels = 0;
        self.attach_meta = false;
    }
}

#[derive(Default)]
pub struct TrueHdAtmosDec {
    // Boxed: the pending PCM and worker handle stay off the GObject allocation.
    state: Mutex<Box<State>>,
}

impl TrueHdAtmosDec {
    fn ensure_output_caps(&self, rate: i32, channels: i32) -> Result<(), gst::FlowError> {
        {
            let state = self.state.lock().unwrap();
            if state.caps_set {
                if state.rate == rate && state.channels == channels {
                    return Ok(());
                }
                gst::error!(
                    gst::CAT_RUST,
                    "salsatruehdatmosdec: output format changed from {} Hz / {} ch to {} Hz / {} ch",
                    state.rate,
                    state.channels,
                    rate,
                    channels
                );
                return Err(gst::FlowError::NotNegotiated);
            }
        }

        let borrowed = self.obj();
        let object: &super::TrueHdAtmosDec = borrowed.as_ref();
        let attach_meta = negotiate_attach_meta(object);
        let caps = object_audio_src_caps(rate, channels as usize, attach_meta);
        object
            .set_output_caps(&caps)
            .map_err(|_| gst::FlowError::NotNegotiated)?;

        let mut state = self.state.lock().unwrap();
        state.rate = rate;
        state.channels = channels;
        state.caps_set = true;
        state.attach_meta = attach_meta;
        Ok(())
    }

    fn accumulate(
        &self,
        spatial: SpatialFrame,
        snapshot: Option<SpatialSnapshot>,
    ) -> Result<(), gst::FlowError> {
        let channel_count = spatial.pcm.channel_count();
        if channel_count == 0 {
            return Ok(());
        }
        let rate = i32::try_from(spatial.sample_rate).unwrap_or(48_000);
        self.ensure_output_caps(rate, channel_count as i32)?;

        let interleaved = spatial.pcm.interleave();
        {
            let mut state = self.state.lock().unwrap();
            if state.channels as usize != channel_count {
                return Err(gst::FlowError::NotNegotiated);
            }
            let start = state.received;
            if let Some(payload) = snapshot {
                let result = match &mut state.scene {
                    Some(scene) => scene.submit(&payload, start),
                    None => objectaudio_ir::SceneTimeline::from_payload(&payload, start)
                        .map(|scene| state.scene = Some(scene)),
                };
                result.map_err(|error| {
                    gst::error!(gst::CAT_RUST, "invalid TrueHD metadata: {error}");
                    gst::FlowError::Error
                })?;
            }
            state.received += spatial.frames as i64;
            state.pending.extend_from_slice(&interleaved);
        }
        self.flush_ready()
    }

    /// Push decoded access units, treating a flush race as a normal stop.
    ///
    /// `finish_frame` returns `Flushing` when a seek starts while a buffer is
    /// in flight. Propagating it makes BaseParse treat the downstream stop as
    /// a fatal stream error, so drop the remaining units instead; `FLUSH_STOP`
    /// will reset the worker.
    fn push_decoded(&self, decoded: Vec<Decoded>) -> Result<(), gst::FlowError> {
        for decoded in decoded {
            match self.accumulate(decoded.spatial, decoded.snapshot) {
                Ok(()) => {}
                Err(gst::FlowError::Flushing) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn flush_ready(&self) -> Result<(), gst::FlowError> {
        let needed = {
            let state = self.state.lock().unwrap();
            if state.channels == 0 {
                return Ok(());
            }
            let samples = state.pending_samples();
            if samples >= CHUNK_SAMPLES {
                CHUNK_SAMPLES
            } else {
                0
            }
        };
        if needed == 0 {
            return Ok(());
        }
        self.flush_samples(needed)
    }

    fn flush_samples(&self, samples: usize) -> Result<(), gst::FlowError> {
        let (data, channels, attach_meta, meta) = {
            let mut state = self.state.lock().unwrap();
            let channels = state.channels as usize;
            if channels == 0 || samples == 0 {
                return Ok(());
            }
            let take = (samples * channels).min(state.pending.len());
            let data: Vec<f32> = state.pending.drain(..take).collect();
            let meta = state
                .scene
                .as_ref()
                .map(|scene| scene.payload(state.emitted, take / channels));
            state.emitted += (take / channels) as i64;
            let emitted = state.emitted;
            if let Some(scene) = &mut state.scene {
                scene.discard_before(emitted);
            }
            (data, channels, state.attach_meta, meta)
        };

        let meta = if attach_meta { meta.as_ref() } else { None };
        let borrowed = self.obj();
        let object: &super::TrueHdAtmosDec = borrowed.as_ref();
        push_pcm(object, &data, channels, meta, -1)?;
        Ok(())
    }

    fn drain(&self) -> Result<(), gst::FlowError> {
        let (take, channels) = {
            let mut state = self.state.lock().unwrap();
            let channels = state.channels as usize;
            let samples = state.pending_samples();
            let take = round_up_32(samples);
            if channels > 0 && take > samples {
                state.pending.resize(take * channels, 0.0);
            }
            (take, channels)
        };
        if channels > 0 && take > 0 {
            match self.flush_samples(take) {
                Ok(()) | Err(gst::FlowError::Flushing) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn take_worker(&self) -> Option<Worker> {
        self.state.lock().unwrap().worker.take()
    }
}

fn round_up_32(samples: usize) -> usize {
    samples.div_ceil(BLOCK_SAMPLES) * BLOCK_SAMPLES
}

#[glib::object_subclass]
impl ObjectSubclass for TrueHdAtmosDec {
    const NAME: &'static str = "GstSalsaTrueHdAtmosDec";
    type Type = super::TrueHdAtmosDec;
    type ParentType = gst_audio::AudioDecoder;
}

impl ObjectImpl for TrueHdAtmosDec {}

impl GstObjectImpl for TrueHdAtmosDec {}

impl ElementImpl for TrueHdAtmosDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
            std::sync::OnceLock::new();
        Some(METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "TrueHD Atmos Decoder",
                "Codec/Decoder/Audio",
                "Decodes a TrueHD Atmos object presentation into raw object audio",
                "salsa",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> = std::sync::OnceLock::new();
        TEMPLATES.get_or_init(|| {
            let sink_caps: gst::Caps =
                "audio/x-true-hd, framed=(boolean)true, alignment=(string)frame, \
                 rate=(int)[1,655350]; \
                 audio/x-truehd, framed=(boolean)true, alignment=(string)frame, \
                 rate=(int)[1,655350]"
                    .parse()
                    .unwrap();
            let src_caps = object_audio_template_caps(MAX_CHANNELS, TRUEHD_RATES);
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

impl AudioDecoderImpl for TrueHdAtmosDec {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::TrueHdAtmosDec = borrowed.as_ref();
        object.set_needs_format(true);
        object.set_estimate_rate(true);
        object.set_use_default_pad_acceptcaps(true);
        let sink_pad =
            <super::TrueHdAtmosDec as gst_audio::prelude::AudioDecoderExtManual>::sink_pad(object);
        sink_pad.set_object_flags(gst::ObjectFlags::from_bits_retain(
            gst::PadFlags::ACCEPT_TEMPLATE.bits(),
        ));
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        if let Some(worker) = self.take_worker() {
            worker.shutdown();
        }
        let mut state = self.state.lock().unwrap();
        state.clear_stream();
        Ok(())
    }

    fn set_format(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        if let Some(worker) = self.take_worker() {
            worker.shutdown();
        }
        let structure = caps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(gst::CAT_RUST, "empty sink caps"))?;
        let rate = structure.get::<i32>("rate").unwrap_or(48_000);
        let worker = Worker::spawn().map_err(|error| {
            gst::loggable_error!(
                gst::CAT_RUST,
                "cannot spawn the TrueHD worker thread: {error}"
            )
        })?;
        let mut state = self.state.lock().unwrap();
        state.clear_stream();
        state.rate = rate;
        state.worker = Some(worker);
        Ok(())
    }

    fn handle_frame(
        &self,
        buffer: Option<&gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(buffer) = buffer else {
            self.drain()?;
            return Ok(gst::FlowSuccess::Ok);
        };
        let data = {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            map.to_vec()
        };
        let decoded = {
            let state = self.state.lock().unwrap();
            let Some(worker) = &state.worker else {
                return Err(gst::FlowError::NotNegotiated);
            };
            worker.push(data).map_err(|error| {
                gst::error!(gst::CAT_RUST, "salsatruehdatmosdec: decode failed: {error}");
                gst::FlowError::Error
            })?
        };
        self.push_decoded(decoded)?;
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if event.type_() == gst::EventType::Eos
            && let Err(error) = self.drain()
        {
            gst::error!(
                gst::CAT_RUST,
                "salsatruehdatmosdec: EOS drain failed: {error:?}"
            );
            return false;
        }
        self.parent_sink_event(event)
    }

    fn flush(&self, hard: bool) {
        {
            let mut state = self.state.lock().unwrap();
            state.pending.clear();
            state.scene = None;
            state.received = 0;
            state.emitted = 0;
            if let Some(worker) = &state.worker
                && let Err(error) = worker.reset()
            {
                gst::warning!(gst::CAT_RUST, "salsatruehdatmosdec: reset failed: {error}");
            }
        }
        self.parent_flush(hard);
    }
}
