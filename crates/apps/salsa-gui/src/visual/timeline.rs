//! Visual sampling and trail history over the shared scene timeline.
//!
//! The probe publishes keyframes, not per-display-frame snapshots. The UI
//! samples this timeline at the pipeline position so motion stays aligned with
//! the audible stream even when the display refresh rate differs from the
//! decoder rate.
//!
//! Motion trails are committed to an append-only history as the playhead
//! advances. They are never rebuilt from the keyframes afterwards: a point the
//! object has already passed must keep the position it had at that instant.

use gstreamer as gst;
use objectaudio_ir::{ObjectState, SceneTimeline, SpatialPayload, StateTrack};

/// Seconds of keyframe history kept per object (also the maximum trail depth).
pub const HISTORY_SECONDS: u64 = 12;
/// Hard cap on keyframes retained per object.
pub const MAX_KEYFRAMES_PER_OBJECT: usize = 256;
/// Maximum trail points stored per object.
pub const MAX_TRAIL_POINTS: usize = 24;
/// Default trail length in seconds.
pub const DEFAULT_TRAIL_SECONDS: f32 = 1.5;
/// A buffer gap longer than this marks the snapshot stale.
pub const STALE_AFTER_NS: u64 = 300_000_000;

const LEVEL_ATTACK_S: f32 = 0.04;
const LEVEL_RELEASE_S: f32 = 0.30;
const TRIM_EVERY: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualState {
    Live,
    Stale,
    Seeking,
    Eos,
}

#[derive(Debug, Clone)]
pub struct ObjectVisual {
    pub id: u32,
    pub position: [f32; 3],
    pub gain: f32,
    pub enabled: bool,
    /// Normalized 0..1 level derived from the decoder PCM (-60 dBFS .. 0 dBFS).
    pub level: f32,
    /// Recent positions, oldest first.
    pub trail: Vec<[f32; 3]>,
}

#[derive(Debug, Clone)]
pub struct VisualSnapshot {
    pub time: gst::ClockTime,
    pub objects: Vec<ObjectVisual>,
    /// Normalized 0..1 level derived from decoded LFE PCM (-60 dBFS .. 0 dBFS).
    pub lfe_level: f32,
    pub state: VisualState,
}

/// One committed point of an object's motion history.
#[derive(Debug, Clone, Copy)]
struct TrailPoint {
    time_ns: u64,
    position: [f32; 3],
}

#[derive(Debug)]
pub(crate) enum TapEvent {
    Caps {
        epoch: u64,
        channels: u32,
        rate: u32,
    },
    Buffer {
        epoch: u64,
        pts_ns: u64,
        payloads: Vec<SpatialPayload>,
        /// RMS levels in decoder channel order: LFE, then objects.
        levels: Vec<f32>,
        discont: bool,
    },
    Flush {
        epoch: u64,
        stop: bool,
    },
    Segment {
        epoch: u64,
    },
    Eos {
        epoch: u64,
    },
}

impl TapEvent {
    pub(crate) fn epoch(&self) -> u64 {
        match self {
            Self::Caps { epoch, .. }
            | Self::Buffer { epoch, .. }
            | Self::Flush { epoch, .. }
            | Self::Segment { epoch }
            | Self::Eos { epoch } => *epoch,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Timeline {
    epoch: u64,
    sample_rate: u32,
    channels: u32,
    object_ids: Vec<u32>,
    scene: Option<SceneTimeline>,
    /// Append-only trail history per object, oldest first.
    trails: Vec<Vec<TrailPoint>>,
    /// Grid spacing currently used by `trails`; zero until the first trail.
    trail_step_ns: u64,
    /// Next grid instant to commit to `trails`.
    trail_next_ns: Option<u64>,
    // Smoothed RMS levels in decoder channel order: LFE, then objects.
    levels: Vec<f32>,
    level_time_ns: Option<u64>,
    last_pts_ns: Option<u64>,
    last_meta_ns: Option<u64>,
    state: VisualState,
    frozen: bool,
    frames_since_trim: u32,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            epoch: 0,
            sample_rate: 48_000,
            channels: 0,
            object_ids: Vec::new(),
            scene: None,
            trails: Vec::new(),
            trail_step_ns: 0,
            trail_next_ns: None,
            levels: Vec::new(),
            level_time_ns: None,
            last_pts_ns: None,
            last_meta_ns: None,
            state: VisualState::Seeking,
            frozen: false,
            frames_since_trim: 0,
        }
    }
}

impl Timeline {
    pub(crate) fn reset(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.object_ids.clear();
        self.scene = None;
        self.clear_trails();
        self.levels.clear();
        self.level_time_ns = None;
        self.last_pts_ns = None;
        self.last_meta_ns = None;
        self.state = VisualState::Seeking;
        self.frozen = false;
    }

