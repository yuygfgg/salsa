//! OAMD position timeline for the binaural renderer.
//!
//! OAMD positions are quantised Q15 triples that are scheduled on the same
//! 32-sample grid as the speaker renderer's control blocks. The number of tracks
//! is decided by the input stream at runtime.

use objectaudio_ir::SpatialPayload;

use crate::error::HrtfError;

const METADATA_BLOCK_SAMPLES: i64 = objectaudio_ir::METADATA_BLOCK_SAMPLES as i64;

/// Nearest metadata block boundary, ties toward the earlier block.
pub fn align_metadata_sample(sample: i64) -> i64 {
    let numerator = sample + METADATA_BLOCK_SAMPLES / 2 - 1;
    let quotient = if numerator >= 0 {
        numerator / METADATA_BLOCK_SAMPLES
    } else {
        -((-numerator + METADATA_BLOCK_SAMPLES - 1) / METADATA_BLOCK_SAMPLES)
    };
    METADATA_BLOCK_SAMPLES * quotient
}

#[derive(Debug, Clone, Copy, Default)]
struct Transition {
    start_sample: i64,
    duration_samples: i64,
    origin: [f64; 3],
    target: [f64; 3],
}

impl Transition {
    fn end_sample(&self) -> i64 {
        self.start_sample + self.duration_samples
    }
}

#[derive(Debug, Clone, Default)]
struct PositionTrack {
    initial: [f64; 3],
    last_target: [f64; 3],
    transitions: Vec<Transition>,
    cursor: usize,
}

impl PositionTrack {
    fn set_initial(&mut self, position: [f64; 3]) {
        self.initial = position;
        self.last_target = position;
    }

    fn append(&mut self, start_sample: i64, duration_samples: i64, target: [f64; 3]) {
        if let Some(previous) = self.transitions.last() {
            if start_sample < previous.end_sample() {
                // Overlapping ramps only happen on malformed metadata, so
                // clamp instead of rejecting the stream.
            }
            if start_sample == previous.start_sample && previous.duration_samples == 0 {
                let mut replacement = *previous;
                replacement.duration_samples = duration_samples;
                replacement.target = target;
                *self.transitions.last_mut().unwrap() = replacement;
                self.last_target = target;
                return;
            }
        }
        self.transitions.push(Transition {
            start_sample,
            duration_samples,
            origin: self.last_target,
            target,
        });
        self.last_target = target;
    }

    fn position_at(&mut self, sample: i64) -> [f64; 3] {
        while self.cursor < self.transitions.len() {
            let transition = &self.transitions[self.cursor];
            if sample < transition.end_sample() {
                break;
            }
            self.initial = transition.target;
            self.cursor += 1;
        }
        if self.cursor >= self.transitions.len() {
            return self.initial;
        }
        let transition = &self.transitions[self.cursor];
        if sample < transition.start_sample {
            return self.initial;
        }
        if transition.duration_samples == 0 {
            return transition.target;
        }
        let amount = (sample - transition.start_sample) as f64 / transition.duration_samples as f64;
        [
            transition.origin[0] + (transition.target[0] - transition.origin[0]) * amount,
            transition.origin[1] + (transition.target[1] - transition.origin[1]) * amount,
            transition.origin[2] + (transition.target[2] - transition.origin[2]) * amount,
        ]
    }
}

/// Position tracks for the point objects (channel 0 is the LFE).
#[derive(Debug, Clone)]
pub struct OamdPositionTimeline {
    object_count: usize,
    tracks: Vec<PositionTrack>,
    initialized: bool,
    previous_targets: Vec<[f64; 3]>,
    has_previous: Vec<bool>,
    last_coded_event_sample: i64,
    last_query_sample: i64,
}

impl Default for OamdPositionTimeline {
    fn default() -> Self {
        Self::new(0)
    }
}

