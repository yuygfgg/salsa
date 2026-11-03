//! Shared interpretation of resolved states and authored transition targets.
//!
//! Tracks keep absolute sample times. Buffer sizes, display refresh rates, and
//! renderer control grids do not define the authored trajectory.

use crate::{
    IrError, ObjectEvent, ObjectState, ProgramCalibration, Snapshot, SpatialPayload, Transition,
};

/// Evaluated states precede transition targets at the same sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum KeyKind {
    State,
    Target,
}

#[derive(Debug, Clone)]
struct Key {
    sample: i64,
    kind: KeyKind,
    origin: ObjectState,
    target: ObjectState,
    transition: Transition,
}

impl Key {
    fn at(&self, sample: i64) -> ObjectState {
        let duration = self.transition.samples();
        let amount = if duration == 0 {
            1.0
        } else {
            ((sample - self.sample) as f64 / f64::from(duration)).clamp(0.0, 1.0)
        };
        interpolate(self.origin, self.target, amount, self.transition)
    }
}

/// A resolved object track. Insertions at the same time replace the same kind
/// of update. A state snapshot precedes a target at that time. Replayed buffer
/// prefixes are therefore idempotent, including after a region copy.
#[derive(Debug, Clone)]
pub struct StateTrack {
    initial: ObjectState,
    keys: Vec<Key>,
    retained_from: Option<(i64, KeyKind)>,
}

impl StateTrack {
    pub fn new(initial: ObjectState) -> Self {
        Self {
            initial,
            keys: Vec::new(),
            retained_from: None,
        }
    }

    pub fn first_sample(&self) -> Option<i64> {
        self.keys.first().map(|key| key.sample)
    }

    pub fn state_at(&self, sample: i64) -> ObjectState {
        let end = self.keys.partition_point(|key| key.sample <= sample);
        end.checked_sub(1)
            .map_or(self.initial, |index| self.keys[index].at(sample))
    }

    pub fn set_state(&mut self, sample: i64, state: ObjectState) {
        self.insert(sample, state, Transition::Step, KeyKind::State);
    }

    pub fn set_target(&mut self, sample: i64, target: ObjectState, transition: Transition) {
        if transition == Transition::Step {
            self.set_state(sample, target);
            return;
        }
        self.insert(sample, target, transition, KeyKind::Target);
    }

    fn insert(&mut self, sample: i64, target: ObjectState, transition: Transition, kind: KeyKind) {
        debug_assert_eq!(self.initial.id, target.id);
        let order = (sample, kind);
        // A sliced buffer can replay a causal prefix after pruning. The
        // retained key already has its origin; older updates cannot change it.
        if self.retained_from.is_some_and(|first| order < first) {
            return;
        }
        let index = self
            .keys
            .partition_point(|key| (key.sample, key.kind) < order);
        let key = Key {
            sample,
            kind,
            origin: self.initial,
            target,
            transition,
        };
        if self
            .keys
            .get(index)
            .is_some_and(|key| (key.sample, key.kind) == order)
        {
            self.keys[index] = key;
        } else {
            self.keys.insert(index, key);
        }
        for index in index..self.keys.len() {
            self.keys[index].origin = if index == 0 {
                self.initial
            } else {
                self.keys[index - 1].at(self.keys[index].sample)
            };
        }
    }

    /// Keep the last key before the cutoff, including its original ramp origin.
    pub fn discard_before(&mut self, sample: i64) {
        let count = self
            .keys
            .partition_point(|key| key.sample < sample)
            .saturating_sub(1);
        if count > 0 {
            self.initial = self.keys[count].origin;
            self.retained_from = Some((self.keys[count].sample, self.keys[count].kind));
            self.keys.drain(..count);
        }
    }

    /// Export a resolved state plus the remaining transitions in a buffer.
    /// A ramp that crosses the left edge resumes from that edge's actual state.
    pub fn events(&self, start: i64, frames: usize) -> Vec<ObjectEvent> {
        let end = start + frames as i64;
        let mut events = Vec::new();
        let first = self.keys.partition_point(|key| key.sample <= start);
        if let Some(key) = first.checked_sub(1).map(|index| &self.keys[index]) {
            let remaining = key.sample + i64::from(key.transition.samples()) - start;
            if remaining > 0 {
                let transition = match key.transition {
                    Transition::Step => Transition::Step,
                    Transition::Position(_) => Transition::Position(remaining as u32),
                    Transition::All(_) => Transition::All(remaining as u32),
                };
                events.push(ObjectEvent {
                    offset: 0,
                    transition,
                    state: key.target,
                });
            }
        }
        for key in &self.keys[first..] {
            if key.sample >= end {
                break;
            }
            events.push(ObjectEvent {
                offset: key.sample - start,
                transition: key.transition,
                state: key.target,
            });
        }
        events
    }
}