    fn clear_trails(&mut self) {
        self.trails.clear();
        self.trail_step_ns = 0;
        self.trail_next_ns = None;
    }

    pub(crate) fn mark_stale(&mut self) {
        if self.state != VisualState::Eos {
            self.state = VisualState::Stale;
        }
    }

    pub(crate) fn handle(&mut self, event: TapEvent) {
        let epoch = event.epoch();
        if epoch < self.epoch {
            return;
        }
        if epoch > self.epoch {
            self.reset(epoch);
        }
        match event {
            TapEvent::Caps {
                epoch: _,
                channels,
                rate,
            } => self.set_caps(channels, rate),
            TapEvent::Buffer {
                epoch: _,
                pts_ns,
                payloads,
                levels,
                discont,
            } => self.push_buffer(pts_ns, payloads, levels, discont),
            TapEvent::Flush { epoch: _, stop } => {
                self.reset(self.epoch);
                if stop {
                    self.state = VisualState::Seeking;
                }
            }
            TapEvent::Segment { epoch: _ } => {
                self.reset(self.epoch);
            }
            TapEvent::Eos { epoch: _ } => {
                self.state = VisualState::Eos;
                self.frozen = true;
            }
        }
    }

    fn set_caps(&mut self, channels: u32, rate: u32) {
        if channels != self.channels || rate != self.sample_rate {
            self.channels = channels;
            self.sample_rate = rate.max(1);
            self.clear_timeline();
        }
    }

    fn clear_timeline(&mut self) {
        self.object_ids.clear();
        self.scene = None;
        self.clear_trails();
        self.levels.clear();
        self.level_time_ns = None;
        self.last_pts_ns = None;
        self.state = VisualState::Seeking;
    }

    fn push_buffer(
        &mut self,
        pts_ns: u64,
        payloads: Vec<SpatialPayload>,
        levels: Vec<f32>,
        discont: bool,
    ) {
        if discont
            || self
                .last_pts_ns
                .is_some_and(|last| pts_ns.saturating_add(1_000_000) < last)
        {
            self.clear_timeline();
        }

        if !payloads.is_empty() {
            for payload in payloads {
                if let Err(error) = payload.validate() {
                    gst::warning!(gst::CAT_RUST, "invalid visual metadata: {error}");
                    continue;
                }
                if payload.sample_rate != self.sample_rate {
                    self.clear_timeline();
                    self.sample_rate = payload.sample_rate;
                }
                self.ensure_objects(&payload.objects);
                self.apply_payload(pts_ns, &payload);
            }
            self.state = VisualState::Live;
            self.last_meta_ns = Some(pts_ns);
        } else {
            if self.frozen {
                self.state = VisualState::Eos;
            } else if self
                .last_meta_ns
                .is_none_or(|last| pts_ns.saturating_sub(last) > STALE_AFTER_NS)
            {
                self.state = VisualState::Stale;
            }
        }
        self.update_levels(&levels, pts_ns);
        self.last_pts_ns = Some(pts_ns);
        self.frames_since_trim += 1;
        if self.frames_since_trim >= TRIM_EVERY {
            self.frames_since_trim = 0;
            self.trim(pts_ns);
        }
    }

    fn ensure_objects(&mut self, objects: &[ObjectState]) {
        let same = self.object_ids.len() == objects.len()
            && self
                .object_ids
                .iter()
                .zip(objects)
                .all(|(candidate, object)| *candidate == object.id);
        if same {
            return;
        }
        self.object_ids = objects.iter().map(|object| object.id).collect();
        self.scene = None;
        self.clear_trails();
        self.levels.clear();
        self.level_time_ns = None;
    }