impl OamdPositionTimeline {
    /// Create a timeline for `object_count` point objects (LFE excluded).
    pub fn new(object_count: usize) -> Self {
        Self {
            object_count,
            tracks: vec![PositionTrack::default(); object_count],
            initialized: false,
            previous_targets: vec![[0.0; 3]; object_count],
            has_previous: vec![false; object_count],
            last_coded_event_sample: -1,
            last_query_sample: -1,
        }
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.object_count);
    }

    pub fn initialized(&self) -> bool {
        self.initialized
    }

    fn targets(&self, frame: &SpatialPayload) -> Vec<[f64; 3]> {
        let mut targets = vec![[0.0; 3]; self.object_count];
        for (index, target) in targets.iter_mut().enumerate() {
            let Some(object) = frame.objects.get(index) else {
                continue;
            };
            *target = [
                f64::from(object.position[0]),
                f64::from(object.position[1]),
                f64::from(object.position[2]),
            ];
        }
        targets
    }

    /// Feed one spatial payload snapshot and its intra-buffer events.
    ///
    /// `frame_start_sample` is the number of samples already submitted;
    /// `outer_sample_offset` is the payload snapshot offset inside the buffer.
    pub fn submit_update(
        &mut self,
        frame: &SpatialPayload,
        frame_start_sample: i64,
        outer_sample_offset: i64,
        object_delay_samples: i64,
    ) -> Result<(), HrtfError> {
        let targets = self.targets(frame);
        let coded_event = frame_start_sample + outer_sample_offset;
        if coded_event < self.last_coded_event_sample {
            return Err(HrtfError::Format(format!(
                "non-monotonic spatial update at sample {coded_event}"
            )));
        }
        self.last_coded_event_sample = coded_event;
        if !self.initialized {
            for index in 0..self.object_count {
                self.tracks[index].set_initial(targets[index]);
                self.previous_targets[index] = targets[index];
                self.has_previous[index] = true;
            }
            self.initialized = true;
        } else {
            let ramp = frame
                .events
                .first()
                .map_or(0, |event| i64::from(event.ramp_samples));
            let transition_start = align_metadata_sample(coded_event + object_delay_samples);
            for index in 0..self.object_count {
                if self.has_previous[index] && self.previous_targets[index] == targets[index] {
                    continue;
                }
                self.tracks[index].append(transition_start, ramp, targets[index]);
                self.previous_targets[index] = targets[index];
                self.has_previous[index] = true;
            }
        }

        for event in &frame.events {
            let Some(index) = frame
                .objects
                .iter()
                .position(|object| object.id == event.object)
                .filter(|index| *index < self.object_count)
            else {
                continue;
            };
            let target = [
                f64::from(event.state.position[0]),
                f64::from(event.state.position[1]),
                f64::from(event.state.position[2]),
            ];
            if self.previous_targets[index] == target {
                continue;
            }
            let start =
                align_metadata_sample(coded_event + object_delay_samples + i64::from(event.offset));
            self.tracks[index].append(start, i64::from(event.ramp_samples), target);
            self.previous_targets[index] = target;
        }
        Ok(())
    }

    /// Fill `positions` with the point-object positions at `sample`; queries
    /// must increase. The caller owns the buffer so per-block queries do not
    /// allocate once its capacity has grown.
    pub fn positions_at_into(
        &mut self,
        sample: i64,
        positions: &mut Vec<[f64; 3]>,
    ) -> Result<(), HrtfError> {
        if sample < self.last_query_sample {
            return Err(HrtfError::State(
                "binaural metadata positions must be queried monotonically".into(),
            ));
        }
        self.last_query_sample = sample;
        positions.clear();
        positions.reserve(self.object_count);
        for track in &mut self.tracks {
            positions.push(track.position_at(sample));
        }
        Ok(())
    }

    /// Convenience wrapper around [`Self::positions_at_into`].
    pub fn positions_at(&mut self, sample: i64) -> Result<Vec<[f64; 3]>, HrtfError> {
        let mut positions = Vec::new();
        self.positions_at_into(sample, &mut positions)?;
        Ok(positions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objectaudio_ir::SpatialPayload;

    #[test]
    fn alignment_ties_toward_the_earlier_block() {
        assert_eq!(align_metadata_sample(0), 0);
        assert_eq!(align_metadata_sample(15), 0);
        assert_eq!(align_metadata_sample(16), 0);
        assert_eq!(align_metadata_sample(17), 32);
        assert_eq!(align_metadata_sample(-17), -32);
    }

    #[test]
    fn q15_center_is_the_front() {
        let position = objectaudio_ir::q15_to_adm([16384, 0, 0]);
        assert!(position[0].abs() < 0.02);
        assert!((position[1] - 1.0).abs() < 0.02);
        assert_eq!(position[2], 0.0);
    }

    #[test]
    fn tracks_are_dynamic() {
        let timeline = OamdPositionTimeline::new(128);
        assert_eq!(timeline.object_count(), 128);
    }

    #[test]
    fn targets_follow_the_payload_snapshot() {
        let frame = SpatialPayload::new(
            48_000,
            vec![objectaudio_ir::ObjectState::new(1, [0.5, 0.5, 0.0])],
            Vec::new(),
        );
        let timeline = OamdPositionTimeline::new(1);
        assert_eq!(timeline.targets(&frame), vec![[0.5, 0.5, 0.0]]);
    }
}
