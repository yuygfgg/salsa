//! Project resolved IR tracks onto DAMF's position-only ramps.

use std::collections::BTreeMap;

use objectaudio_ir::{ObjectEvent, ObjectState, SceneTimeline, StateTrack, Transition};

use crate::{DamfError, DamfEvent, Gain, Metadata, Result};

impl Metadata {
    /// Export the samples in `[0, frames)` with contiguous DAMF object IDs.
    ///
    /// Resolve signed history and simultaneous updates before conversion.
    /// DAMF can ramp positions, but gain and size are step properties. Expand
    /// trajectories that DAMF cannot express into evaluated sample states.
    pub fn from_timeline(
        timeline: &SceneTimeline,
        frames: usize,
        object_id_base: i64,
    ) -> Result<Self> {
        let end = i64::try_from(frames)
            .map_err(|_| DamfError::Invalid("DAMF duration exceeds the IR timeline".into()))?;
        let mut events = Vec::new();
        if frames > 0 {
            for (index, track) in timeline.tracks().iter().enumerate() {
                let id = object_id_base
                    .checked_add(index as i64)
                    .ok_or_else(|| DamfError::Invalid("DAMF object ID overflow".into()))?;
                events.extend(export_track(track, end, id, index, timeline));
            }
        }
        events.sort_by_key(|event| (event.sample_pos, event.id));
        Ok(Self {
            sample_rate: timeline.sample_rate(),
            events,
        })
    }
}

fn export_track(
    track: &StateTrack,
    end: i64,
    id: i64,
    index: usize,
    timeline: &SceneTimeline,
) -> Vec<DamfEvent> {
    let bypass_at = |sample: i64| {
        timeline
            .calibration_at(sample)
            .is_some_and(|calibration| calibration.is_bypassed(index))
    };
    // The track has already resolved state-before-target ordering. One update
    // per time now describes each interval, including a ramp crossing zero.
    let initial = track.state_at(0);
    let mut updates = BTreeMap::from([(
        0,
        ObjectEvent {
            offset: 0,
            transition: Transition::Step,
            state: initial,
        },
    )]);
    for event in track.events(0, end as usize) {
        updates.insert(event.offset, event);
    }
    let mut updates = updates.into_values().peekable();
    let mut output = TrackExport {
        id,
        spatial: StateTrack::new(initial),
        events: Vec::new(),
    };
    while let Some(event) = updates.next() {
        let start = event.offset;
        let next = updates.peek().map_or(end, |event| event.offset);
        let origin = track.state_at(start);
        let duration = event.transition.samples();
        let constant_properties =
            origin.gain == event.state.gain && origin.size == event.state.size;
        if duration == 0 || (constant_properties && origin.position == event.state.position) {
            output.push(start, origin, 0, bypass_at(start));
        } else if !output.events.is_empty()
            && output.spatial.state_at(start).position == origin.position
            && (constant_properties || matches!(event.transition, Transition::Position(_)))
        {
            output.push(start, event.state, duration, bypass_at(start));
        } else {
            // A first ramp or a simultaneous position jump has no representable
            // DAMF origin. Sampling also preserves All's linear gain and size.
            // Include the settled state, unless another update interrupts it.
            let stop = next.min(start.saturating_add(i64::from(duration) + 1));
            for sample in start..stop {
                output.push(sample, track.state_at(sample), 0, bypass_at(sample));
            }
        }
    }
    output.events
}

struct TrackExport {
    id: i64,
    spatial: StateTrack,
    events: Vec<DamfEvent>,
}

impl TrackExport {
    fn push(&mut self, sample: i64, state: ObjectState, ramp: u32, bypass: bool) {
        self.events
            .push(damf_event(self.id, sample as u64, state, ramp, bypass));
        self.spatial
            .set_target(sample, state, Transition::Position(ramp));
        self.spatial.discard_before(sample);
    }
}