    fn update_levels(&mut self, raw: &[f32], pts_ns: u64) {
        self.levels.resize(raw.len(), 0.0);
        if raw.is_empty() {
            self.level_time_ns = None;
            return;
        }
        let first = self.level_time_ns.is_none();
        let dt = self
            .level_time_ns
            .map(|last| (pts_ns.saturating_sub(last) as f32 / 1e9).clamp(0.001, 0.1))
            .unwrap_or(0.025);
        let reset = self
            .level_time_ns
            .is_some_and(|last| pts_ns.saturating_sub(last) > 500_000_000);
        for (level, raw) in self.levels.iter_mut().zip(raw) {
            let target = raw.max(0.0);
            if first || reset {
                *level = target;
                continue;
            }
            let previous = *level;
            let tau = if target > previous {
                LEVEL_ATTACK_S
            } else {
                LEVEL_RELEASE_S
            };
            let alpha = 1.0 - (-dt / tau).exp();
            *level = previous + alpha * (target - previous);
        }
        self.level_time_ns = Some(pts_ns);
    }

    fn tracks(&self) -> &[StateTrack] {
        self.scene
            .as_ref()
            .map(SceneTimeline::tracks)
            .unwrap_or(&[])
    }

    fn apply_payload(&mut self, pts_ns: u64, payload: &SpatialPayload) {
        let start = ns_to_sample(pts_ns, self.sample_rate);
        let result = match &mut self.scene {
            Some(scene) => scene.submit(payload, start),
            None => {
                SceneTimeline::from_payload(payload, start).map(|scene| self.scene = Some(scene))
            }
        };
        if let Err(error) = result {
            gst::warning!(gst::CAT_RUST, "invalid visual metadata: {error}");
        }
    }

    fn trim(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(HISTORY_SECONDS * 1_000_000_000);
        if let Some(scene) = &mut self.scene {
            scene.discard_before(ns_to_sample(cutoff, self.sample_rate));
        }
    }

    /// Append fixed-grid trajectory samples up to `time_ns`.
    ///
    /// Samples are stored once and never recomputed. Sampling on an absolute
    /// grid means the same instant always maps to the same vertex, so moving
    /// the playhead only slides the window; it cannot re-pick or re-shape the
    /// part of the trail the object has already travelled.
    fn commit_trails(&mut self, time_ns: u64, trail_ns: u64) {
        let step_ns = trail_step_ns(trail_ns);
        if self.trail_step_ns != step_ns {
            self.trails.clear();
            self.trail_next_ns = None;
            self.trail_step_ns = step_ns;
        }
        if self.trails.len() != self.tracks().len() {
            self.trails = vec![Vec::new(); self.tracks().len()];
            self.trail_next_ns = None;
        }

        let newest = (time_ns / step_ns) * step_ns;
        let oldest = time_ns.saturating_sub(trail_ns);
        let Some(first) = self.trail_next_ns else {
            // First snapshot: backfill the visible window from the timeline.
            self.backfill_trails(newest, oldest, step_ns);
            return;
        };
        if newest < first {
            // The playhead moved back a little (position query jitter). Keep
            // the committed history; only hide the points that are ahead.
            self.trim_trails(oldest);
            return;
        }
        let start = if newest.saturating_sub(first) > trail_ns {
            // A jump larger than the history window: rebuild it instead of
            // walking every intervening grid step.
            for trail in &mut self.trails {
                trail.clear();
            }
            (oldest / step_ns) * step_ns
        } else {
            first
        };

        let mut times = Vec::new();
        let mut time = start;
        while time <= newest {
            times.push(time);
            match time.checked_add(step_ns) {
                Some(next) => time = next,
                None => break,
            }
        }
        if !times.is_empty() {
            self.push_trail_times(&times);
            self.trail_next_ns = newest.checked_add(step_ns);
        }
        self.trim_trails(oldest);
    }