fn interpolate(
    origin: ObjectState,
    target: ObjectState,
    amount: f64,
    transition: Transition,
) -> ObjectState {
    // Complete destructuring makes each added IR property an explicit decision.
    let ObjectState {
        id,
        position,
        gain,
        enabled,
        size,
        spread,
    } = target;
    let lerp = |a: f32, b: f32| (f64::from(a) + f64::from(b - a) * amount) as f32;
    let optional = |a: Option<f32>, b: Option<f32>| match (a, b) {
        (Some(a), Some(b)) => Some(lerp(a, b)),
        (_, b) => b,
    };
    match transition {
        Transition::Step => target,
        Transition::Position(_) => ObjectState {
            id,
            position: std::array::from_fn(|axis| lerp(origin.position[axis], position[axis])),
            gain,
            enabled,
            size,
            spread,
        },
        Transition::All(_) => ObjectState {
            id,
            position: std::array::from_fn(|axis| lerp(origin.position[axis], position[axis])),
            gain: lerp(origin.gain, gain),
            enabled,
            size: optional(origin.size, size),
            spread: optional(origin.spread, spread),
        },
    }
}

/// A validated, format-independent scene in channel order. Channel zero is
/// LFE; track `i` belongs to PCM channel `i + 1` and object id `i + 1`.
#[derive(Debug, Clone)]
pub struct SceneTimeline {
    sample_rate: u32,
    tracks: Vec<StateTrack>,
    /// Programme calibration changes. A payload without a calibration element
    /// leaves the most recent value in force.
    calibrations: Vec<(i64, ProgramCalibration)>,
}

impl SceneTimeline {
    pub fn from_payload(payload: &SpatialPayload, buffer_start: i64) -> Result<Self, IrError> {
        payload.validate()?;
        let mut scene = Self {
            sample_rate: payload.sample_rate,
            // Layout information is available before the first update, but
            // audio is silent until that update takes effect. A first target
            // can ramp amplitude from silence without inventing a position.
            tracks: payload
                .objects
                .iter()
                .map(|object| {
                    StateTrack::new(ObjectState {
                        gain: 0.0,
                        enabled: false,
                        ..*object
                    })
                })
                .collect(),
            calibrations: Vec::new(),
        };
        scene.submit(payload, buffer_start)?;
        // The reference seeds the initial controls from the leading frames, so
        // the first trim element is in force before its own event arrives.
        if let Some(calibration) = &payload.calibration {
            scene.set_calibration(buffer_start, calibration.clone());
        }
        Ok(scene)
    }

    pub fn submit(&mut self, payload: &SpatialPayload, buffer_start: i64) -> Result<(), IrError> {
        payload.validate_layout(self.sample_rate, self.tracks.len() + 1)?;
        let sample = buffer_start + payload.snapshot_offset;
        for (track, &object) in self.tracks.iter_mut().zip(&payload.objects) {
            match payload.snapshot {
                Snapshot::State => track.set_state(sample, object),
                Snapshot::Continue => {}
                Snapshot::Target(transition) => track.set_target(sample, object, transition),
            }
        }
        for event in &payload.events {
            self.tracks[event.state.id as usize - 1].set_target(
                buffer_start + event.offset,
                event.state,
                event.transition,
            );
        }
        if let Some(calibration) = &payload.calibration {
            self.set_calibration(sample, calibration.clone());
        }
        Ok(())
    }

    fn set_calibration(&mut self, sample: i64, calibration: ProgramCalibration) {
        let index = self
            .calibrations
            .partition_point(|(existing, _)| *existing < sample);
        if self
            .calibrations
            .get(index)
            .is_some_and(|(existing, _)| *existing == sample)
        {
            self.calibrations[index] = (sample, calibration);
        } else {
            self.calibrations.insert(index, (sample, calibration));
        }
    }

