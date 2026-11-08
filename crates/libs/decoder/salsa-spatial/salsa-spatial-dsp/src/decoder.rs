//! Complete salsa-spatial object decoder.
//!
//! [`SalsaSpatialDecoder`] owns the Opus bed decoder, the shared 1920/240
//! filterbank, the parameter timeline, the regularized object estimator and
//! the streaming synthesizers. It turns each 100 ms [`SalsaSpatialFrame`] into
//! an [`objectaudio_ir::SpatialFrame`] whose channel 0 is the LFE and whose
//! channels `1..=N` are the stable source objects.
//!
//! # Timing
//!
//! Every position is an absolute 48 kHz sample. The Opus lookahead is removed
//! exactly once from the leading decoded samples (never per tile), so the
//! emitted PCM starts on the source clock. Analysis and synthesis are
//! streaming across tile boundaries: the STFT history and overlap-add state
//! are never reset between tiles.
//!
//! # Parameter timeline
//!
//! Per-band relative object powers are reconstructed directly from
//! [`salsa_spatial::params::ParameterAnchor::ell_half_db`] (integer half-dB).
//! They are never collapsed to a band-averaged vector. At each STFT hop the
//! two surrounding anchors are interpolated: the compact [`BandCovariance`]
//! is convex-interpolated (preserving positive semidefiniteness, including the
//! union of pair lists), `nu` is interpolated linearly in power, and the
//! unobservable masks are OR-ed.
//!
//! # Bed fallback
//!
//! A missing or invalid enhancement packet never changes committed predictor
//! state. The decoder keeps playing the decoded bed mapped onto the object
//! channels, and only returns to the object path after a valid checkpoint
//! restores the predictor chain. The transition is crossfaded over one 20 ms
//! packet. Seeking is supported in principle by [`SalsaSpatialDecoder::reset`]
//! followed by a checkpoint install and core/filterbank preroll; this decoder
//! does not itself implement a seek.

use num_complex::Complex;
use objectaudio_ir::{
    IrError, ObjectEvent, ObjectState, PcmBuffer, SceneTimeline, SpatialDecoder, SpatialFrame,
    SpatialPayload, Transition,
};
use salsa_spatial::params::{ParameterAnchor, ParameterTile, SceneParameterDecoder};
use salsa_spatial::{
    BaseFold, CORE_CHANNEL_COUNT, FFT_BINS, HALF_DB_SCALE, HOP_SAMPLES, LFE_CHANNEL_INDEX,
    MAX_OBJECTS, PARAM_BAND_COUNT, SAMPLE_RATE, SalsaSpatialFrame, SalsaSpatialStreamConfig,
    SceneSnapshot, TILE_SAMPLES, TrajectoryKnot,
};
use salsa_spatial_core_decoder::{CoreDecodeError, OpusBedDecoder};
use thiserror::Error;

use crate::covariance::{BandCovariance, covariance_from_band_powers};
use crate::panner::CorePanner;
use crate::reconstruct::{ReconstructionError, estimate_objects};
use crate::stft::{Spectrum, Stft, Synthesis};

/// Crossfade length between the bed fallback and the object render: one 20 ms
/// Opus packet.
const CROSSFADE_SAMPLES: u64 = 960;
/// Maximum number of 5 ms hops retained in the trajectory history. Two tiles
/// are enough to cover the analysis lag; a few more keep flush comfortable.
const MAX_TRAJECTORY_SNAPSHOTS: usize = 8;
/// Below this normalized power a band is treated as silent.
const TRACE_EPSILON: f64 = 1.0e-30;