    fn backfill_trails(&mut self, newest: u64, oldest: u64, step_ns: u64) {
        let start = (oldest / step_ns) * step_ns;
        let mut times = Vec::with_capacity(MAX_TRAIL_POINTS + 1);
        let mut time = start;
        while time <= newest {
            times.push(time);
            match time.checked_add(step_ns) {
                Some(next) => time = next,
                None => break,
            }
        }
        self.push_trail_times(&times);
        self.trail_next_ns = newest.checked_add(step_ns);
        self.trim_trails(oldest);
    }

    fn push_trail_times(&mut self, times: &[u64]) {
        let tracks = self
            .scene
            .as_ref()
            .map(SceneTimeline::tracks)
            .unwrap_or(&[]);
        let trails = &mut self.trails;
        for &time_ns in times {
            for (track, trail) in tracks.iter().zip(trails.iter_mut()) {
                let state = sample_track(track, time_ns, self.sample_rate);
                trail.push(TrailPoint {
                    time_ns,
                    position: state.position,
                });
            }
        }
    }

    fn trim_trails(&mut self, oldest: u64) {
        for trail in &mut self.trails {
            let drop = trail.partition_point(|point| point.time_ns < oldest);
            if drop > 0 {
                trail.drain(0..drop);
            }
            let max = MAX_TRAIL_POINTS + 1;
            if trail.len() > max {
                let remove = trail.len() - max;
                trail.drain(0..remove);
            }
        }
    }

    fn trail_for(&self, index: usize, time_ns: u64, trail_ns: u64) -> Vec<[f32; 3]> {
        let mut points = Vec::new();
        if let Some(trail) = self.trails.get(index) {
            let oldest = time_ns.saturating_sub(trail_ns);
            let start = trail.partition_point(|point| point.time_ns < oldest);
            let end = trail.partition_point(|point| point.time_ns <= time_ns);
            points.extend(trail[start..end].iter().map(|point| point.position));
        }
        // Connect the committed history to the current interpolated position.
        // This is the only vertex that moves between frames.
        let state = sample_track(&self.tracks()[index], time_ns, self.sample_rate);
        if points.last().is_none_or(|last| *last != state.position) {
            points.push(state.position);
        }
        points
    }