    /// Programme calibration in force at `sample`, or `None` before the first
    /// payload that carried one.
    pub fn calibration_at(&self, sample: i64) -> Option<&ProgramCalibration> {
        let end = self
            .calibrations
            .partition_point(|(existing, _)| *existing <= sample);
        end.checked_sub(1).map(|index| &self.calibrations[index].1)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn object_count(&self) -> usize {
        self.tracks.len()
    }

    pub fn tracks(&self) -> &[StateTrack] {
        &self.tracks
    }

    pub fn state_at(&self, index: usize, sample: i64) -> ObjectState {
        self.tracks[index].state_at(sample)
    }

    pub fn states_at(&self, sample: i64) -> Vec<ObjectState> {
        self.tracks
            .iter()
            .map(|track| track.state_at(sample))
            .collect()
    }

    pub fn discard_before(&mut self, sample: i64) {
        for track in &mut self.tracks {
            track.discard_before(sample);
        }
        // Keep the last calibration at or before the cutoff, plus any future
        // changes; calibration is programme-wide, not per-object history.
        let keep_from = self
            .calibrations
            .partition_point(|(existing, _)| *existing <= sample)
            .saturating_sub(1);
        if keep_from > 0 {
            self.calibrations.drain(..keep_from);
        }
    }

    pub fn payload(&self, start: i64, frames: usize) -> SpatialPayload {
        let mut events: Vec<_> = self
            .tracks
            .iter()
            .flat_map(|track| track.events(start, frames))
            .collect();
        events.sort_by_key(|event| event.offset);
        let payload = SpatialPayload::new(self.sample_rate, self.states_at(start), events);
        match self.calibration_at(start) {
            Some(calibration) => payload.with_calibration(calibration.clone()),
            None => payload,
        }
    }
}

/// Align a renderer control time; ties go to the earlier block.
pub fn align_sample(sample: i64, grid: u32) -> i64 {
    assert!(grid > 0, "control grid must be nonzero");
    let grid = i64::from(grid);
    (sample + (grid - 1) / 2).div_euclid(grid) * grid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_then_interrupts_from_the_current_position() {
        let mut track = StateTrack::new(ObjectState::new(1, [-1.0, 0.0, 0.0]));
        track.set_target(
            32,
            ObjectState::new(1, [1.0, 0.0, 0.0]),
            Transition::Position(128),
        );
        track.set_target(
            96,
            ObjectState::new(1, [-1.0, 0.0, 0.0]),
            Transition::Position(128),
        );
        assert_eq!(track.state_at(16).position[0], -1.0);
        assert!((track.state_at(128).position[0] + 0.25).abs() < 1e-5);
    }

    #[test]
    fn slicing_keeps_ramp_timing_and_replay_is_idempotent() {
        let payload = SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [-1.0, 0.0, 0.0])],
            vec![ObjectEvent {
                offset: 32,
                transition: Transition::Position(128),
                state: ObjectState::new(1, [1.0, 0.0, 0.0]),
            }],
        );
        let mut scene = SceneTimeline::from_payload(&payload, 0).unwrap();
        let suffix = payload.slice(64, 128).unwrap();
        scene.submit(&suffix, 64).unwrap();
        assert!((scene.state_at(0, 96).position[0]).abs() < 1e-5);

        // Exporting a future evaluated state must preserve the origin of a
        // target at the same time, just as a state snapshot does at the edge.
        let update = SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [0.0; 3])],
            vec![ObjectEvent {
                offset: 200,
                transition: Transition::Position(32),
                state: ObjectState::new(1, [1.0, 0.0, 0.0]),
            }],
        )
        .with_snapshot_offset(200);
        scene.submit(&update, 0).unwrap();
        let exported = scene.payload(128, 128);
        let replay = SceneTimeline::from_payload(&exported, 128).unwrap();
        assert!((replay.state_at(0, 216).position[0] - 0.5).abs() < 1e-5);
        assert_eq!(scene.state_at(0, 160).position[0], 1.0);
        scene.discard_before(80);
        scene.submit(&suffix, 64).unwrap();
        assert!((scene.state_at(0, 96).position[0]).abs() < 1e-5);
    }
    #[test]
    fn calibration_stays_in_force_until_a_new_element_arrives() {
        let calibration = ProgramCalibration {
            warp_mode: 1,
            ..ProgramCalibration::default()
        };
        let initial = SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [0.0, 0.5, 0.0])],
            Vec::new(),
        )
        .with_calibration(calibration.clone());
        let mut scene = SceneTimeline::from_payload(&initial, 0).unwrap();
        assert_eq!(scene.calibration_at(0), Some(&calibration));
        assert_eq!(scene.calibration_at(-1), None);

        let quiet = SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [0.0, 0.5, 0.0])],
            Vec::new(),
        );
        scene.submit(&quiet, 100).unwrap();
        assert_eq!(scene.calibration_at(100), Some(&calibration));

        let mut updated = calibration.clone();
        updated.warp_mode = 2;
        let change = SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [0.0, 0.5, 0.0])],
            Vec::new(),
        )
        .with_calibration(updated.clone());
        scene.submit(&change, 200).unwrap();
        assert_eq!(scene.calibration_at(199), Some(&calibration));
        assert_eq!(scene.calibration_at(200), Some(&updated));

        let exported = scene.payload(150, 64);
        assert_eq!(exported.calibration, Some(calibration.clone()));
        scene.discard_before(250);
        assert_eq!(scene.calibration_at(250), Some(&updated));
    }
}
