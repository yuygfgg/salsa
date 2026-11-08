//! Independent state checkpoint for one tile boundary.
//!
//! A checkpoint is self-contained: it carries the committed integer predictors
//! (activity, power base, fast coefficients, sparse corrections, core-error
//! envelope state, and correlation pairs) together with the trajectory state
//! needed by the next tile. It is coded in its own record with fresh entropy
//! contexts, so parsing it never depends on an earlier tile or on the current
//! tile's parameter payload.

use crate::constants::{CORE_CHANNEL_COUNT, PARAM_BAND_COUNT};
use crate::params::{CorrelationPair, ObjectPowerState, PowerModel, TrajectoryKnot};

/// Complete state needed to start decoding at a tile boundary.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneSnapshot {
    pub start_sample: u64,
    pub object_count: usize,
    pub active: Vec<bool>,
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
    /// Position at the end of the retained trajectory for `object`.
    pub fn position(&self, object: usize) -> Option<[f64; 3]> {
        self.trajectories
            .get(object)
            .and_then(|knots| knots.last())
            .map(|knot| knot.position)
    }
}