/// Errors produced by [`SalsaSpatialDecoder`].
#[derive(Debug, Error)]
pub enum DecoderError {
    #[error("core decode failed: {0}")]
    Core(#[from] CoreDecodeError),
    #[error("parameter decode failed: {0}")]
    Parameters(#[from] salsa_spatial::SpatialError),
    #[error("object reconstruction failed: {0}")]
    Reconstruction(#[from] ReconstructionError),
    #[error("objectaudio-ir rejected the decoded frame: {0}")]
    Ir(#[from] IrError),
    #[error("invalid salsa-spatial decoder configuration: {0}")]
    InvalidConfig(String),
    #[error("unsupported sample rate {actual}; salsa-spatial is {expected} Hz")]
    SampleRate { expected: u32, actual: u32 },
    #[error("salsa-spatial decoder has already been finished")]
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Object,
    Bed,
}

#[derive(Debug, Clone, Copy)]
struct ModeTransition {
    from: OutputMode,
    to: OutputMode,
    start: u64,
    len: u64,
}

/// One decoded anchor prepared for the 5 ms timeline.
#[derive(Debug, Clone)]
struct TimelineAnchor {
    sample: u64,
    covariance: BandCovariance,
    /// `[band][channel]` decoded core-error powers.
    nu: [[f64; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT],
    /// `[band][channel]` unobservable mask.
    unobservable: [[bool; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT],
}

/// Trajectories covering one input tile. The history is needed because STFT
/// frames emitted while decoding tile `k` can still lie inside tile `k - 1`.
#[derive(Debug, Clone)]
struct TrajectorySnapshot {
    start: u64,
    frames: usize,
    trajectories: Vec<Vec<TrajectoryKnot>>,
}

/// Parameters interpolated at one STFT center.
struct SceneView {
    covariance: BandCovariance,
    nu: [[f64; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT],
    unobservable: [bool; PARAM_BAND_COUNT],
    active: bool,
}

/// The complete salsa-spatial object decoder.
pub struct SalsaSpatialDecoder {
    object_count: usize,
    headroom: f64,

    core: OpusBedDecoder,
    fold: BaseFold,
    panner: CorePanner,
    enhanced_supported: bool,
    parameters: SceneParameterDecoder,

    core_stfts: [Stft; CORE_CHANNEL_COUNT],
    lfe_stft: Stft,
    analyzed_hops: u64,
    lookahead_remaining: usize,
    source_origin: Option<u64>,
    declared_samples: u64,

    object_synthesis: Vec<Synthesis>,
    lfe_synthesis: Synthesis,

    anchors: Vec<TimelineAnchor>,
    current_trajectories: Vec<Vec<TrajectoryKnot>>,
    base_trajectories: Vec<Vec<TrajectoryKnot>>,
    trajectory_history: Vec<TrajectorySnapshot>,

    scene: SceneTimeline,
    initial_scene: SceneTimeline,

    mode: Option<OutputMode>,
    transition: Option<ModeTransition>,
    awaiting_checkpoint: bool,

    pending: Vec<Vec<f32>>,
    output_emitted: u64,
    finished: bool,
}

impl SalsaSpatialDecoder {
    /// Build a decoder from the sequence-header configuration.
    pub fn new(config: SalsaSpatialStreamConfig) -> Result<Self, DecoderError> {
        if config.object_count > MAX_OBJECTS {
            return Err(DecoderError::InvalidConfig(format!(
                "object count {} exceeds the profile maximum of {MAX_OBJECTS}",
                config.object_count
            )));
        }
        if !config.headroom_gain.is_finite() || config.headroom_gain <= 0.0 {
            return Err(DecoderError::InvalidConfig(
                "headroom gain must be finite and positive".to_owned(),
            ));
        }

        config.validate().map_err(|error| {
            DecoderError::InvalidConfig(format!("descriptor validation failed: {error}"))
        })?;
        let object_count = config.object_count;
        let fold = config.fold();
        let panner = CorePanner::from_config(config.core_panner);
        let enhanced_supported = config.enhanced_supported();
        let core = OpusBedDecoder::from_config(&config)?;
        let parameters = SceneParameterDecoder::new(object_count);
        let lookahead_remaining = core.lookahead_samples();

        let base_trajectories = build_base_trajectories(&config);
        let current_trajectories = base_trajectories.clone();

        let objects = (0..object_count)
            .map(|index| output_object(index, position_along(&base_trajectories[index], 0)))
            .collect();
        let scene =
            SceneTimeline::from_payload(&SpatialPayload::new(SAMPLE_RATE, objects, Vec::new()), 0)?;

        let object_synthesis = (0..object_count).map(|_| Synthesis::new()).collect();
        let pending = (0..=object_count).map(|_| Vec::new()).collect();

        Ok(Self {
            headroom: 1.0 / config.headroom_gain,
            object_count,
            core,
            fold,
            panner,
            enhanced_supported,
            parameters,
            core_stfts: std::array::from_fn(|_| Stft::new()),
            lfe_stft: Stft::new(),
            analyzed_hops: 0,
            lookahead_remaining,
            source_origin: None,
            declared_samples: 0,
            object_synthesis,
            lfe_synthesis: Synthesis::new(),
            anchors: Vec::new(),
            current_trajectories,
            base_trajectories,
            trajectory_history: Vec::new(),
            initial_scene: scene.clone(),
            scene,
            mode: None,
            transition: None,
            awaiting_checkpoint: false,
            pending,
            output_emitted: 0,
            finished: false,
        })
    }

    /// Drop all transport and DSP state.
    ///
    /// This restores the decoder to its initial state but keeps the stream
    /// configuration. Seeking must start from a checkpoint and then preroll the
    /// Opus streams and filterbank; this method only provides the clean state
    /// boundary.
    pub fn reset(&mut self) {
        self.core.reset();
        self.parameters.reset();
        self.core_stfts = std::array::from_fn(|_| Stft::new());
        self.lfe_stft = Stft::new();
        self.analyzed_hops = 0;
        self.lookahead_remaining = self.core.lookahead_samples();
        self.source_origin = None;
        self.declared_samples = 0;
        self.object_synthesis = (0..self.object_count).map(|_| Synthesis::new()).collect();
        self.lfe_synthesis = Synthesis::new();
        self.anchors.clear();
        self.current_trajectories = self.base_trajectories.clone();
        self.trajectory_history.clear();
        self.mode = None;
        self.transition = None;
        self.awaiting_checkpoint = true;
        self.scene = self.initial_scene.clone();
        self.pending = (0..=self.object_count).map(|_| Vec::new()).collect();
        self.output_emitted = 0;
        self.finished = false;
    }

    /// Number of source objects (excluding the LFE).
    pub fn object_count(&self) -> usize {
        self.object_count
    }

    /// Emit any remaining samples at the end of the stream.
    pub fn finish(&mut self) -> Result<Option<SpatialFrame>, DecoderError> {
        if self.finished {
            return Ok(None);
        }

        let mut core_spectra: [Vec<Spectrum>; CORE_CHANNEL_COUNT] =
            std::array::from_fn(|_| Vec::new());
        for channel in 0..CORE_CHANNEL_COUNT {
            core_spectra[channel] = self.core_stfts[channel].flush();
        }
        let lfe_spectra = self.lfe_stft.flush();
        let count = core_spectra[0].len();
        for hop in 0..count {
            let center = self.source_origin.unwrap_or(0)
                + (self.analyzed_hops + hop as u64) * HOP_SAMPLES as u64;
            let mut core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
            for channel in 0..CORE_CHANNEL_COUNT {
                core[channel * FFT_BINS..(channel + 1) * FFT_BINS]
                    .copy_from_slice(&core_spectra[channel][hop]);
            }
            self.process_hop(center, &core, &lfe_spectra[hop])?;
        }
        self.analyzed_hops += count as u64;

        let total = self.declared_samples as usize;
        let lfe_tail = self.lfe_synthesis.finish(total);
        self.pending[0].extend(lfe_tail);
        for object in 0..self.object_count {
            let tail = self.object_synthesis[object].finish(total);
            self.pending[object + 1].extend(tail);
        }
        self.align_pending();
        self.finished = true;

        let available = self.pending_len();
        if available == 0 {
            return Ok(None);
        }
        self.take_output(available)
    }

    fn decode_inner(&mut self, frame: &SalsaSpatialFrame) -> Result<SpatialFrame, DecoderError> {
        if frame.sample_rate != 0 && frame.sample_rate != SAMPLE_RATE {
            return Err(DecoderError::SampleRate {
                expected: SAMPLE_RATE,
                actual: frame.sample_rate,
            });
        }

        self.source_origin.get_or_insert(frame.start_sample);
        self.declared_samples += frame.frames as u64;

        // Decode the five Opus substreams and align them on the source clock.
        let mut decoded: [Vec<f32>; 8] = std::array::from_fn(|_| Vec::new());
        let mut extension_bytes = Vec::new();
        for packet in &frame.packets {
            let bed = self.core.decode(packet)?;
            if let Some(fragment) = self.core.take_extension() {
                extension_bytes.extend_from_slice(&fragment);
            }
            for (channel, samples) in bed.into_iter().enumerate() {
                decoded[channel].extend_from_slice(&samples);
            }
        }
        self.discard_lookahead(&mut decoded);

        // Install an independent checkpoint before decoding its tile. The
        // checkpoint has its own entropy stream, so it never depends on the
        // tile payload or on earlier tiles. Installation is atomic: a partially
        // validated checkpoint leaves the committed predictor untouched.
        let checkpoint_installed = match frame.checkpoint.as_ref() {
            Some(checkpoint) if checkpoint.start_sample == frame.start_sample => {
                match self.parameters.install_checkpoint(checkpoint) {
                    Ok(()) => {
                        self.install_checkpoint_trajectories(checkpoint);
                        self.awaiting_checkpoint = false;
                        true
                    }
                    Err(error) => {
                        log::warn!(
                            "salsa-spatial: rejecting checkpoint at {}: {error}",
                            frame.start_sample
                        );
                        self.awaiting_checkpoint = true;
                        false
                    }
                }
            }
            Some(checkpoint) => {
                log::warn!(
                    "salsa-spatial: checkpoint at {} does not match tile {}",
                    checkpoint.start_sample,
                    frame.start_sample
                );
                false
            }
            None => false,
        };

        // Decode the enhancement before touching the timeline. `decode_tile`
        // validates the complete tile and commits predictor state only on
        // success, so a bad packet cannot corrupt later prediction. A stale
        // decoder stays on the bed until a complete checkpoint restores the
        // predictor chain. `frame.parameters` exposes only band-averaged
        // `SceneParameters`, which cannot drive the per-band covariance.
        // Prefer the raw enhancement bytes, then any center-padding fragments,
        // and otherwise fall back to the decoded bed.
        let mut enhancement = if frame.enhancement.bytes.is_empty() {
            extension_bytes
        } else {
            frame.enhancement.bytes.clone()
        };

        let mut decoded_tile = None;
        if !self.enhanced_supported {
            // Unknown or unsupported required models select ordinary base
            // playback instead of substituting a different panner.
        } else if enhancement.is_empty() {
            self.awaiting_checkpoint = true;
        } else if self.awaiting_checkpoint && !checkpoint_installed {
            // A delta tile cannot be trusted against predictor state from
            // before the gap. Keep playing the bed until a checkpoint arrives.
        } else {
            match self
                .parameters
                .decode_tile(frame.start_sample, frame.frames, &enhancement)
            {
                Ok(tile) => decoded_tile = Some(tile),
                Err(error) => {
                    log::warn!(
                        "salsa-spatial: rejecting enhancement for tile {}: {error}",
                        frame.start_sample
                    );
                    self.awaiting_checkpoint = true;
                }
            }
        }
        enhancement.clear();

        let target_mode = if decoded_tile.is_some() {
            OutputMode::Object
        } else {
            OutputMode::Bed
        };
        self.set_mode(frame.start_sample, target_mode);

        if let Some(tile) = decoded_tile.as_ref() {
            self.install_anchors(tile);
        }
        self.push_trajectory_history(frame, decoded_tile.as_ref())?;

        // Run the shared filterbank on the seven full-range channels and the
        // separate LFE analyser.
        let feed_frames = decoded[0].len();
        if feed_frames > 0 {
            let mut core_spectra: [Vec<Spectrum>; CORE_CHANNEL_COUNT] =
                std::array::from_fn(|_| Vec::new());
            for channel in 0..CORE_CHANNEL_COUNT {
                core_spectra[channel] = self.core_stfts[channel].push(&decoded[channel]);
            }
            let lfe_spectra = self.lfe_stft.push(&decoded[LFE_CHANNEL_INDEX]);
            let count = core_spectra[0].len();
            for hop in 0..count {
                let center = self.source_origin.unwrap_or(0)
                    + (self.analyzed_hops + hop as u64) * HOP_SAMPLES as u64;
                let mut core = vec![Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
                for channel in 0..CORE_CHANNEL_COUNT {
                    core[channel * FFT_BINS..(channel + 1) * FFT_BINS]
                        .copy_from_slice(&core_spectra[channel][hop]);
                }
                self.process_hop(center, &core, &lfe_spectra[hop])?;
            }
            self.analyzed_hops += count as u64;
        }

        self.drain_synthesis();
        self.align_pending();
        match self.take_output(TILE_SAMPLES)? {
            Some(frame) => Ok(frame),
            None => self.empty_frame(),
        }
    }

    fn discard_lookahead(&mut self, decoded: &mut [Vec<f32>; 8]) {
        if self.lookahead_remaining == 0 {
            return;
        }
        let available = decoded.iter().map(Vec::len).min().unwrap_or(0);
        let drop = self.lookahead_remaining.min(available);
        if drop == 0 {
            return;
        }
        for channel in decoded.iter_mut() {
            channel.drain(0..drop);
        }
        self.lookahead_remaining -= drop;
    }

    fn install_checkpoint_trajectories(&mut self, checkpoint: &SceneSnapshot) {
        for (index, knots) in checkpoint.trajectories.iter().enumerate() {
            if index < self.current_trajectories.len() && !knots.is_empty() {
                self.current_trajectories[index] = knots.clone();
            }
        }
    }

    fn set_mode(&mut self, start_sample: u64, target: OutputMode) {
        if let Some(previous) = self.mode {
            if previous != target {
                self.transition = Some(ModeTransition {
                    from: previous,
                    to: target,
                    start: start_sample,
                    len: CROSSFADE_SAMPLES,
                });
            }
        } else {
            self.transition = None;
        }
        self.mode = Some(target);
    }

    fn object_weight(&mut self, sample: u64) -> f64 {
        let target = match self.mode {
            Some(OutputMode::Object) => 1.0,
            _ => 0.0,
        };
        let Some(transition) = self.transition else {
            return target;
        };
        let to = if transition.to == OutputMode::Object {
            1.0
        } else {
            0.0
        };
        if transition.len == 0 || sample >= transition.start + transition.len {
            self.transition = None;
            return to;
        }
        let from = if transition.from == OutputMode::Object {
            1.0
        } else {
            0.0
        };
        if sample <= transition.start {
            return from;
        }
        let amount = (sample - transition.start) as f64 / transition.len as f64;
        from + (to - from) * amount.clamp(0.0, 1.0)
    }

    fn install_anchors(&mut self, tile: &ParameterTile) {
        for anchor in &tile.anchors {
            let powers = anchor_band_powers(anchor);
            let covariance = covariance_from_band_powers(&powers, &tile.pairs);
            let mut nu = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
            let mut unobservable = [[false; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
            for band in 0..PARAM_BAND_COUNT {
                for channel in 0..CORE_CHANNEL_COUNT {
                    nu[band][channel] = anchor.error.nu[channel][band];
                    unobservable[band][channel] = anchor.error.unobservable[channel][band];
                }
            }
            let entry = TimelineAnchor {
                sample: anchor.sample,
                covariance,
                nu,
                unobservable,
            };
            match self
                .anchors
                .binary_search_by_key(&anchor.sample, |anchor| anchor.sample)
            {
                Ok(index) => self.anchors[index] = entry,
                Err(index) => self.anchors.insert(index, entry),
            }
        }
        self.prune_anchors(tile.start_sample);
    }

    fn prune_anchors(&mut self, current_start: u64) {
        if self.anchors.len() <= 64 {
            return;
        }
        let cutoff = current_start.saturating_sub(2 * TILE_SAMPLES as u64);
        let keep_from = self
            .anchors
            .partition_point(|anchor| anchor.sample < cutoff)
            .saturating_sub(1);
        if keep_from > 0 {
            self.anchors.drain(0..keep_from);
        }
    }

    fn push_trajectory_history(
        &mut self,
        frame: &SalsaSpatialFrame,
        tile: Option<&ParameterTile>,
    ) -> Result<(), DecoderError> {
        let mut trajectories = self.current_trajectories.clone();
        if let Some(tile) = tile {
            for (index, knots) in tile.trajectories.iter().enumerate() {
                if index < trajectories.len() && !knots.is_empty() {
                    trajectories[index] = knots.clone();
                }
            }
        }
        // The compressed trajectory drives both D and the renderer. Authored
        // gain/activity is already in the PCM; gating the synthesized tail a
        // second time would introduce discontinuities at activity changes.
        let mut objects = Vec::with_capacity(self.object_count);
        let mut events = Vec::new();
        for (object, knots) in trajectories.iter().enumerate() {
            objects.push(output_object(
                object,
                position_along(knots, frame.start_sample),
            ));
            for pair in knots.windows(2) {
                let start = pair[0].sample.max(frame.start_sample);
                let end = pair[1].sample;
                if end <= start || start >= frame.start_sample + frame.frames as u64 {
                    continue;
                }
                events.push(ObjectEvent {
                    offset: (start - frame.start_sample) as i64,
                    transition: Transition::Position((end - start) as u32),
                    state: output_object(object, pair[1].position),
                });
            }
        }
        events.sort_by_key(|event| event.offset);
        self.scene.submit(
            &SpatialPayload::new(SAMPLE_RATE, objects, events),
            frame.start_sample as i64,
        )?;
        self.current_trajectories = trajectories.clone();
        self.trajectory_history.push(TrajectorySnapshot {
            start: frame.start_sample,
            frames: frame.frames.max(1),
            trajectories,
        });
        if self.trajectory_history.len() > MAX_TRAJECTORY_SNAPSHOTS {
            self.trajectory_history.remove(0);
        }
        Ok(())
    }

    fn scene_at(&self, sample: u64) -> Option<SceneView> {
        if self.anchors.is_empty() {
            return None;
        }
        let index = self
            .anchors
            .partition_point(|anchor| anchor.sample <= sample);
        let (first, second, amount) = if index == 0 {
            (&self.anchors[0], &self.anchors[0], 0.0)
        } else if index >= self.anchors.len() {
            let last = &self.anchors[self.anchors.len() - 1];
            (last, last, 0.0)
        } else {
            let first = &self.anchors[index - 1];
            let second = &self.anchors[index];
            let span = second.sample.saturating_sub(first.sample);
            let amount = if span == 0 {
                0.0
            } else {
                ((sample - first.sample) as f64 / span as f64).clamp(0.0, 1.0)
            };
            (first, second, amount)
        };

        let covariance = if amount == 0.0 {
            first.covariance.clone()
        } else {
            first.covariance.interpolate(&second.covariance, amount)
        };

        let mut nu = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
        let mut unobservable = [false; PARAM_BAND_COUNT];
        for band in 0..PARAM_BAND_COUNT {
            for channel in 0..CORE_CHANNEL_COUNT {
                nu[band][channel] =
                    (1.0 - amount) * first.nu[band][channel] + amount * second.nu[band][channel];
                unobservable[band] |=
                    first.unobservable[band][channel] || second.unobservable[band][channel];
            }
        }
        let active = (0..PARAM_BAND_COUNT).any(|band| covariance.trace(band) > TRACE_EPSILON);
        Some(SceneView {
            covariance,
            nu,
            unobservable,
            active,
        })
    }

    fn trajectory_snapshot_for(&self, sample: u64) -> Option<&TrajectorySnapshot> {
        for snapshot in self.trajectory_history.iter().rev() {
            let end = snapshot.start + snapshot.frames as u64;
            if sample >= snapshot.start && sample < end {
                return Some(snapshot);
            }
        }
        self.trajectory_history.last()
    }

    fn knots_at(&self, sample: u64, object: usize) -> &[TrajectoryKnot] {
        if let Some(snapshot) = self.trajectory_snapshot_for(sample)
            && let Some(knots) = snapshot.trajectories.get(object)
            && !knots.is_empty()
        {
            return knots;
        }
        if let Some(knots) = self.current_trajectories.get(object)
            && !knots.is_empty()
        {
            return knots;
        }
        if let Some(knots) = self.base_trajectories.get(object)
            && !knots.is_empty()
        {
            return knots;
        }
        &[]
    }

    fn downmix_at(&self, sample: u64) -> Vec<[f64; CORE_CHANNEL_COUNT]> {
        (0..self.object_count)
            .map(|object| {
                let knots = self.knots_at(sample, object);
                if knots.is_empty() {
                    self.panner.observation_column([0.0, 0.0, 0.0], self.fold)
                } else {
                    self.panner.observation_columns(knots, sample, 1, self.fold)[0]
                }
            })
            .collect()
    }

    fn process_hop(
        &mut self,
        center: u64,
        core: &[Complex<f64>],
        lfe: &[Complex<f64>],
    ) -> Result<(), DecoderError> {
        let weight = self.object_weight(center);
        let scene = self.scene_at(center);

        let estimates = if weight > 0.0 && self.object_count > 0 {
            if let Some(scene) = scene.as_ref() {
                let downmix = self.downmix_at(center);
                let hop = (center / HOP_SAMPLES as u64) as usize;
                Some(estimate_objects(
                    hop,
                    core,
                    &downmix,
                    &scene.covariance,
                    &scene.nu,
                    scene.active,
                    &scene.unobservable,
                )?)
            } else {
                None
            }
        } else {
            None
        };

        self.lfe_synthesis.push_spectrum(lfe);
        if self.object_count == 0 {
            return Ok(());
        }

        for object in 0..self.object_count {
            let bed_needed = estimates.is_none() || weight < 1.0;
            let object_weight = if estimates.is_some() { weight } else { 0.0 };

            if estimates.is_some() && !bed_needed {
                if let Some(estimates) = estimates.as_ref() {
                    self.object_synthesis[object].push_spectrum(&estimates[object]);
                }
                continue;
            }
            if !bed_needed {
                continue;
            }

            let mut spectrum = vec![Complex::new(0.0, 0.0); FFT_BINS];
            if let Some(estimates) = estimates.as_ref()
                && object_weight > 0.0
            {
                for (value, estimate) in spectrum.iter_mut().zip(estimates[object].iter()) {
                    *value += *estimate * object_weight;
                }
            }
            if bed_needed && object < CORE_CHANNEL_COUNT {
                let bed_weight = if estimates.is_some() {
                    1.0 - object_weight
                } else {
                    1.0
                };
                let channel = &core[object * FFT_BINS..(object + 1) * FFT_BINS];
                for (value, sample) in spectrum.iter_mut().zip(channel) {
                    *value += *sample * bed_weight;
                }
            }
            self.object_synthesis[object].push_spectrum(&spectrum);
        }

        Ok(())
    }

    fn drain_synthesis(&mut self) {
        let lfe = self.lfe_synthesis.drain();
        self.pending[0].extend(lfe);
        for object in 0..self.object_count {
            let samples = self.object_synthesis[object].drain();
            self.pending[object + 1].extend(samples);
        }
    }

    fn pending_len(&self) -> usize {
        self.pending.first().map_or(0, Vec::len)
    }

    fn align_pending(&mut self) {
        let length = self.pending.iter().map(Vec::len).min().unwrap_or(0);
        for channel in &mut self.pending {
            channel.truncate(length);
        }
    }

    fn take_output(&mut self, limit: usize) -> Result<Option<SpatialFrame>, DecoderError> {
        let available = self.pending_len();
        if available == 0 {
            return Ok(None);
        }
        let start = self.source_origin.unwrap_or(0) + self.output_emitted;
        let length = available.min(limit.max(1));
        if length == 0 {
            return Ok(None);
        }

        let mut channels = Vec::with_capacity(self.pending.len());
        let headroom = self.headroom as f32;
        for channel in &mut self.pending {
            let mut chunk: Vec<f32> = channel.drain(0..length).collect();
            for sample in &mut chunk {
                *sample *= headroom;
            }
            channels.push(chunk);
        }

        let payload = self.scene.payload(start as i64, length);
        let pcm = PcmBuffer::from_planar(channels)?;
        let spatial = SpatialFrame::new(SAMPLE_RATE, length, payload.objects, payload.events, pcm)?;

        self.output_emitted += length as u64;
        self.scene.discard_before((start + length as u64) as i64);
        Ok(Some(spatial))
    }

    fn empty_frame(&self) -> Result<SpatialFrame, DecoderError> {
        let pcm = PcmBuffer::silent(self.object_count + 1, 0);
        let spatial = SpatialFrame::new(
            SAMPLE_RATE,
            0,
            self.scene
                .states_at((self.source_origin.unwrap_or(0) + self.output_emitted) as i64),
            Vec::new(),
            pcm,
        )?;
        Ok(spatial)
    }
}

fn output_object(index: usize, position: [f64; 3]) -> ObjectState {
    ObjectState::new(index as u32 + 1, position.map(|value| value as f32))
}

impl SpatialDecoder for SalsaSpatialDecoder {
    type Frame = SalsaSpatialFrame;
    type Error = DecoderError;

    fn decode(&mut self, frame: &Self::Frame) -> Result<SpatialFrame, Self::Error> {
        if self.finished {
            return Err(DecoderError::Finished);
        }
        self.decode_inner(frame)
    }
}

/// Reconstruct normalized per-band powers directly from the integer half-dB
/// anchor. Powers are normalized within each band, the maximum log value is
/// removed before exponentiation, the represented active range is bounded to
/// 90 dB, and inactive objects stay exactly zero.
fn anchor_band_powers(anchor: &ParameterAnchor) -> Vec<[f64; PARAM_BAND_COUNT]> {
    let object_count = anchor.active.len().min(anchor.ell_half_db.len());
    let mut powers = vec![[0.0f64; PARAM_BAND_COUNT]; object_count];
    for band in 0..PARAM_BAND_COUNT {
        let mut maximum = f64::NEG_INFINITY;
        for object in 0..object_count {
            if anchor.active[object] {
                let log_power = f64::from(anchor.ell_half_db[object][band]) * HALF_DB_SCALE;
                maximum = maximum.max(log_power);
            }
        }
        if !maximum.is_finite() {
            continue;
        }
        let mut sum = 0.0;
        for object in 0..object_count {
            if !anchor.active[object] {
                continue;
            }
            let log_power = f64::from(anchor.ell_half_db[object][band]) * HALF_DB_SCALE;
            let bounded = (log_power - maximum).max(-90.0);
            let power = 10f64.powf(bounded / 10.0);
            powers[object][band] = power;
            sum += power;
        }
        if sum > 0.0 {
            for row in &mut powers {
                row[band] /= sum;
            }
        }
    }
    powers
}

fn build_base_trajectories(config: &SalsaSpatialStreamConfig) -> Vec<Vec<TrajectoryKnot>> {
    (0..config.object_count)
        .map(|object| {
            if let Some(knots) = config.initial_trajectories.get(object)
                && !knots.is_empty()
            {
                return knots.clone();
            }
            let position = config
                .initial_objects
                .get(object)
                .map_or([0.0; 3], |state| {
                    [
                        f64::from(state.position[0]),
                        f64::from(state.position[1]),
                        f64::from(state.position[2]),
                    ]
                });
            vec![TrajectoryKnot {
                sample: 0,
                position,
            }]
        })
        .collect()
}

fn position_along(knots: &[TrajectoryKnot], sample: u64) -> [f64; 3] {
    if knots.is_empty() {
        return [0.0; 3];
    }
    let after = knots.partition_point(|knot| knot.sample <= sample);
    if after == 0 {
        return knots[0].position;
    }
    if after == knots.len() {
        return knots[knots.len() - 1].position;
    }
    let first = knots[after - 1];
    let second = knots[after];
    if sample == first.sample || second.sample <= first.sample {
        return first.position;
    }
    let amount = (sample - first.sample) as f64 / (second.sample - first.sample) as f64;
    [
        first.position[0] + (second.position[0] - first.position[0]) * amount,
        first.position[1] + (second.position[1] - first.position[1]) * amount,
        first.position[2] + (second.position[2] - first.position[2]) * amount,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_powers_are_normalized_and_inactive_is_zero() {
        let mut ell = vec![[0i32; PARAM_BAND_COUNT]; 3];
        for values in &mut ell[0] {
            *values = 20;
        }
        for values in &mut ell[1] {
            *values = 10;
        }
        for values in &mut ell[2] {
            *values = 100;
        }
        let anchor = ParameterAnchor {
            sample: 0,
            active: vec![true, true, false],
            ell_half_db: ell,
            error: Default::default(),
        };
        let powers = anchor_band_powers(&anchor);
        for band in 0..PARAM_BAND_COUNT {
            assert_eq!(powers[2][band], 0.0);
            let sum: f64 = powers.iter().map(|row| row[band]).sum();
            assert!((sum - 1.0).abs() < 1e-12);
            assert!(powers[0][band] > powers[1][band]);
        }
    }

    #[test]
    fn position_interpolation_is_piecewise_linear() {
        let knots = [
            TrajectoryKnot {
                sample: 0,
                position: [-1.0, 0.0, 0.0],
            },
            TrajectoryKnot {
                sample: 100,
                position: [1.0, 0.0, 0.0],
            },
        ];
        let position = position_along(&knots, 50);
        assert!((position[0]).abs() < 1e-12);
        assert_eq!(position_along(&knots, 1000), [1.0, 0.0, 0.0]);
    }
}
