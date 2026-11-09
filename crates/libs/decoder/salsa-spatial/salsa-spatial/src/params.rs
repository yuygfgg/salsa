//! Quantized scene parameters and their coding state.
//!
//! This module owns the complete, deterministic parameter model used by both
//! the encoder-side RD search and the decoder-side parameter timeline:
//!
//! * relative object powers with an explicit inactive state,
//! * per-band temporal power deltas against the previous anchor,
//! * disjoint bounded correlation pairs,
//! * seven scene-level core-error envelopes with an unobservable-band mask,
//! * piecewise-linear object trajectories, and
//! * independent one-second checkpoints of the complete predictor state.
//!
//! All predictor state is integer-valued. The half-dB state grid is used for
//! log powers; error envelopes use a 2 dB grid. Power conversion happens only
//! *after* reconstruction, never inside a predictor or entropy decision.
//!
//! The tile syntax is:
//!
//! 1. anchor count,
//! 2. per anchor: sample delta, activity changes, active-object power deltas,
//!    and the error envelope,
//! 3. the disjoint correlation pair list and per-band coefficient
//!    innovations,
//! 4. per-object trajectory updates.
//!
//! Every symbol is coded with the adaptive contexts in [`EntropyContexts`].
//! Power deltas are predicted from the previous parameter anchor; inactive
//! objects carry zero power and emit no power symbols. Contexts persist across
//! tiles and reset at each independently coded checkpoint. A checkpoint is
//! serialized with its own reset entropy contexts and describes the state
//! immediately *before* its tile.

use crate::constants::{
    CORE_CHANNEL_COUNT, DCT_COEFF_COUNT, HALF_DB_SCALE, MAX_OBJECTS, PARAM_BAND_COUNT,
};
use crate::correction::{TileCorrection, decode_trailing_corrections};
use crate::entropy::{
    AdaptiveModel, DIRECT_INNOVATION_COUNT, EntropyContexts, EntropyDecoder, EntropyEncoder,
};
use crate::error::{Result, SpatialError};
use crate::snapshot::SceneSnapshot;

const MAX_CORRELATION_PAIRS: usize = 8;
const MIN_ERROR_LOG: i32 = -160;
const MAX_ERROR_LOG: i32 = 160;
const POSITION_STEPS: f64 = 4095.0;
const MAX_TRAJECTORY_KNOTS: usize = 100_000;
/// Payload magic for an independently coded checkpoint.
pub const CHECKPOINT_MAGIC: u32 = 0x5343_4B51; // "SCKQ"

/// One piecewise-linear trajectory knot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrajectoryKnot {
    pub sample: u64,
    /// ADM position quantized with 12-bit precision over the supported range.
    pub position: [f64; 3],
}

/// One trajectory anchor, kept for syntax symmetry with the design.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrajectoryAnchor {
    pub sample: u64,
    pub position: [f64; 3],
}

/// One quantized complex correlation coefficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CorrelationCoefficient {
    /// Magnitude in 1/16 steps, 0..=15.
    pub magnitude: u8,
    /// Phase in 1/32 of a turn, 0..=31.
    pub phase: u8,
}

/// A disjoint correlated pair and its per-band coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CorrelationPair {
    pub a: u16,
    pub b: u16,
    pub rho: [Option<CorrelationCoefficient>; PARAM_BAND_COUNT],
}

/// Decoded covariance anchor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CovarianceAnchor {
    /// Normalized object powers, sum one over active objects.
    pub q: Vec<f64>,
    pub pairs: Vec<CorrelationPair>,
}