fn damf_event(id: i64, sample_pos: u64, state: ObjectState, ramp: u32, bypass: bool) -> DamfEvent {
    let ObjectState {
        id: _,
        position: [x, y, z],
        gain,
        enabled,
        size,
        spread: _,
    } = state;
    let gain = if gain > 0.0 {
        Gain::Db(20.0 * gain.log10())
    } else {
        Gain::MinusInfinity
    };
    DamfEvent {
        id,
        sample_pos,
        active: Some(enabled),
        position: Some([x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0), z.clamp(0.0, 1.0)]),
        snap: Some(false),
        elevation: Some(true),
        zones: Some("all".to_string()),
        size: Some(size.unwrap_or(0.0)),
        decorr: Some(0),
        importance: Some(1.0),
        gain: Some(gain),
        ramp_length: Some(ramp),
        trim_bypass: Some(bypass),
        dialog: Some(-1.0),
        music: Some(-1.0),
        screen_factor: Some(0.0),
        depth_factor: Some(0.25),
        head_track_mode: Some("undefined".to_string()),
        binaural_render_mode: Some("undefined".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectTrack;
    use objectaudio_ir::SpatialPayload;

    fn assert_roundtrip(timeline: &SceneTimeline, frames: usize) -> Metadata {
        let metadata = Metadata::from_timeline(timeline, frames, 10).unwrap();
        assert_eq!(metadata.sample_rate, timeline.sample_rate());
        for (index, expected) in timeline.tracks().iter().enumerate() {
            let events: Vec<_> = metadata
                .events
                .iter()
                .filter(|event| event.id == 10 + index as i64)
                .collect();
            assert!(
                events
                    .windows(2)
                    .all(|pair| pair[0].sample_pos < pair[1].sample_pos)
            );
            assert!(events.iter().all(|event| event.sample_pos < frames as u64));
            let actual = ObjectTrack::dynamic(&events);
            for sample in 0..frames {
                let actual = actual.state_at(sample as u64);
                let expected = expected.state_at(sample as i64);
                assert_eq!(actual.enabled, expected.enabled, "sample {sample}");
                for (a, b) in actual.position.into_iter().zip(expected.position).chain([
                    (actual.gain, expected.gain),
                    (actual.size.unwrap_or(0.0), expected.size.unwrap_or(0.0)),
                ]) {
                    assert!(
                        (a - b).abs() < 1e-6,
                        "sample {sample}: got {actual:?}, expected {expected:?}"
                    );
                }
            }
        }
        metadata
    }

    #[test]
    fn exports_per_object_trim_bypass_from_calibration() {
        let payload = SpatialPayload::new(
            48_000,
            vec![
                ObjectState::new(1, [0.0, 0.0, 0.0]),
                ObjectState::new(2, [0.0, 0.0, 0.0]),
            ],
            Vec::new(),
        )
        .with_calibration(objectaudio_ir::ProgramCalibration {
            bypass: vec![false, true],
            ..objectaudio_ir::ProgramCalibration::default()
        });
        let metadata =
            Metadata::from_timeline(&SceneTimeline::from_payload(&payload, 0).unwrap(), 64, 10)
                .unwrap();
        for event in &metadata.events {
            assert_eq!(event.trim_bypass, Some(event.id == 11));
        }
    }

    #[test]
    fn keeps_representable_position_ramps_sparse() {
        for transition in [Transition::Position(64), Transition::All(64)] {
            let initial = ObjectState::new(1, [-1.0, 0.0, 0.0]);
            let payload = SpatialPayload::new(
                48_000,
                vec![initial],
                vec![ObjectEvent {
                    offset: 32,
                    transition,
                    state: ObjectState {
                        position: [1.0, 0.0, 0.0],
                        ..initial
                    },
                }],
            );
            let metadata =
                assert_roundtrip(&SceneTimeline::from_payload(&payload, 0).unwrap(), 128);
            assert_eq!(metadata.events.len(), 2);
            assert_eq!(metadata.events[1].sample_pos, 32);
            assert_eq!(metadata.events[1].ramp_length, Some(64));
        }
    }

    #[test]
    fn preserves_gain_only_and_size_only_ramps() {
        for (gain, size) in [(0.25, 0.0), (1.0, 0.8), (0.0, 0.8)] {
            let initial = ObjectState {
                size: Some(0.0),
                ..ObjectState::new(1, [0.0; 3])
            };
            let payload = SpatialPayload::new(
                48_000,
                vec![initial],
                vec![ObjectEvent {
                    offset: 8,
                    transition: Transition::All(64),
                    state: ObjectState {
                        gain,
                        size: Some(size),
                        ..initial
                    },
                }],
            );
            assert_roundtrip(&SceneTimeline::from_payload(&payload, 0).unwrap(), 128);
        }
    }

    #[test]
    fn preserves_short_interrupted_and_completed_ramps_after_slicing() {
        for duration in [0, 1, 2, 32, 64] {
            for transition in [Transition::Position(duration), Transition::All(duration)] {
                let initial = ObjectState {
                    size: Some(0.2),
                    ..ObjectState::new(1, [-1.0, 0.0, 0.0])
                };
                let target = ObjectState {
                    position: [1.0, 0.5, 0.8],
                    gain: 0.0,
                    size: Some(0.8),
                    ..initial
                };
                let payload = SpatialPayload::new(
                    48_000,
                    vec![initial],
                    vec![
                        ObjectEvent {
                            offset: 8,
                            transition,
                            state: target,
                        },
                        ObjectEvent {
                            offset: 17,
                            transition,
                            state: ObjectState {
                                enabled: false,
                                gain: 0.7,
                                ..initial
                            },
                        },
                        ObjectEvent {
                            offset: 40,
                            transition: Transition::Step,
                            state: initial,
                        },
                    ],
                );
                for start in [0, 7, 16, 45, 100] {
                    let sliced = payload.slice(start, 128).unwrap();
                    assert_roundtrip(&SceneTimeline::from_payload(&sliced, 0).unwrap(), 128);
                }
            }
        }
    }
}
