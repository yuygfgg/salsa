//! Timestamped object keyframes and interpolation.
//!
//! The probe publishes keyframes, not per-display-frame snapshots. The UI
//! samples this timeline at the pipeline position so motion stays aligned with
//! the audible stream even when the display refresh rate differs from the
//! decoder rate.

use gstreamer as gst;
use objectaudio_ir::{ObjectState, SpatialPayload};

/// Seconds of keyframe history kept per object (also the maximum trail depth).
pub const HISTORY_SECONDS: u64 = 12;
/// Hard cap on keyframes retained per object.
pub const MAX_KEYFRAMES_PER_OBJECT: usize = 256;
/// Objects above this count get no trails (labels only on hover).
pub const MAX_TRAIL_OBJECTS: usize = 64;
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

#[derive(Debug, Clone)]
pub(crate) struct Keyframe {
    pub time_ns: u64,
    pub state: ObjectState,
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
        offset: usize,
        payload: Option<SpatialPayload>,
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
    tracks: Vec<Vec<Keyframe>>,
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
            tracks: Vec::new(),
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
        self.tracks.clear();
        self.levels.clear();
        self.level_time_ns = None;
        self.last_pts_ns = None;
        self.last_meta_ns = None;
        self.state = VisualState::Seeking;
        self.frozen = false;
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
                offset,
                payload,
                levels,
                discont,
            } => self.push_buffer(pts_ns, offset, payload, levels, discont),
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
        let objects = channels.saturating_sub(1) as usize;
        if channels != self.channels || rate != self.sample_rate {
            self.channels = channels;
            self.sample_rate = rate.max(1);
            if objects != self.tracks.len() {
                self.object_ids.clear();
                self.tracks.clear();
                self.levels.clear();
                self.level_time_ns = None;
                self.state = VisualState::Seeking;
            }
        }
    }

    fn clear_timeline(&mut self) {
        self.object_ids.clear();
        self.tracks.clear();
        self.levels.clear();
        self.level_time_ns = None;
        self.last_pts_ns = None;
        self.state = VisualState::Seeking;
    }

    fn push_buffer(
        &mut self,
        pts_ns: u64,
        offset: usize,
        payload: Option<SpatialPayload>,
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

        let rate = payload
            .as_ref()
            .map(|payload| payload.sample_rate)
            .filter(|rate| *rate != 0)
            .unwrap_or(self.sample_rate);

        if let Some(payload) = payload {
            self.sample_rate = rate;
            self.ensure_objects(&payload.objects);
            self.update_levels(&levels, pts_ns);
            self.apply_payload(pts_ns, offset, &payload);
            self.state = VisualState::Live;
            self.last_meta_ns = Some(pts_ns);
        } else {
            self.update_levels(&levels, pts_ns);
            if self.frozen {
                self.state = VisualState::Eos;
            } else if self
                .last_meta_ns
                .is_none_or(|last| pts_ns.saturating_sub(last) > STALE_AFTER_NS)
            {
                self.state = VisualState::Stale;
            }
        }

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
        self.tracks = vec![Vec::new(); objects.len()];
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

    fn apply_payload(&mut self, pts_ns: u64, offset: usize, payload: &SpatialPayload) {
        let rate = self.sample_rate.max(1);
        let snapshot_ns = pts_ns.saturating_add(offset_to_ns(offset, rate));
        let mut updates: Vec<(u64, usize, ObjectState)> =
            Vec::with_capacity(payload.objects.len() + payload.events.len() * 2);

        for (index, object) in payload.objects.iter().enumerate() {
            updates.push((snapshot_ns, index, object.clone()));
        }

        let mut events = payload.events.clone();
        events.sort_by_key(|event| event.offset);
        for event in events {
            let Some(index) = self.object_index(event.object) else {
                continue;
            };
            let time_ns = pts_ns.saturating_add(offset_to_ns(event.offset as usize, rate));
            if event.ramp_samples > 0 {
                let previous = self
                    .sample_state(index, time_ns)
                    .or_else(|| payload.objects.get(index).cloned());
                if let Some(previous) = previous {
                    updates.push((time_ns, index, previous));
                }
                let end_ns =
                    time_ns.saturating_add(offset_to_ns(event.ramp_samples as usize, rate));
                updates.push((end_ns, index, event.state));
            } else {
                updates.push((time_ns, index, event.state));
            }
        }

        updates.sort_by_key(|(time_ns, _, _)| *time_ns);
        for (time_ns, index, state) in updates {
            self.insert_keyframe(index, time_ns, state);
        }
    }

    fn object_index(&self, id: u32) -> Option<usize> {
        self.object_ids
            .iter()
            .position(|candidate| *candidate == id)
    }

    fn insert_keyframe(&mut self, index: usize, time_ns: u64, state: ObjectState) {
        let Some(track) = self.tracks.get_mut(index) else {
            return;
        };
        let position = track.partition_point(|keyframe| keyframe.time_ns < time_ns);
        if let Some(existing) = track.get_mut(position)
            && existing.time_ns == time_ns
        {
            existing.state = state;
            return;
        }
        track.insert(position, Keyframe { time_ns, state });
    }

    fn trim(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(HISTORY_SECONDS.saturating_mul(1_000_000_000));
        for track in &mut self.tracks {
            let drop = track.partition_point(|keyframe| keyframe.time_ns < cutoff);
            if drop > 0 {
                track.drain(0..drop);
            }
            if track.len() > MAX_KEYFRAMES_PER_OBJECT {
                let remove = track.len() - MAX_KEYFRAMES_PER_OBJECT;
                track.drain(0..remove);
            }
        }
    }

    fn sample_state(&self, index: usize, time_ns: u64) -> Option<ObjectState> {
        let track = self.tracks.get(index)?;
        sample_track(track, time_ns)
    }

    pub(crate) fn snapshot_at(&self, time_ns: u64, trail_seconds: f32) -> Option<VisualSnapshot> {
        let first_ns = self
            .tracks
            .iter()
            .filter_map(|track| track.first().map(|keyframe| keyframe.time_ns))
            .min()
            .or(self.level_time_ns)?;

        let stale = self.state == VisualState::Stale
            || self.last_pts_ns.is_some_and(|last| {
                time_ns.saturating_sub(last) > STALE_AFTER_NS && self.state != VisualState::Eos
            });

        let trail_ns = (trail_seconds.max(0.0) * 1e9) as u64;
        let mut objects = Vec::with_capacity(self.tracks.len());
        for (index, track) in self.tracks.iter().enumerate() {
            let Some(state) = sample_track(track, time_ns) else {
                continue;
            };
            let level = self
                .levels
                .get(index + 1)
                .copied()
                .map(normalized_level)
                .unwrap_or(0.0);
            let trail = if trail_ns == 0 || self.tracks.len() > MAX_TRAIL_OBJECTS {
                Vec::new()
            } else {
                build_trail(track, time_ns, trail_ns)
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

fn offset_to_ns(samples: usize, rate: u32) -> u64 {
    (samples as u128 * 1_000_000_000u128 / u128::from(rate.max(1))) as u64
}

fn normalized_level(rms: f32) -> f32 {
    if !rms.is_finite() || rms <= 1e-7 {
        0.0
    } else {
        ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    }
}

fn sample_track(track: &[Keyframe], time_ns: u64) -> Option<ObjectState> {
    if track.is_empty() {
        return None;
    }
    let index = track.partition_point(|keyframe| keyframe.time_ns <= time_ns);
    if index == 0 {
        return Some(track[0].state.clone());
    }
    let before = &track[index - 1];
    let Some(after) = track.get(index) else {
        return Some(before.state.clone());
    };
    if after.time_ns <= before.time_ns {
        return Some(after.state.clone());
    }
    let span = (after.time_ns - before.time_ns) as f32;
    let elapsed = (time_ns - before.time_ns) as f32;
    Some(lerp_state(
        &before.state,
        &after.state,
        (elapsed / span).clamp(0.0, 1.0),
    ))
}

fn lerp_state(a: &ObjectState, b: &ObjectState, amount: f32) -> ObjectState {
    let lerp = |x: f32, y: f32| x + (y - x) * amount;
    ObjectState {
        id: a.id,
        position: [
            lerp(a.position[0], b.position[0]),
            lerp(a.position[1], b.position[1]),
            lerp(a.position[2], b.position[2]),
        ],
        gain: lerp(a.gain, b.gain),
        enabled: if amount < 0.5 { a.enabled } else { b.enabled },
        size: match (a.size, b.size) {
            (Some(x), Some(y)) => Some(lerp(x, y)),
            (Some(x), None) => Some(x),
            (None, Some(y)) => Some(y),
            (None, None) => None,
        },
        spread: match (a.spread, b.spread) {
            (Some(x), Some(y)) => Some(lerp(x, y)),
            (Some(x), None) => Some(x),
            (None, Some(y)) => Some(y),
            (None, None) => None,
        },
    }
}

fn build_trail(track: &[Keyframe], time_ns: u64, trail_ns: u64) -> Vec<[f32; 3]> {
    let end = track.partition_point(|keyframe| keyframe.time_ns <= time_ns);
    if end == 0 {
        return Vec::new();
    }
    let start_time = time_ns.saturating_sub(trail_ns);
    let start = track.partition_point(|keyframe| keyframe.time_ns < start_time);
    if start >= end {
        return Vec::new();
    }
    let count = end - start;
    let stride = count.div_ceil(MAX_TRAIL_POINTS).max(1);
    let mut trail = Vec::with_capacity(count / stride + 1);
    let mut index = start;
    while index < end {
        trail.push(track[index].state.position);
        index += stride;
    }
    if let Some(state) = sample_track(track, time_ns)
        && trail.last().is_none_or(|last| *last != state.position)
    {
        trail.push(state.position);
    }
    trail
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
            offset,
            payload: Some(payload),
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
    fn interpolation_is_linear_between_keyframes() {
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
        assert!((snapshot.objects[0].position[0] - 0.5).abs() < 1e-4);
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
            ramp_samples: 4_800,
            object: 1,
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
                offset: 0,
                payload: Some(payload(
                    vec![object(1, [0.0; 3]), object(2, [0.0; 3])],
                    vec![],
                )),
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
                offset: 0,
                payload: Some(payload(vec![object(1, [0.0; 3])], vec![])),
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
            offset: 0,
            payload: Some(payload(vec![object(1, [0.0; 3])], vec![])),
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
            offset: 0,
            payload: None,
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
            offset: 0,
            payload: Some(payload(vec![object(1, [1.0, 0.0, 0.0])], vec![])),
            levels: vec![0.0, 0.5],
            discont: true,
        });
        assert_eq!(timeline.tracks[0].len(), 1);
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
            offset: 0,
            payload: Some(payload(vec![object(1, [1.0, 0.0, 0.0])], vec![])),
            levels: vec![0.5, 0.5],
            discont: false,
        });
        assert!(timeline.snapshot_at(1_000_000_000, 0.0).is_none());
    }
}