/// Seven measured core-error envelopes.
#[derive(Debug, Clone, PartialEq)]
pub struct CoreErrorEnvelope {
    pub nu: [[f64; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
    pub unobservable: [[bool; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
}

impl Default for CoreErrorEnvelope {
    fn default() -> Self {
        Self {
            nu: [[0.0; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
            unobservable: [[false; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
        }
    }
}

impl CoreErrorEnvelope {
    /// Reconstruct the values represented by the two-dB syntax grid.
    pub fn quantized(&self) -> Self {
        let mut result = self.clone();
        for channel in 0..CORE_CHANNEL_COUNT {
            for band in 0..PARAM_BAND_COUNT {
                result.nu[channel][band] = if self.unobservable[channel][band] {
                    0.0
                } else {
                    dequantize_error(quantize_error(self.nu[channel][band]))
                };
            }
        }
        result
    }
}

/// Complete decoded parameters at one anchor on the 5 ms grid.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneParameters {
    pub sample: u64,
    pub covariance: CovarianceAnchor,
    pub error: CoreErrorEnvelope,
}

impl SceneParameters {
    /// Reconstruct DSP-facing parameters from a decoded anchor.
    ///
    /// Active objects are normalised with the maximum log power subtracted
    /// before exponentiation, the represented active range is bounded to
    /// 90 dB, and inactive objects receive exactly zero power. A common offset
    /// in a band therefore changes nothing.
    pub fn from_anchor(anchor: &ParameterAnchor, pairs: &[CorrelationPair]) -> SceneParameters {
        let object_count = anchor.active.len().min(anchor.ell_half_db.len());
        let mut powers = vec![0.0f64; object_count];
        let mut mean_log = vec![f64::NEG_INFINITY; object_count];
        let mut max_log = f64::NEG_INFINITY;

        // The decoded anchor carries one log-power vector per object. Summarise
        // it into the band-averaged relative power expected by
        // `CovarianceAnchor::q`, keeping the exact inactive-zero state.
        for object in 0..object_count {
            if !anchor.active[object] {
                continue;
            }
            let mut sum = 0.0f64;
            for band in 0..PARAM_BAND_COUNT {
                sum += f64::from(anchor.ell_half_db[object][band]) * HALF_DB_SCALE;
            }
            let mean = sum / PARAM_BAND_COUNT as f64;
            mean_log[object] = mean;
            max_log = max_log.max(mean);
        }

        if max_log.is_finite() {
            let mut sum = 0.0f64;
            for object in 0..object_count {
                if mean_log[object].is_finite() {
                    // Subtract the maximum before exponentiation and bound the
                    // represented active range to 90 dB.
                    let bounded = (mean_log[object] - max_log).max(-90.0);
                    powers[object] = 10f64.powf(bounded / 10.0);
                    sum += powers[object];
                }
            }
            if sum > 0.0 {
                for power in &mut powers {
                    *power /= sum;
                }
            }
        }

        SceneParameters {
            sample: anchor.sample,
            covariance: CovarianceAnchor {
                q: powers,
                pairs: pairs.to_vec(),
            },
            error: anchor.error.clone(),
        }
    }
}

/// Quantized object activity and log-power state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObjectPowerState {
    pub active: Vec<bool>,
    /// [object][band], half-dB units.
    pub ell_half_db: Vec<[i32; PARAM_BAND_COUNT]>,
}

/// Orthonormal 24-point DCT-II basis.
#[derive(Debug, Clone)]
pub struct DctBasis {
    pub basis: [[f64; PARAM_BAND_COUNT]; DCT_COEFF_COUNT],
}

impl Default for DctBasis {
    fn default() -> Self {
        Self::new()
    }
}

impl DctBasis {
    pub fn new() -> Self {
        let mut basis = [[0.0; PARAM_BAND_COUNT]; DCT_COEFF_COUNT];
        for (r, row) in basis.iter_mut().enumerate() {
            for (b, value) in row.iter_mut().enumerate() {
                let norm = if r == 0 {
                    (1.0 / PARAM_BAND_COUNT as f64).sqrt()
                } else {
                    (2.0 / PARAM_BAND_COUNT as f64).sqrt()
                };
                *value = norm
                    * (std::f64::consts::PI * (b as f64 + 0.5) * r as f64
                        / PARAM_BAND_COUNT as f64)
                        .cos();
            }
        }
        Self { basis }
    }

    pub fn analysis(&self, band_values: &[f64; PARAM_BAND_COUNT]) -> [f64; DCT_COEFF_COUNT] {
        let mut out = [0.0; DCT_COEFF_COUNT];
        for (r, value) in out.iter_mut().enumerate() {
            *value = self.basis[r]
                .iter()
                .zip(band_values)
                .map(|(basis, x)| basis * x)
                .sum();
        }
        out
    }

    pub fn synthesis(&self, coefficients: &[f64; DCT_COEFF_COUNT]) -> [f64; PARAM_BAND_COUNT] {
        let mut out = [0.0; PARAM_BAND_COUNT];
        for (b, value) in out.iter_mut().enumerate() {
            *value = (0..DCT_COEFF_COUNT)
                .map(|r| self.basis[r][b] * coefficients[r])
                .sum();
        }
        out
    }
}

/// One decoded parameter anchor.
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterAnchor {
    pub sample: u64,
    pub active: Vec<bool>,
    /// `[object][band]` reconstructed log power, integer half-dB state grid.
    pub ell_half_db: Vec<[i32; PARAM_BAND_COUNT]>,
    pub error: CoreErrorEnvelope,
}

impl Default for ParameterAnchor {
    fn default() -> Self {
        Self {
            sample: 0,
            active: Vec::new(),
            ell_half_db: Vec::new(),
            error: CoreErrorEnvelope::default(),
        }
    }
}

/// One 100 ms (or shorter final) parameter tile supplied to the encoder.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParameterTile {
    pub start_sample: u64,
    pub frames: usize,
    pub anchors: Vec<ParameterAnchor>,
    /// Tile-level disjoint correlation pairs (same pair list for every
    /// band/anchor in the tile).
    pub pairs: Vec<CorrelationPair>,
    /// Per-object piecewise-linear trajectory knots in absolute sample
    /// positions.
    pub trajectories: Vec<Vec<TrajectoryKnot>>,
    /// Optional rank-r reconstruction-matrix correction keyframes for this
    /// tile. The decoder spaces them evenly over the tile and interpolates.
    pub correction: Vec<TileCorrection>,
}

#[derive(Debug, Clone)]
struct ScenePredictor {
    object_count: usize,
    active: Vec<bool>,
    ell_half_db: Vec<[i32; PARAM_BAND_COUNT]>,
    error_log: [[i32; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
    error_unobservable: [[bool; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
    pairs: Vec<CorrelationPair>,
    trajectories: Vec<Vec<TrajectoryKnot>>,
}

impl ScenePredictor {
    fn new(object_count: usize) -> Self {
        Self {
            object_count,
            active: vec![false; object_count],
            ell_half_db: vec![[0; PARAM_BAND_COUNT]; object_count],
            error_log: [[0; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
            error_unobservable: [[false; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
            pairs: Vec::new(),
            trajectories: vec![Vec::new(); object_count],
        }
    }

    fn snapshot_state(&self, start_sample: u64) -> SceneSnapshot {
        SceneSnapshot {
            start_sample,
            object_count: self.object_count,
            active: self.active.clone(),
            trajectories: self.trajectories.clone(),
            power: ObjectPowerState {
                active: self.active.clone(),
                ell_half_db: self.ell_half_db.clone(),
            },
            error_log: self.error_log,
            error_unobservable: self.error_unobservable,
            correlation_pairs: self.pairs.clone(),
            reset_predictors: false,
        }
    }

    fn apply_snapshot(&mut self, snapshot: &SceneSnapshot) -> Result<()> {
        if snapshot.object_count != self.object_count {
            return Err(SpatialError::Parameter("checkpoint object count mismatch"));
        }
        if snapshot.object_count > MAX_OBJECTS || snapshot.active != snapshot.power.active {
            return Err(SpatialError::Parameter("invalid checkpoint activity state"));
        }
        if snapshot.active.len() != self.object_count
            || snapshot.power.active.len() != self.object_count
            || snapshot.power.ell_half_db.len() != self.object_count
            || snapshot.trajectories.len() != self.object_count
        {
            return Err(SpatialError::Parameter("checkpoint state length mismatch"));
        }
        self.active = snapshot.active.clone();
        self.ell_half_db = snapshot.power.ell_half_db.clone();
        self.error_log = snapshot.error_log;
        self.error_unobservable = snapshot.error_unobservable;
        self.pairs = snapshot.correlation_pairs.clone();
        self.trajectories = snapshot.trajectories.clone();
        Ok(())
    }
}

/// Encoder-side parameter state and entropy contexts.
#[derive(Debug, Clone)]
pub struct SceneParameterEncoder {
    object_count: usize,
    predictor: ScenePredictor,
    contexts: EntropyContexts,
}

impl SceneParameterEncoder {
    pub fn new(object_count: usize) -> Self {
        assert!(object_count <= MAX_OBJECTS, "too many objects");
        Self {
            object_count,
            predictor: ScenePredictor::new(object_count),
            contexts: EntropyContexts::new(object_count),
        }
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn reset(&mut self) {
        self.predictor = ScenePredictor::new(self.object_count);
        self.contexts = EntropyContexts::new(self.object_count);
    }

    /// Reset entropy contexts at an independently coded checkpoint.
    pub fn reset_entropy(&mut self) {
        self.contexts = EntropyContexts::new(self.object_count);
    }

    /// State immediately before the tile that starts at `start_sample`.
    pub fn checkpoint(&self, start_sample: u64) -> SceneSnapshot {
        self.snapshot(start_sample)
    }

    /// Current reusable predictor snapshot (for diagnostics and checkpoints).
    pub fn snapshot(&self, start_sample: u64) -> SceneSnapshot {
        self.predictor.snapshot_state(start_sample)
    }

    /// Encode one tile to rANS bytes.
    pub fn encode_tile(&mut self, tile: &ParameterTile) -> Result<Vec<u8>> {
        let (bytes, _bits, predictor, contexts) = self.encode_tile_impl(tile, true)?;
        self.predictor = predictor;
        self.contexts = contexts;
        Ok(bytes)
    }

    /// Estimate bits from the current integer CDF state without mutating
    /// committed state.
    pub fn estimate_tile_bits(&self, tile: &ParameterTile) -> Result<f64> {
        let (_bytes, bits, _predictor, _contexts) = self.encode_tile_impl(tile, false)?;
        Ok(bits)
    }

    fn encode_tile_impl(
        &self,
        tile: &ParameterTile,
        build_bytes: bool,
    ) -> Result<(Vec<u8>, f64, ScenePredictor, EntropyContexts)> {
        let object_count = self.object_count;
        validate_tile(tile, object_count)?;

        let mut state = self.predictor.clone();
        let mut contexts = self.contexts.clone();
        let mut encoder = EntropyEncoder::new();

        encode_power(&mut encoder, &mut contexts, &mut state, tile)?;
        encode_correlations(&mut encoder, &mut contexts, &mut state, tile)?;
        encode_trajectories(&mut encoder, &mut contexts, &mut state, tile)?;

        let mut bits = encoder.estimated_total_bits();
        let mut bytes = if build_bytes {
            encoder.finish()
        } else {
            Vec::new()
        };
        if tile
            .correction
            .iter()
            .any(|correction| !correction.is_empty())
        {
            let trailer = crate::correction::encode_keyframes(&tile.correction);
            bits += 8.0 * trailer.len() as f64;
            if build_bytes {
                bytes.extend_from_slice(&trailer);
            }
        }
        Ok((bytes, bits, state, contexts))
    }
}

/// Decoder-side parameter state and entropy contexts.
#[derive(Debug, Clone)]
pub struct SceneParameterDecoder {
    object_count: usize,
    predictor: ScenePredictor,
    contexts: EntropyContexts,
}

impl SceneParameterDecoder {
    pub fn new(object_count: usize) -> Self {
        assert!(object_count <= MAX_OBJECTS, "too many objects");
        Self {
            object_count,
            predictor: ScenePredictor::new(object_count),
            contexts: EntropyContexts::new(object_count),
        }
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn reset(&mut self) {
        self.predictor = ScenePredictor::new(self.object_count);
        self.contexts = EntropyContexts::new(self.object_count);
    }

    /// Reset entropy contexts at an independently coded checkpoint.
    pub fn reset_entropy(&mut self) {
        self.contexts = EntropyContexts::new(self.object_count);
    }

    /// State immediately before the tile that starts at `start_sample`.
    pub fn checkpoint(&self, start_sample: u64) -> SceneSnapshot {
        self.snapshot(start_sample)
    }

    pub fn snapshot(&self, start_sample: u64) -> SceneSnapshot {
        self.predictor.snapshot_state(start_sample)
    }

    /// Validate and install a checkpoint as the committed predictor state.
    ///
    /// The checkpoint replaces the integer predictors and trajectory state and
    /// resets the entropy contexts, matching the independent checkpoint stream.
    /// A checkpoint that fails validation leaves the previous state untouched.
    pub fn install_checkpoint(&mut self, checkpoint: &SceneSnapshot) -> Result<()> {
        let mut predictor = self.predictor.clone();
        predictor.apply_snapshot(checkpoint)?;
        self.predictor = predictor;
        self.contexts = EntropyContexts::new(self.object_count);
        Ok(())
    }

    /// Decode one tile.
    ///
    /// The payload is fully validated in temporary state and committed only
    /// after the complete tile and the rANS end marker have been verified.
    pub fn decode_tile(
        &mut self,
        start_sample: u64,
        frames: usize,
        bytes: &[u8],
    ) -> Result<ParameterTile> {
        let (tile, predictor, contexts) = self.decode_tile_impl(start_sample, frames, bytes)?;
        self.predictor = predictor;
        self.contexts = contexts;
        Ok(tile)
    }

    fn decode_tile_impl(
        &self,
        start_sample: u64,
        frames: usize,
        bytes: &[u8],
    ) -> Result<(ParameterTile, ScenePredictor, EntropyContexts)> {
        let mut state = self.predictor.clone();
        let mut contexts = self.contexts.clone();
        let mut decoder = EntropyDecoder::new(bytes)?;

        let anchors = decode_power(&mut decoder, &mut contexts, &mut state, start_sample)?;
        let pairs = decode_correlations(&mut decoder, &mut contexts, &mut state)?;
        let trajectories =
            decode_trajectories(&mut decoder, &mut contexts, &mut state, start_sample)?;

        decoder.finish()?;

        let tile = ParameterTile {
            start_sample,
            frames,
            anchors,
            pairs,
            trajectories,
            correction: decode_trailing_corrections(bytes)?,
        };
        Ok((tile, state, contexts))
    }
}

fn validate_tile(tile: &ParameterTile, object_count: usize) -> Result<()> {
    if tile.trajectories.len() != object_count {
        return Err(SpatialError::Parameter("trajectory object count mismatch"));
    }
    for anchor in &tile.anchors {
        if anchor.active.len() != object_count || anchor.ell_half_db.len() != object_count {
            return Err(SpatialError::Parameter("anchor object count mismatch"));
        }
    }
    if tile.pairs.len() > MAX_CORRELATION_PAIRS {
        return Err(SpatialError::Parameter("too many correlation pairs"));
    }
    for correction in &tile.correction {
        if correction.bands.len() != PARAM_BAND_COUNT {
            return Err(SpatialError::Parameter("correction band count mismatch"));
        }
    }
    Ok(())
}

fn enc_signed(
    encoder: &mut EntropyEncoder,
    model: &mut AdaptiveModel,
    tail: &mut AdaptiveModel,
    value: i64,
) -> Result<()> {
    encoder.encode_signed_escape(value, DIRECT_INNOVATION_COUNT, model, tail)
}

fn dec_signed(
    decoder: &mut EntropyDecoder<'_>,
    model: &mut AdaptiveModel,
    tail: &mut AdaptiveModel,
) -> Result<i64> {
    decoder.decode_signed_escape(DIRECT_INNOVATION_COUNT, model, tail)
}

fn enc_unsigned(
    encoder: &mut EntropyEncoder,
    model: &mut AdaptiveModel,
    tail: &mut AdaptiveModel,
    value: u64,
) -> Result<()> {
    encoder.encode_unsigned_escape(value, DIRECT_INNOVATION_COUNT, model, tail)
}

fn dec_unsigned(
    decoder: &mut EntropyDecoder<'_>,
    model: &mut AdaptiveModel,
    tail: &mut AdaptiveModel,
) -> Result<u64> {
    decoder.decode_unsigned_escape(DIRECT_INNOVATION_COUNT, model, tail)
}

fn encode_power(
    encoder: &mut EntropyEncoder,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
    tile: &ParameterTile,
) -> Result<()> {
    let object_count = state.object_count;
    enc_unsigned(
        encoder,
        &mut contexts.anchor_count,
        &mut contexts.tail,
        tile.anchors.len() as u64,
    )?;

    let mut previous_sample = tile.start_sample as i64;
    for anchor in &tile.anchors {
        let delta = anchor.sample as i64 - previous_sample;
        enc_signed(
            encoder,
            &mut contexts.anchor_time,
            &mut contexts.tail,
            delta,
        )?;
        previous_sample = anchor.sample as i64;

        for object in 0..object_count {
            let new_active = anchor.active[object];
            let changed = new_active != state.active[object];
            encoder.encode(&mut contexts.activity_change, usize::from(changed))?;
            if changed {
                encoder.encode(&mut contexts.activity_value, usize::from(new_active))?;
                state.active[object] = new_active;
            }
        }

        for object in 0..object_count {
            if !anchor.active[object] {
                state.ell_half_db[object] = [0; PARAM_BAND_COUNT];
                continue;
            }
            let target = anchor.ell_half_db[object];
            for band in 0..PARAM_BAND_COUNT {
                let delta = i64::from(target[band]) - i64::from(state.ell_half_db[object][band]);
                enc_signed(
                    encoder,
                    &mut contexts.power_delta[band],
                    &mut contexts.tail,
                    delta,
                )?;
            }
            state.ell_half_db[object] = target;
        }

        encode_error_envelope(encoder, contexts, state, anchor)?;
    }

    Ok(())
}

fn decode_power(
    decoder: &mut EntropyDecoder<'_>,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
    start_sample: u64,
) -> Result<Vec<ParameterAnchor>> {
    let object_count = state.object_count;
    let anchor_count =
        dec_unsigned(decoder, &mut contexts.anchor_count, &mut contexts.tail)? as usize;
    if anchor_count > MAX_TRAJECTORY_KNOTS {
        return Err(SpatialError::Parameter("too many parameter anchors"));
    }

    let mut anchors = Vec::with_capacity(anchor_count);
    let mut previous_sample = start_sample as i64;
    for _ in 0..anchor_count {
        let delta = dec_signed(decoder, &mut contexts.anchor_time, &mut contexts.tail)?;
        let sample = previous_sample
            .checked_add(delta)
            .ok_or(SpatialError::Parameter("anchor time overflow"))?;
        if sample < 0 {
            return Err(SpatialError::Parameter("negative anchor sample"));
        }
        previous_sample = sample;

        let mut active = vec![false; object_count];
        for object in 0..object_count {
            let changed = decoder.decode(&mut contexts.activity_change)? == 1;
            if changed {
                let value = decoder.decode(&mut contexts.activity_value)?;
                if value > 1 {
                    return Err(SpatialError::Parameter("invalid activity flag"));
                }
                active[object] = value == 1;
                state.active[object] = active[object];
            } else {
                active[object] = state.active[object];
            }
        }

        let mut ell_half_db = vec![[0i32; PARAM_BAND_COUNT]; object_count];
        for object in 0..object_count {
            if !active[object] {
                state.ell_half_db[object] = [0; PARAM_BAND_COUNT];
                continue;
            }
            let mut ell = [0i32; PARAM_BAND_COUNT];
            for band in 0..PARAM_BAND_COUNT {
                let delta =
                    dec_signed(decoder, &mut contexts.power_delta[band], &mut contexts.tail)?;
                let value = i64::from(state.ell_half_db[object][band]) + delta;
                ell[band] =
                    i32::try_from(value).map_err(|_| SpatialError::Parameter("power overflow"))?;
            }
            state.ell_half_db[object] = ell;
            ell_half_db[object] = ell;
        }

        let error = decode_error_envelope(decoder, contexts, state)?;
        anchors.push(ParameterAnchor {
            sample: sample as u64,
            active,
            ell_half_db,
            error,
        });
    }

    Ok(anchors)
}

fn quantize_error(nu: f64) -> i32 {
    if !nu.is_finite() || nu <= 0.0 {
        return MIN_ERROR_LOG;
    }
    let db = 10.0 * nu.log10();
    let quantized = (db / 2.0).round() as i32;
    quantized.clamp(MIN_ERROR_LOG, MAX_ERROR_LOG)
}

fn dequantize_error(quantized: i32) -> f64 {
    if quantized <= MIN_ERROR_LOG {
        0.0
    } else {
        10f64.powf(2.0 * f64::from(quantized) / 10.0)
    }
}

fn encode_error_envelope(
    encoder: &mut EntropyEncoder,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
    anchor: &ParameterAnchor,
) -> Result<()> {
    let mut current = [[0i32; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT];
    for channel in 0..CORE_CHANNEL_COUNT {
        for band in 0..PARAM_BAND_COUNT {
            let unobservable = anchor.error.unobservable[channel][band];
            let changed = unobservable != state.error_unobservable[channel][band];
            encoder.encode(&mut contexts.error_mask_change, usize::from(changed))?;
            if changed {
                encoder.encode(&mut contexts.error_mask_value, usize::from(unobservable))?;
                state.error_unobservable[channel][band] = unobservable;
            }

            let quantized = if unobservable {
                MIN_ERROR_LOG
            } else {
                quantize_error(anchor.error.nu[channel][band])
            };
            let predicted = if band == 0 {
                state.error_log[channel][0]
            } else {
                current[channel][band - 1] + state.error_log[channel][band]
                    - state.error_log[channel][band - 1]
            };
            enc_signed(
                encoder,
                &mut contexts.error_innovation[channel],
                &mut contexts.tail,
                i64::from(quantized) - i64::from(predicted),
            )?;
            state.error_log[channel][band] = quantized;
            current[channel][band] = quantized;
        }
    }
    Ok(())
}

fn decode_error_envelope(
    decoder: &mut EntropyDecoder<'_>,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
) -> Result<CoreErrorEnvelope> {
    let mut envelope = CoreErrorEnvelope::default();
    let mut current = [[0i32; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT];
    for channel in 0..CORE_CHANNEL_COUNT {
        for band in 0..PARAM_BAND_COUNT {
            let changed = decoder.decode(&mut contexts.error_mask_change)? == 1;
            if changed {
                let value = decoder.decode(&mut contexts.error_mask_value)?;
                if value > 1 {
                    return Err(SpatialError::Parameter("invalid error mask"));
                }
                state.error_unobservable[channel][band] = value == 1;
            }
            let unobservable = state.error_unobservable[channel][band];

            let predicted = if band == 0 {
                state.error_log[channel][0]
            } else {
                current[channel][band - 1] + state.error_log[channel][band]
                    - state.error_log[channel][band - 1]
            };
            let innovation = dec_signed(
                decoder,
                &mut contexts.error_innovation[channel],
                &mut contexts.tail,
            )?;
            let quantized = i64::from(predicted) + innovation;
            let quantized = i32::try_from(quantized)
                .map_err(|_| SpatialError::Parameter("error envelope overflow"))?
                .clamp(MIN_ERROR_LOG, MAX_ERROR_LOG);
            state.error_log[channel][band] = quantized;
            current[channel][band] = quantized;

            envelope.unobservable[channel][band] = unobservable;
            envelope.nu[channel][band] = if unobservable {
                0.0
            } else {
                dequantize_error(quantized)
            };
        }
    }
    Ok(envelope)
}

fn find_previous_coefficient(
    pairs: &[CorrelationPair],
    a: u16,
    b: u16,
    band: usize,
) -> Option<CorrelationCoefficient> {
    pairs
        .iter()
        .find(|pair| pair.a == a && pair.b == b)
        .and_then(|pair| pair.rho[band])
}

fn wrap_phase_delta(delta: i32) -> i32 {
    let steps = crate::constants::CORRELATION_PHASE_STEPS as i32;
    let wrapped = (delta + steps / 2).rem_euclid(steps) - steps / 2;
    wrapped
}

fn encode_correlations(
    encoder: &mut EntropyEncoder,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
    tile: &ParameterTile,
) -> Result<()> {
    let object_count = state.object_count;
    let max_pairs = core::cmp::min(MAX_CORRELATION_PAIRS, object_count / 2);
    if tile.pairs.len() > max_pairs {
        return Err(SpatialError::Parameter("too many correlation pairs"));
    }
    if contexts.correlation_count.symbol_count() != max_pairs + 1 {
        return Err(SpatialError::Parameter("correlation context mismatch"));
    }
    encoder.encode(&mut contexts.correlation_count, tile.pairs.len())?;

    let mut used = vec![false; object_count];
    for pair in &tile.pairs {
        let a = usize::from(pair.a);
        let b = usize::from(pair.b);
        if a >= object_count || b >= object_count || a == b {
            return Err(SpatialError::Parameter("invalid correlation pair"));
        }
        if used[a] || used[b] {
            return Err(SpatialError::Parameter("correlation pair is not disjoint"));
        }
        used[a] = true;
        used[b] = true;
        encoder.encode(&mut contexts.correlation_pair_a, a)?;
        encoder.encode(&mut contexts.correlation_pair_b, b)?;

        for band in 0..PARAM_BAND_COUNT {
            let current = pair.rho[band];
            encoder.encode(
                &mut contexts.correlation_present,
                usize::from(current.is_some()),
            )?;
            let Some(coefficient) = current else {
                continue;
            };
            if coefficient.magnitude > crate::constants::CORRELATION_MAX_MAGNITUDE {
                return Err(SpatialError::Parameter(
                    "correlation magnitude out of range",
                ));
            }
            let previous = find_previous_coefficient(&state.pairs, pair.a, pair.b, band);
            let previous_magnitude = previous.map_or(0, |value| i32::from(value.magnitude));
            let innovation = i32::from(coefficient.magnitude) - previous_magnitude;
            enc_signed(
                encoder,
                &mut contexts.correlation_magnitude[band],
                &mut contexts.tail,
                i64::from(innovation),
            )?;

            if coefficient.magnitude > 0 {
                let previous_phase = previous
                    .filter(|value| value.magnitude > 0)
                    .map_or(0, |value| i32::from(value.phase));
                let delta = wrap_phase_delta(i32::from(coefficient.phase) - previous_phase);
                enc_signed(
                    encoder,
                    &mut contexts.correlation_phase[band],
                    &mut contexts.tail,
                    i64::from(delta),
                )?;
            }
        }
    }

    state.pairs = tile.pairs.clone();
    Ok(())
}

fn decode_correlations(
    decoder: &mut EntropyDecoder<'_>,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
) -> Result<Vec<CorrelationPair>> {
    let object_count = state.object_count;
    let max_pairs = core::cmp::min(MAX_CORRELATION_PAIRS, object_count / 2);
    let pair_count = decoder.decode(&mut contexts.correlation_count)?;
    if pair_count > max_pairs {
        return Err(SpatialError::Parameter("too many correlation pairs"));
    }

    let mut pairs = Vec::with_capacity(pair_count);
    let mut used = vec![false; object_count];
    for _ in 0..pair_count {
        let a = decoder.decode(&mut contexts.correlation_pair_a)?;
        let b = decoder.decode(&mut contexts.correlation_pair_b)?;
        if a >= object_count || b >= object_count || a == b {
            return Err(SpatialError::Parameter("invalid correlation pair"));
        }
        if used[a] || used[b] {
            return Err(SpatialError::Parameter("correlation pair is not disjoint"));
        }
        used[a] = true;
        used[b] = true;

        let mut rho = [None; PARAM_BAND_COUNT];
        for band in 0..PARAM_BAND_COUNT {
            let present = decoder.decode(&mut contexts.correlation_present)?;
            if present == 0 {
                continue;
            }
            let previous = find_previous_coefficient(&state.pairs, a as u16, b as u16, band);
            let previous_magnitude = previous.map_or(0, |value| i32::from(value.magnitude));
            let innovation = dec_signed(
                decoder,
                &mut contexts.correlation_magnitude[band],
                &mut contexts.tail,
            )?;
            let magnitude = i64::from(previous_magnitude) + innovation;
            let max_magnitude = i64::from(crate::constants::CORRELATION_MAX_MAGNITUDE);
            if !(0..=max_magnitude).contains(&magnitude) {
                return Err(SpatialError::Parameter(
                    "correlation magnitude out of range",
                ));
            }
            let magnitude = magnitude as u8;
            let mut phase = 0u8;
            if magnitude > 0 {
                let previous_phase = previous
                    .filter(|value| value.magnitude > 0)
                    .map_or(0, |value| i32::from(value.phase));
                let delta = dec_signed(
                    decoder,
                    &mut contexts.correlation_phase[band],
                    &mut contexts.tail,
                )?;
                let steps = crate::constants::CORRELATION_PHASE_STEPS as i32;
                phase = (previous_phase + delta as i32).rem_euclid(steps) as u8;
            }
            rho[band] = Some(CorrelationCoefficient { magnitude, phase });
        }
        pairs.push(CorrelationPair {
            a: a as u16,
            b: b as u16,
            rho,
        });
    }

    state.pairs = pairs.clone();
    Ok(pairs)
}

fn position_limits(axis: usize) -> (f64, f64) {
    if axis < 2 { (-1.0, 1.0) } else { (0.0, 1.0) }
}

pub(crate) fn quantize_position(axis: usize, value: f64) -> u16 {
    let (low, high) = position_limits(axis);
    let normalized = ((value - low) / (high - low)).clamp(0.0, 1.0);
    (normalized * POSITION_STEPS).round() as u16
}

pub(crate) fn dequantize_position(axis: usize, code: u16) -> f64 {
    let (low, high) = position_limits(axis);
    low + (f64::from(code) / POSITION_STEPS) * (high - low)
}

fn encode_trajectories(
    encoder: &mut EntropyEncoder,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
    tile: &ParameterTile,
) -> Result<()> {
    for object in 0..state.object_count {
        let knots = &tile.trajectories[object];
        let update = !knots.is_empty() && knots != &state.trajectories[object];
        encoder.encode(&mut contexts.trajectory_update, usize::from(update))?;
        if !update {
            continue;
        }

        enc_unsigned(
            encoder,
            &mut contexts.trajectory_count,
            &mut contexts.tail,
            knots.len() as u64,
        )?;
        let mut previous_sample = state.trajectories[object]
            .last()
            .map_or(tile.start_sample as i64, |knot| knot.sample as i64);
        let mut previous_position = [0u16; 3];
        if let Some(last) = state.trajectories[object].last() {
            for axis in 0..3 {
                previous_position[axis] = quantize_position(axis, last.position[axis]);
            }
        }
        let mut decoded = Vec::with_capacity(knots.len());
        for knot in knots {
            let delta = knot.sample as i64 - previous_sample;
            enc_signed(
                encoder,
                &mut contexts.trajectory_time,
                &mut contexts.tail,
                delta,
            )?;
            previous_sample = knot.sample as i64;
            let mut position = [0.0f64; 3];
            for axis in 0..3 {
                let code = quantize_position(axis, knot.position[axis]);
                let code_delta = i64::from(code) - i64::from(previous_position[axis]);
                enc_signed(
                    encoder,
                    &mut contexts.trajectory_position_delta,
                    &mut contexts.tail,
                    code_delta,
                )?;
                previous_position[axis] = code;
                position[axis] = dequantize_position(axis, code);
            }
            decoded.push(TrajectoryKnot {
                sample: knot.sample,
                position,
            });
        }
        state.trajectories[object] = decoded;
    }
    Ok(())
}

fn decode_trajectories(
    decoder: &mut EntropyDecoder<'_>,
    contexts: &mut EntropyContexts,
    state: &mut ScenePredictor,
    start_sample: u64,
) -> Result<Vec<Vec<TrajectoryKnot>>> {
    let mut trajectories = Vec::with_capacity(state.object_count);
    for object in 0..state.object_count {
        let update = decoder.decode(&mut contexts.trajectory_update)? == 1;
        if !update {
            trajectories.push(state.trajectories[object].clone());
            continue;
        }

        let knot_count =
            dec_unsigned(decoder, &mut contexts.trajectory_count, &mut contexts.tail)? as usize;
        if knot_count > MAX_TRAJECTORY_KNOTS {
            return Err(SpatialError::Parameter("too many trajectory knots"));
        }
        let mut previous_sample = state.trajectories[object]
            .last()
            .map_or(start_sample as i64, |knot| knot.sample as i64);
        let mut previous_position = [0u16; 3];
        if let Some(last) = state.trajectories[object].last() {
            for axis in 0..3 {
                previous_position[axis] = quantize_position(axis, last.position[axis]);
            }
        }
        let mut knots = Vec::with_capacity(knot_count);
        for _ in 0..knot_count {
            let delta = dec_signed(decoder, &mut contexts.trajectory_time, &mut contexts.tail)?;
            let sample = previous_sample
                .checked_add(delta)
                .ok_or(SpatialError::Parameter("trajectory time overflow"))?;
            if sample < 0 {
                return Err(SpatialError::Parameter("negative trajectory sample"));
            }
            previous_sample = sample;
            let mut position = [0.0f64; 3];
            for axis in 0..3 {
                let code_delta = dec_signed(
                    decoder,
                    &mut contexts.trajectory_position_delta,
                    &mut contexts.tail,
                )?;
                let code = (i64::from(previous_position[axis]) + code_delta)
                    .clamp(0, POSITION_STEPS as i64) as u16;
                previous_position[axis] = code;
                position[axis] = dequantize_position(axis, code);
            }
            knots.push(TrajectoryKnot {
                sample: sample as u64,
                position,
            });
        }
        state.trajectories[object] = knots.clone();
        trajectories.push(knots);
    }
    Ok(trajectories)
}

fn raw_unsigned(
    encoder: &mut EntropyEncoder,
    contexts: &mut EntropyContexts,
    value: u64,
) -> Result<()> {
    encoder.encode_unsigned(value, &mut contexts.snapshot_bits)
}

fn read_raw_unsigned(
    decoder: &mut EntropyDecoder<'_>,
    contexts: &mut EntropyContexts,
) -> Result<u64> {
    decoder.decode_unsigned(&mut contexts.snapshot_bits)
}

fn encode_checkpoint_state(
    encoder: &mut EntropyEncoder,
    contexts: &mut EntropyContexts,
    snapshot: &SceneSnapshot,
) -> Result<()> {
    encoder.encode(
        &mut contexts.snapshot_flag,
        usize::from(snapshot.reset_predictors),
    )?;
    raw_unsigned(encoder, contexts, snapshot.start_sample)?;
    raw_unsigned(encoder, contexts, snapshot.object_count as u64)?;

    for &active in &snapshot.active {
        encoder.encode(&mut contexts.snapshot_flag, usize::from(active))?;
    }
    raw_unsigned(encoder, contexts, snapshot.trajectories.len() as u64)?;
    for knots in &snapshot.trajectories {
        raw_unsigned(encoder, contexts, knots.len() as u64)?;
        let mut previous_sample = 0i64;
        let mut previous_position = [0u16; 3];
        for knot in knots {
            enc_signed(
                encoder,
                &mut contexts.snapshot_trajectory,
                &mut contexts.tail,
                knot.sample as i64 - previous_sample,
            )?;
            previous_sample = knot.sample as i64;
            for axis in 0..3 {
                let code = quantize_position(axis, knot.position[axis]);
                let code_delta = i64::from(code) - i64::from(previous_position[axis]);
                enc_signed(
                    encoder,
                    &mut contexts.snapshot_position_delta,
                    &mut contexts.tail,
                    code_delta,
                )?;
                previous_position[axis] = code;
            }
        }
    }

    raw_unsigned(encoder, contexts, snapshot.power.ell_half_db.len() as u64)?;
    for bands in &snapshot.power.ell_half_db {
        for band in 0..PARAM_BAND_COUNT {
            enc_signed(
                encoder,
                &mut contexts.snapshot_ell,
                &mut contexts.tail,
                i64::from(bands[band]),
            )?;
        }
    }

    for channel in 0..CORE_CHANNEL_COUNT {
        for band in 0..PARAM_BAND_COUNT {
            enc_signed(
                encoder,
                &mut contexts.snapshot_error,
                &mut contexts.tail,
                i64::from(snapshot.error_log[channel][band]),
            )?;
            encoder.encode(
                &mut contexts.snapshot_flag,
                usize::from(snapshot.error_unobservable[channel][band]),
            )?;
        }
    }

    raw_unsigned(encoder, contexts, snapshot.correlation_pairs.len() as u64)?;
    for pair in &snapshot.correlation_pairs {
        raw_unsigned(encoder, contexts, u64::from(pair.a))?;
        raw_unsigned(encoder, contexts, u64::from(pair.b))?;
        for band in 0..PARAM_BAND_COUNT {
            match pair.rho[band] {
                Some(coefficient) => {
                    encoder.encode(&mut contexts.snapshot_flag, 1)?;
                    raw_unsigned(encoder, contexts, u64::from(coefficient.magnitude))?;
                    raw_unsigned(encoder, contexts, u64::from(coefficient.phase))?;
                }
                None => encoder.encode(&mut contexts.snapshot_flag, 0)?,
            }
        }
    }
    Ok(())
}

fn decode_checkpoint_state(
    decoder: &mut EntropyDecoder<'_>,
    contexts: &mut EntropyContexts,
) -> Result<SceneSnapshot> {
    let reset_predictors = decoder.decode(&mut contexts.snapshot_flag)? == 1;
    let start_sample = read_raw_unsigned(decoder, contexts)?;
    let snapshot_objects = read_raw_unsigned(decoder, contexts)? as usize;
    if snapshot_objects > MAX_OBJECTS {
        return Err(SpatialError::Parameter("snapshot object count too large"));
    }

    let mut active = Vec::with_capacity(snapshot_objects);
    for _ in 0..snapshot_objects {
        let value = decoder.decode(&mut contexts.snapshot_flag)?;
        if value > 1 {
            return Err(SpatialError::Parameter("invalid snapshot activity flag"));
        }
        active.push(value == 1);
    }

    let trajectory_count = read_raw_unsigned(decoder, contexts)? as usize;
    if trajectory_count > MAX_OBJECTS {
        return Err(SpatialError::Parameter(
            "snapshot trajectory count too large",
        ));
    }
    let mut trajectories = Vec::with_capacity(trajectory_count);
    for _ in 0..trajectory_count {
        let knot_count = read_raw_unsigned(decoder, contexts)? as usize;
        if knot_count > MAX_TRAJECTORY_KNOTS {
            return Err(SpatialError::Parameter("snapshot trajectory too long"));
        }
        let mut knots = Vec::with_capacity(knot_count);
        let mut previous_sample = 0i64;
        let mut previous_position = [0u16; 3];
        for _ in 0..knot_count {
            let delta = dec_signed(
                decoder,
                &mut contexts.snapshot_trajectory,
                &mut contexts.tail,
            )?;
            let sample = previous_sample
                .checked_add(delta)
                .ok_or(SpatialError::Parameter("snapshot trajectory time overflow"))?;
            if sample < 0 {
                return Err(SpatialError::Parameter("negative snapshot sample"));
            }
            previous_sample = sample;
            let mut position = [0.0f64; 3];
            for axis in 0..3 {
                let code_delta = dec_signed(
                    decoder,
                    &mut contexts.snapshot_position_delta,
                    &mut contexts.tail,
                )?;
                let code = (i64::from(previous_position[axis]) + code_delta)
                    .clamp(0, POSITION_STEPS as i64) as u16;
                previous_position[axis] = code;
                position[axis] = dequantize_position(axis, code);
            }
            knots.push(TrajectoryKnot {
                sample: sample as u64,
                position,
            });
        }
        trajectories.push(knots);
    }

    let ell_count = read_raw_unsigned(decoder, contexts)? as usize;
    if ell_count > MAX_OBJECTS {
        return Err(SpatialError::Parameter("snapshot power count too large"));
    }
    let mut ell_half_db = Vec::with_capacity(ell_count);
    for _ in 0..ell_count {
        let mut bands = [0i32; PARAM_BAND_COUNT];
        for band in &mut bands {
            *band = i32::try_from(dec_signed(
                decoder,
                &mut contexts.snapshot_ell,
                &mut contexts.tail,
            )?)
            .map_err(|_| SpatialError::Parameter("snapshot power overflow"))?;
        }
        ell_half_db.push(bands);
    }

    let mut error_log = [[0i32; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT];
    let mut error_unobservable = [[false; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT];
    for channel in 0..CORE_CHANNEL_COUNT {
        for band in 0..PARAM_BAND_COUNT {
            error_log[channel][band] = i32::try_from(dec_signed(
                decoder,
                &mut contexts.snapshot_error,
                &mut contexts.tail,
            )?)
            .map_err(|_| SpatialError::Parameter("snapshot error overflow"))?;
            let value = decoder.decode(&mut contexts.snapshot_flag)?;
            if value > 1 {
                return Err(SpatialError::Parameter("invalid snapshot error mask"));
            }
            error_unobservable[channel][band] = value == 1;
        }
    }

    let pair_count = read_raw_unsigned(decoder, contexts)? as usize;
    if pair_count > MAX_CORRELATION_PAIRS {
        return Err(SpatialError::Parameter("snapshot pair count too large"));
    }
    let mut correlation_pairs = Vec::with_capacity(pair_count);
    for _ in 0..pair_count {
        let a = read_raw_unsigned(decoder, contexts)? as u16;
        let b = read_raw_unsigned(decoder, contexts)? as u16;
        let mut rho = [None; PARAM_BAND_COUNT];
        for band in 0..PARAM_BAND_COUNT {
            let present = decoder.decode(&mut contexts.snapshot_flag)?;
            if present == 0 {
                continue;
            }
            let magnitude = read_raw_unsigned(decoder, contexts)? as u8;
            let phase = read_raw_unsigned(decoder, contexts)? as u8;
            if magnitude > crate::constants::CORRELATION_MAX_MAGNITUDE
                || phase >= crate::constants::CORRELATION_PHASE_STEPS as u8
            {
                return Err(SpatialError::Parameter("invalid snapshot correlation"));
            }
            rho[band] = Some(CorrelationCoefficient { magnitude, phase });
        }
        correlation_pairs.push(CorrelationPair { a, b, rho });
    }

    Ok(SceneSnapshot {
        start_sample,
        object_count: snapshot_objects,
        active: active.clone(),
        trajectories,
        power: ObjectPowerState {
            active,
            ell_half_db,
        },
        error_log,
        error_unobservable,
        correlation_pairs,
        reset_predictors,
    })
}

/// Serialize a checkpoint with independent, freshly reset entropy contexts.
pub fn encode_checkpoint(checkpoint: &SceneSnapshot) -> Result<Vec<u8>> {
    if checkpoint.object_count > MAX_OBJECTS {
        return Err(SpatialError::Parameter("checkpoint object count too large"));
    }
    let mut contexts = EntropyContexts::new(checkpoint.object_count);
    contexts.reset_tile();
    let mut encoder = EntropyEncoder::new();
    encoder.encode_unsigned(u64::from(CHECKPOINT_MAGIC), &mut contexts.snapshot_bits)?;
    encode_checkpoint_state(&mut encoder, &mut contexts, checkpoint)?;
    Ok(encoder.finish())
}

/// Parse a complete checkpoint. The record is rejected unless its rANS stream
/// ends exactly at the payload boundary.
pub fn decode_checkpoint(bytes: &[u8]) -> Result<SceneSnapshot> {
    let mut contexts = EntropyContexts::new(1);
    contexts.reset_tile();
    let mut decoder = EntropyDecoder::new(bytes)?;
    let magic = decoder.decode_unsigned(&mut contexts.snapshot_bits)?;
    if magic != u64::from(CHECKPOINT_MAGIC) {
        return Err(SpatialError::Syntax("bad checkpoint magic"));
    }
    let checkpoint = decode_checkpoint_state(&mut decoder, &mut contexts)?;
    decoder.finish()?;
    Ok(checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on_grid(value: f64) -> f64 {
        // Put a coordinate exactly on the 12-bit grid used by the codec.
        let code = quantize_position(0, value);
        dequantize_position(0, code)
    }

    fn on_z_grid(value: f64) -> f64 {
        let code = quantize_position(2, value);
        dequantize_position(2, code)
    }

    fn default_error() -> CoreErrorEnvelope {
        CoreErrorEnvelope::default()
    }

    fn make_tile(object_count: usize, start_sample: u64) -> ParameterTile {
        let mut base = [[0i32; PARAM_BAND_COUNT]; 4];
        for object in 0..4 {
            for band in 0..PARAM_BAND_COUNT {
                base[object][band] = ((object as i32) * 3 - band as i32 * 2) / 2;
            }
        }
        let mut anchors = Vec::new();
        for step in 0..3u64 {
            let mut ell = vec![[0i32; PARAM_BAND_COUNT]; object_count];
            let mut active = vec![false; object_count];
            for object in 0..object_count {
                active[object] = object % 3 != 2 || step == 1;
                for band in 0..PARAM_BAND_COUNT {
                    let base_value = if object < 4 { base[object][band] } else { 0 };
                    ell[object][band] = if active[object] {
                        base_value + ((band as i32 * (step as i32 + 1)) % 7) - 3
                    } else {
                        0
                    };
                }
            }
            let mut error = default_error();
            for channel in 0..CORE_CHANNEL_COUNT {
                for band in 0..PARAM_BAND_COUNT {
                    error.unobservable[channel][band] = (channel + band) % 17 == 0;
                    if !error.unobservable[channel][band] {
                        // Exactly on the 2 dB grid.
                        error.nu[channel][band] =
                            10f64.powf(2.0 * ((channel + band) % 8) as f64 / 10.0 - 0.8);
                    }
                }
            }
            anchors.push(ParameterAnchor {
                sample: start_sample + step * 960,
                active,
                ell_half_db: ell,
                error,
            });
        }

        let mut rho = [None; PARAM_BAND_COUNT];
        for band in 0..PARAM_BAND_COUNT {
            if band % 3 == 0 {
                rho[band] = Some(CorrelationCoefficient {
                    magnitude: ((band % 15) + 1) as u8,
                    phase: (band % 32) as u8,
                });
            }
        }
        let pairs = vec![
            CorrelationPair { a: 0, b: 1, rho },
            CorrelationPair {
                a: 2,
                b: 3,
                rho: [None; PARAM_BAND_COUNT],
            },
        ];

        let trajectories: Vec<Vec<TrajectoryKnot>> = (0..object_count)
            .map(|object| {
                if object % 2 == 0 {
                    vec![
                        TrajectoryKnot {
                            sample: start_sample,
                            position: [
                                on_grid(-0.5 + object as f64 * 0.1),
                                on_grid(0.5),
                                on_z_grid(0.25),
                            ],
                        },
                        TrajectoryKnot {
                            sample: start_sample + 2400,
                            position: [
                                on_grid(0.5),
                                on_grid(-0.5 + object as f64 * 0.05),
                                on_z_grid(0.75),
                            ],
                        },
                    ]
                } else {
                    Vec::new()
                }
            })
            .collect();

        ParameterTile {
            start_sample,
            frames: 4800,
            anchors,
            pairs,
            trajectories,
            correction: Vec::new(),
        }
    }

    #[test]
    fn correction_trailer_roundtrips_through_a_parameter_tile() {
        use crate::correction::{CorrectionComponent, TileCorrection};
        use num_complex::Complex;

        let mut tile = make_tile(4, 0);
        let mut correction = TileCorrection::default();
        correction.bands[2].components.push(CorrectionComponent {
            support: vec![0, 2],
            left: vec![Complex::new(1.0, 0.0), Complex::new(4.0 / 7.0, 0.0)],
            right: [Complex::new(2.0 / 7.0, 0.0); 7],
            left_scale: 0.5,
            right_scale: 1.0,
        });
        tile.correction = vec![correction.clone()];
        let mut encoder = SceneParameterEncoder::new(4);
        let bytes = encoder.encode_tile(&tile).unwrap();
        assert!(!decode_trailing_corrections(&bytes).unwrap().is_empty());
        let mut decoder = SceneParameterDecoder::new(4);
        let decoded = decoder
            .decode_tile(tile.start_sample, tile.frames, &bytes)
            .unwrap();
        assert_eq!(decoded.correction, vec![correction]);
    }

    #[test]
    fn dct_rows_are_orthonormal() {
        let dct = DctBasis::new();
        for r in 0..DCT_COEFF_COUNT {
            for s in 0..DCT_COEFF_COUNT {
                let dot: f64 = (0..PARAM_BAND_COUNT)
                    .map(|band| dct.basis[r][band] * dct.basis[s][band])
                    .sum();
                let expected = if r == s { 1.0 } else { 0.0 };
                assert!((dot - expected).abs() < 1.0e-12);
            }
        }
    }

    #[test]
    fn tile_roundtrips_through_encoder_and_decoder() {
        let tile = make_tile(4, 0);
        let mut encoder = SceneParameterEncoder::new(4);
        let bytes = encoder.encode_tile(&tile).unwrap();
        let mut decoder = SceneParameterDecoder::new(4);
        let decoded = decoder
            .decode_tile(tile.start_sample, tile.frames, &bytes)
            .unwrap();

        assert_eq!(decoded.start_sample, tile.start_sample);
        assert_eq!(decoded.frames, tile.frames);
        assert_eq!(decoded.anchors.len(), tile.anchors.len());
        for (original, reconstructed) in tile.anchors.iter().zip(&decoded.anchors) {
            assert_eq!(original.sample, reconstructed.sample);
            assert_eq!(original.active, reconstructed.active);
            assert_eq!(original.ell_half_db, reconstructed.ell_half_db);
            assert_eq!(
                original.error.unobservable,
                reconstructed.error.unobservable
            );
            for channel in 0..CORE_CHANNEL_COUNT {
                for band in 0..PARAM_BAND_COUNT {
                    if original.error.unobservable[channel][band] {
                        assert_eq!(reconstructed.error.nu[channel][band], 0.0);
                    } else if original.error.nu[channel][band] == 0.0 {
                        assert_eq!(reconstructed.error.nu[channel][band], 0.0);
                    } else {
                        let expected =
                            dequantize_error(quantize_error(original.error.nu[channel][band]));
                        assert_eq!(reconstructed.error.nu[channel][band], expected);
                    }
                }
            }
        }
        assert_eq!(decoded.pairs, tile.pairs);
        for (original, reconstructed) in tile.trajectories.iter().zip(&decoded.trajectories) {
            assert_eq!(original, reconstructed);
        }
    }

    #[test]
    fn checkpoint_roundtrips_and_restores_predictor_state() {
        let first = make_tile(4, 0);
        let second = make_tile(4, 4800);
        let mut encoder = SceneParameterEncoder::new(4);
        let first_bytes = encoder.encode_tile(&first).unwrap();
        let checkpoint = encoder.checkpoint(second.start_sample);
        encoder.reset_entropy();
        let second_bytes = encoder.encode_tile(&second).unwrap();

        let encoded = encode_checkpoint(&checkpoint).unwrap();
        let decoded_checkpoint = decode_checkpoint(&encoded).unwrap();
        assert_eq!(decoded_checkpoint, checkpoint);

        // Continuous decoding establishes the reference state.
        let mut continuous = SceneParameterDecoder::new(4);
        continuous
            .decode_tile(first.start_sample, first.frames, &first_bytes)
            .unwrap();
        continuous.reset_entropy();
        let expected = continuous
            .decode_tile(second.start_sample, second.frames, &second_bytes)
            .unwrap();

        // A fresh decoder installs only the independent checkpoint and still
        // decodes the following tile identically.
        let mut fresh = SceneParameterDecoder::new(4);
        fresh.install_checkpoint(&decoded_checkpoint).unwrap();
        let actual = fresh
            .decode_tile(second.start_sample, second.frames, &second_bytes)
            .unwrap();
        assert_eq!(actual.anchors, expected.anchors);
        assert_eq!(actual.pairs, expected.pairs);
        assert_eq!(actual.trajectories, expected.trajectories);
    }

    #[test]
    fn invalid_checkpoint_leaves_predictor_state_untouched() {
        let tile = make_tile(4, 0);
        let mut encoder = SceneParameterEncoder::new(4);
        let bytes = encoder.encode_tile(&tile).unwrap();
        let mut decoder = SceneParameterDecoder::new(4);
        let checkpoint = decoder.checkpoint(4800);
        let mut invalid = checkpoint.clone();
        invalid.trajectories.pop();
        assert!(decoder.install_checkpoint(&invalid).is_err());
        assert_eq!(decoder.checkpoint(4800), checkpoint);
        // The committed decoder still accepts the real tile.
        decoder
            .decode_tile(tile.start_sample, tile.frames, &bytes)
            .unwrap();
    }

    #[test]
    fn estimate_does_not_mutate_committed_state() {
        let tile = make_tile(4, 0);
        let mut encoder = SceneParameterEncoder::new(4);
        let first = encoder.estimate_tile_bits(&tile).unwrap();
        let second = encoder.estimate_tile_bits(&tile).unwrap();
        assert_eq!(first, second);
        // A subsequent real encode still works and decodes the same tile.
        let bytes = encoder.encode_tile(&tile).unwrap();
        let mut decoder = SceneParameterDecoder::new(4);
        let decoded = decoder.decode_tile(0, tile.frames, &bytes).unwrap();
        assert_eq!(decoded.anchors.len(), tile.anchors.len());
    }

    #[test]
    fn state_predicts_across_tiles() {
        let first = make_tile(4, 0);
        let second = make_tile(4, 4800);
        let mut encoder = SceneParameterEncoder::new(4);
        let mut decoder = SceneParameterDecoder::new(4);

        let first_bytes = encoder.encode_tile(&first).unwrap();
        let first_decoded = decoder
            .decode_tile(first.start_sample, first.frames, &first_bytes)
            .unwrap();
        let second_bytes = encoder.encode_tile(&second).unwrap();
        let second_decoded = decoder
            .decode_tile(second.start_sample, second.frames, &second_bytes)
            .unwrap();

        assert_eq!(
            first_decoded.anchors[0].ell_half_db,
            first.anchors[0].ell_half_db
        );
        assert_eq!(
            second_decoded.anchors[0].ell_half_db,
            second.anchors[0].ell_half_db
        );
        assert_eq!(second_decoded.pairs, second.pairs);
    }

    #[test]
    fn truncated_tile_is_rejected_without_state_change() {
        let tile = make_tile(4, 0);
        let mut encoder = SceneParameterEncoder::new(4);
        let bytes = encoder.encode_tile(&tile).unwrap();

        let mut decoder = SceneParameterDecoder::new(4);
        assert!(
            decoder
                .decode_tile(tile.start_sample, tile.frames, &bytes[..bytes.len() - 2])
                .is_err()
        );

        // The decoder must still accept the complete packet afterwards.
        let decoded = decoder
            .decode_tile(tile.start_sample, tile.frames, &bytes)
            .unwrap();
        assert_eq!(decoded.anchors.len(), tile.anchors.len());
    }

    #[test]
    fn from_anchor_produces_normalized_powers_and_zero_inactive() {
        let mut ell = vec![[0i32; PARAM_BAND_COUNT]; 3];
        for band in 0..PARAM_BAND_COUNT {
            ell[0][band] = 20;
            ell[1][band] = 10;
            ell[2][band] = 100;
        }
        let anchor = ParameterAnchor {
            sample: 0,
            active: vec![true, true, false],
            ell_half_db: ell,
            error: default_error(),
        };
        let parameters = SceneParameters::from_anchor(&anchor, &[]);
        assert_eq!(parameters.covariance.q.len(), 3);
        assert_eq!(parameters.covariance.q[2], 0.0);
        assert!(parameters.covariance.q[0] > parameters.covariance.q[1]);
        let sum: f64 = parameters.covariance.q.iter().sum();
        assert!((sum - 1.0).abs() < 1.0e-12);
    }
}