    pub(crate) fn snapshot_at(
        &mut self,
        time_ns: u64,
        trail_seconds: f32,
    ) -> Option<VisualSnapshot> {
        let first_ns = self
            .tracks()
            .iter()
            .filter_map(StateTrack::first_sample)
            .min()
            .map(|sample| {
                (sample.max(0) as u128 * 1_000_000_000 / u128::from(self.sample_rate)) as u64
            })
            .or(self.level_time_ns)?;

        let stale = self.state == VisualState::Stale
            || self.last_pts_ns.is_some_and(|last| {
                time_ns.saturating_sub(last) > STALE_AFTER_NS && self.state != VisualState::Eos
            });

        let trail_ns = if trail_seconds.is_finite() {
            (trail_seconds.max(0.0) * 1e9) as u64
        } else {
            0
        };
        let trails_enabled = trail_ns > 0;
        if trails_enabled {
            self.commit_trails(time_ns, trail_ns);
        }

        let mut objects = Vec::with_capacity(self.tracks().len());
        for (index, track) in self.tracks().iter().enumerate() {
            let state = sample_track(track, time_ns, self.sample_rate);
            let level = self
                .levels
                .get(index + 1)
                .copied()
                .map(normalized_level)
                .unwrap_or(0.0);
            let trail = if trails_enabled {
                self.trail_for(index, time_ns, trail_ns)
            } else {
                Vec::new()
            };
            objects.push(ObjectVisual {
                id: state.id,
                position: state.position,
                gain: state.gain,
                enabled: state.enabled,
                level,
                trail,
            });
        }

        let state = if self.state == VisualState::Eos {
            VisualState::Eos
        } else if stale {
            VisualState::Stale
        } else if time_ns.saturating_add(50_000_000) < first_ns {
            VisualState::Seeking
        } else {
            VisualState::Live
        };

        Some(VisualSnapshot {
            time: gst::ClockTime::from_nseconds(time_ns),
            objects,
            lfe_level: self
                .levels
                .first()
                .copied()
                .map(normalized_level)
                .unwrap_or(0.0),
            state,
        })
    }
}

fn ns_to_sample(ns: u64, rate: u32) -> i64 {
    (u128::from(ns) * u128::from(rate) / 1_000_000_000) as i64
}

fn normalized_level(rms: f32) -> f32 {
    if !rms.is_finite() || rms <= 1e-7 {
        0.0
    } else {
        ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    }
}

fn sample_track(track: &StateTrack, time_ns: u64, rate: u32) -> ObjectState {
    track.state_at(ns_to_sample(time_ns, rate))
}

fn trail_step_ns(trail_ns: u64) -> u64 {
    let intervals = (MAX_TRAIL_POINTS.saturating_sub(1)).max(1) as u64;
    (trail_ns / intervals).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objectaudio_ir::ObjectEvent;

    fn object(id: u32, position: [f32; 3]) -> ObjectState {
        ObjectState::new(id, position)
    }

    fn payload(objects: Vec<ObjectState>, events: Vec<ObjectEvent>) -> SpatialPayload {
        SpatialPayload::new(48_000, objects, events)
    }

    fn buffer_event(pts_ns: u64, offset: usize, payload: SpatialPayload) -> TapEvent {
        let levels = vec![0.5; payload.objects.len() + 1];
        TapEvent::Buffer {
            epoch: 0,
            pts_ns,
            payloads: vec![payload.with_snapshot_offset(offset as i64)],
            levels,
            discont: false,
        }
    }

    #[test]
    fn snapshot_offset_is_applied_to_the_timeline() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 3,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            48_000,
            payload(
                vec![object(1, [0.0, 0.0, 0.0]), object(2, [1.0, 0.0, 0.0])],
                vec![],
            ),
        ));
        let snapshot = timeline.snapshot_at(1_000_000_000, 0.0).expect("snapshot");
        assert_eq!(snapshot.objects.len(), 2);
        assert_eq!(snapshot.objects[0].position, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn state_snapshots_do_not_start_moving_early() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![]),
        ));
        timeline.handle(buffer_event(
            1_000_000_000,
            0,
            payload(vec![object(1, [1.0, 0.0, 0.0])], vec![]),
        ));
        let snapshot = timeline.snapshot_at(500_000_000, 0.0).expect("snapshot");
        assert!(snapshot.objects[0].position[0].abs() < 1e-4);
    }

    #[test]
    fn ramps_hold_until_the_event_time() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![]),
        ));
        let event = ObjectEvent {
            offset: 4_800,
            transition: objectaudio_ir::Transition::All(4_800),
            state: object(1, [1.0, 0.0, 0.0]),
        };
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![event]),
        ));
        let before = timeline.snapshot_at(50_000_000, 0.0).expect("snapshot");
        assert!(before.objects[0].position[0].abs() < 1e-4);
        let middle = timeline.snapshot_at(150_000_000, 0.0).expect("snapshot");
        assert!((middle.objects[0].position[0] - 0.5).abs() < 1e-4);
        let after = timeline.snapshot_at(250_000_000, 0.0).expect("snapshot");
        assert!((after.objects[0].position[0] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn lfe_is_not_an_object() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 3,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(
                vec![object(1, [0.0, 0.0, 0.0]), object(2, [1.0, 0.0, 0.0])],
                vec![],
            ),
        ));
        let snapshot = timeline.snapshot_at(0, 0.0).expect("snapshot");
        assert_eq!(snapshot.objects.len(), 2);
        assert_eq!(snapshot.objects[0].id, 1);
    }

    #[test]
    fn lfe_and_object_levels_are_independent() {
        for levels in [vec![1.0, 0.0, 0.0], vec![0.0, 0.01, 0.1]] {
            let mut timeline = Timeline::default();
            timeline.handle(TapEvent::Buffer {
                epoch: 0,
                pts_ns: 0,
                payloads: vec![payload(
                    vec![object(1, [0.0; 3]), object(2, [0.0; 3])],
                    vec![],
                )],
                levels: levels.clone(),
                discont: false,
            });
            let snapshot = timeline.snapshot_at(0, 0.0).unwrap();
            assert_eq!(snapshot.lfe_level, normalized_level(levels[0]));
            assert_eq!(snapshot.objects[0].level, normalized_level(levels[1]));
            assert_eq!(snapshot.objects[1].level, normalized_level(levels[2]));
        }
    }

    #[test]
    fn lfe_only_buffers_produce_a_snapshot() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 1,
            rate: 48_000,
        });
        timeline.handle(buffer_event(0, 0, payload(vec![], vec![])));
        let snapshot = timeline.snapshot_at(0, 0.0).unwrap();
        assert!(snapshot.objects.is_empty());
        assert_eq!(snapshot.lfe_level, normalized_level(0.5));
        assert_eq!(snapshot.state, VisualState::Live);
    }

    #[test]
    fn lfe_level_smooths_attack_and_decays_to_silence() {
        let mut timeline = Timeline::default();
        let mut push = |pts_ns, lfe| {
            timeline.handle(TapEvent::Buffer {
                epoch: 0,
                pts_ns,
                payloads: vec![payload(vec![object(1, [0.0; 3])], vec![])],
                levels: vec![lfe, 0.0],
                discont: false,
            });
            timeline.snapshot_at(pts_ns, 0.0).unwrap()
        };
        assert_eq!(push(0, 0.0).lfe_level, 0.0);
        let attack = push(40_000_000, 1.0).lfe_level;
        assert!(attack > 0.0 && attack < 1.0);
        let release = push(80_000_000, 0.0).lfe_level;
        assert!(release > 0.0 && release < attack);
        for frame in 3..=80 {
            let snapshot = push(frame * 40_000_000, 0.0);
            assert_eq!(snapshot.objects[0].level, 0.0);
        }
        assert_eq!(
            timeline.snapshot_at(3_200_000_000, 0.0).unwrap().lfe_level,
            0.0
        );
    }

    #[test]
    fn flush_resets_the_timeline() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![]),
        ));
        timeline.handle(TapEvent::Flush {
            epoch: 0,
            stop: true,
        });
        assert!(timeline.snapshot_at(0, 1.0).is_none());
        timeline.handle(TapEvent::Buffer {
            epoch: 0,
            pts_ns: 0,
            payloads: vec![payload(vec![object(1, [0.0; 3])], vec![])],
            levels: vec![0.0, 0.5],
            discont: false,
        });
        assert_eq!(timeline.snapshot_at(0, 0.0).unwrap().lfe_level, 0.0);
    }

    #[test]
    fn metadata_gap_marks_the_snapshot_stale() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![]),
        ));
        timeline.handle(TapEvent::Buffer {
            epoch: 0,
            pts_ns: 1_000_000_000,
            payloads: Vec::new(),
            levels: vec![0.0, 0.0],
            discont: false,
        });
        let snapshot = timeline.snapshot_at(1_000_000_000, 0.0).expect("snapshot");
        assert_eq!(snapshot.state, VisualState::Stale);
        assert_eq!(snapshot.lfe_level, 0.0);
        assert_eq!(snapshot.objects[0].position, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn eos_freezes_the_last_snapshot() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![]),
        ));
        timeline.handle(TapEvent::Eos { epoch: 0 });
        let snapshot = timeline.snapshot_at(10_000_000_000, 0.0).expect("snapshot");
        assert_eq!(snapshot.state, VisualState::Eos);
        assert_eq!(snapshot.lfe_level, normalized_level(0.5));
    }

    #[test]
    fn a_discont_buffer_starts_a_new_segment() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![]),
        ));
        timeline.mark_stale();
        let stale = timeline.snapshot_at(0, 0.0).expect("stale snapshot");
        assert_eq!(stale.state, VisualState::Stale);
        assert_eq!(stale.objects[0].position, [0.0, 0.0, 0.0]);

        timeline.handle(TapEvent::Buffer {
            epoch: 0,
            pts_ns: 1_000_000_000,
            payloads: vec![payload(vec![object(1, [1.0, 0.0, 0.0])], vec![])],
            levels: vec![0.0, 0.5],
            discont: true,
        });
        assert_eq!(timeline.tracks().len(), 1);
        let snapshot = timeline.snapshot_at(1_000_000_000, 0.0).expect("snapshot");
        assert_eq!(snapshot.objects[0].position, [1.0, 0.0, 0.0]);
        assert_eq!(snapshot.state, VisualState::Live);
        assert_eq!(snapshot.lfe_level, 0.0);
    }

    #[test]
    fn a_newer_epoch_drops_old_events() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        timeline.handle(buffer_event(
            0,
            0,
            payload(vec![object(1, [0.0, 0.0, 0.0])], vec![]),
        ));
        timeline.reset(2);
        timeline.handle(TapEvent::Buffer {
            epoch: 1,
            pts_ns: 1_000_000_000,
            payloads: vec![payload(vec![object(1, [1.0, 0.0, 0.0])], vec![])],
            levels: vec![0.5, 0.5],
            discont: false,
        });
        assert!(timeline.snapshot_at(1_000_000_000, 0.0).is_none());
    }

    #[test]
    fn committed_trail_history_is_not_rewritten_by_late_metadata() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        // Sparse state snapshots hold until the next authored update.
        for (sample, x) in [(0u64, 0.0f32), (1_000_000_000, 1.0), (2_000_000_000, 2.0)] {
            timeline.handle(buffer_event(
                sample,
                0,
                payload(vec![object(1, [x, 0.0, 0.0])], vec![]),
            ));
        }

        let first = timeline
            .snapshot_at(1_000_000_000, 1.0)
            .expect("snapshot")
            .objects[0]
            .trail
            .clone();
        assert!(first.len() > 2);
        assert!(first.iter().any(|point| point[0] < 0.5));

        // A late keyframe rewrites the interpolation around 0.4 s. The trail
        // has already been travelled, so its committed vertices must not move.
        timeline.apply_payload(
            390_000_000,
            &payload(vec![object(1, [5.0, 0.0, 0.0])], vec![]),
        );
        let resampled =
            sample_track(&timeline.tracks()[0], 400_000_000, timeline.sample_rate).position;
        assert!(
            (resampled[0] - 0.4).abs() > 1.0,
            "the test must actually change the timeline"
        );

        let again = timeline
            .snapshot_at(1_000_000_000, 1.0)
            .expect("snapshot")
            .objects[0]
            .trail
            .clone();
        assert_eq!(first, again);
    }

    #[test]
    fn advancing_the_playhead_keeps_shared_trail_vertices_fixed() {
        let mut timeline = Timeline::default();
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: 2,
            rate: 48_000,
        });
        for step in 0..=300u64 {
            timeline.handle(buffer_event(
                step * 10_000_000,
                0,
                payload(vec![object(1, [step as f32 * 0.01, 0.0, 0.0])], vec![]),
            ));
        }

        let first = timeline
            .snapshot_at(1_000_000_000, 0.5)
            .expect("snapshot")
            .objects[0]
            .trail
            .clone();
        let second = timeline
            .snapshot_at(1_300_000_000, 0.5)
            .expect("snapshot")
            .objects[0]
            .trail
            .clone();
        // The old window [0.5, 1.0] and the new window [0.8, 1.3] share
        // [0.8, 1.0]. Exclude the live endpoint: only committed vertices persist.
        for point in first.iter().take(first.len() - 1) {
            if (0.8..=1.0).contains(&point[0]) {
                assert!(
                    second.iter().any(|other| other == point),
                    "historical trail vertex {point:?} moved"
                );
            }
        }
    }

    #[test]
    fn trails_are_drawn_for_any_object_count() {
        let mut timeline = Timeline::default();
        let count = 96u32;
        timeline.handle(TapEvent::Caps {
            epoch: 0,
            channels: count + 1,
            rate: 48_000,
        });
        let objects_at = |x: f32| {
            (1..=count)
                .map(|id| object(id, [x, 0.0, 0.0]))
                .collect::<Vec<_>>()
        };
        timeline.handle(buffer_event(0, 0, payload(objects_at(0.0), vec![])));
        timeline.handle(buffer_event(
            1_000_000_000,
            0,
            payload(objects_at(1.0), vec![]),
        ));

        let snapshot = timeline.snapshot_at(1_000_000_000, 1.0).expect("snapshot");
        assert_eq!(snapshot.objects.len() as u32, count);
        assert!(snapshot.objects.iter().all(|object| object.trail.len() > 1));
    }
}
