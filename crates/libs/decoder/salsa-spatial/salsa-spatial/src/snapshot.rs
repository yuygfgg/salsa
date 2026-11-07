//! One-second independent parameter and scene snapshot.
//!
//! A snapshot is self-contained: it carries the complete scene description
//! (positions, trajectories and activity) together with every predictor that
//! the next tile needs (power base, fast coefficients, sparse corrections,
//! core-error envelope state, and correlation pairs). Decoding may resume from
//! a snapshot without any earlier parameter packets.

use crate::constants::{CORE_CHANNEL_COUNT, PARAM_BAND_COUNT};
use crate::params::{CorrelationPair, ObjectPowerState, PowerModel, TrajectoryKnot};

/// Complete state needed to start decoding after a snapshot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneSnapshot {
    pub start_sample: u64,
    pub object_count: usize,
    pub active: Vec<bool>,
    pub positions: Vec<[f64; 3]>,
    pub trajectories: Vec<Vec<TrajectoryKnot>>,
    pub power: ObjectPowerState,
    pub power_model: PowerModel,
    /// `[object][band]` sparse band corrections in half-dB units.
    pub correction_half_db: Vec<[i32; PARAM_BAND_COUNT]>,
    /// `[channel][band]` quantised core-error log envelope (2 dB grid).
    pub error_log: [[i32; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
    /// Core-error unobservable-band mask.
    pub error_unobservable: [[bool; PARAM_BAND_COUNT]; CORE_CHANNEL_COUNT],
    /// Correlation pairs predicted from the previous tile.
    pub correlation_pairs: Vec<CorrelationPair>,
    /// Entropy/predictor reset marker.
    pub reset_predictors: bool,
}

impl SceneSnapshot {
    /// The decoded position for `object`, falling back to the last trajectory
    /// knot when an explicit position list entry is unavailable.
    pub fn position(&self, object: usize) -> Option<[f64; 3]> {
        self.positions.get(object).copied().or_else(|| {
            self.trajectories
                .get(object)
                .and_then(|knots| knots.last())
                .map(|knot| knot.position)
        })
    }
}
