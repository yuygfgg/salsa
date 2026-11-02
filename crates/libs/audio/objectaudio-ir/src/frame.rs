use crate::{IrError, ObjectEvent, ObjectState, PcmBuffer, Snapshot, SpatialPayload, Transition};

/// One decoded block of object audio.
///
/// `pcm` is planar: channel 0 is the LFE and channel `i + 1` is the object
/// `objects[i]`. The IR stores objects in channel order and validates that
/// `objects[i].id == i + 1`, so renderers never have to guess.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialFrame {
    pub sample_rate: u32,
    pub frames: usize,
    pub objects: Vec<ObjectState>,
    pub events: Vec<ObjectEvent>,
    pub snapshot: Snapshot,
    pub snapshot_offset: i64,
    pub pcm: PcmBuffer,
}

impl SpatialFrame {
    pub fn new(
        sample_rate: u32,
        frames: usize,
        objects: Vec<ObjectState>,
        events: Vec<ObjectEvent>,
        pcm: PcmBuffer,
    ) -> Result<Self, IrError> {
        let frame = Self {
            sample_rate,
            frames,
            objects,
            events,
            snapshot: Snapshot::State,
            snapshot_offset: 0,
            pcm,
        };
        frame.validate()?;
        Ok(frame)
    }

    pub fn validate(&self) -> Result<(), IrError> {
        if self.sample_rate == 0 {
            return Err(IrError::ZeroSampleRate);
        }
        if self.frames != self.pcm.frames() {
            return Err(IrError::FrameLengthMismatch {
                declared: self.frames,
                actual: self.pcm.frames(),
            });
        }
        self.to_payload()
            .validate_layout(self.sample_rate, self.pcm.channel_count())
    }

    pub fn with_snapshot_offset(mut self, offset: i64) -> Self {
        self.snapshot_offset = offset;
        self
    }

    /// Set the ramp used when a renderer moves to this frame's object state.
    pub fn with_snapshot_ramp(mut self, ramp_samples: u32) -> Self {
        self.snapshot = Snapshot::Target(Transition::All(ramp_samples));
        self
    }

    /// Metadata-only snapshot for `ObjectAudioMeta`.
    pub fn to_payload(&self) -> SpatialPayload {
        let payload =
            SpatialPayload::new(self.sample_rate, self.objects.clone(), self.events.clone());
        SpatialPayload {
            snapshot: self.snapshot,
            snapshot_offset: self.snapshot_offset,
            ..payload
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(objects: Vec<ObjectState>, channels: usize, events: Vec<ObjectEvent>) -> SpatialFrame {
        SpatialFrame {
            sample_rate: 48_000,
            frames: 0,
            objects,
            events,
            snapshot: Snapshot::State,
            snapshot_offset: 0,
            pcm: PcmBuffer::silent(channels, 0),
        }
    }

    #[test]
    fn validates_matching_frame() {
        let frame = SpatialFrame::new(
            48_000,
            4,
            vec![ObjectState::new(1, [0.0, 1.0, 0.0])],
            vec![ObjectEvent {
                offset: 2,
                transition: crate::Transition::All(16),
                state: ObjectState::new(1, [1.0, 1.0, 0.0]),
            }],
            PcmBuffer::silent(2, 4),
        )
        .unwrap();
        assert_eq!(frame.pcm.frames(), 4);
    }

    #[test]
    fn rejects_a_channel_count_that_does_not_match_the_objects() {
        let frame = frame(vec![ObjectState::new(1, [0.0, 1.0, 0.0])], 1, Vec::new());
        assert_eq!(
            frame.validate(),
            Err(IrError::ChannelObjectCountMismatch {
                channels: 1,
                objects: 1,
                expected: 2,
            })
        );
    }

    #[test]
    fn rejects_non_contiguous_object_ids() {
        let frame = frame(vec![ObjectState::new(7, [0.0, 1.0, 0.0])], 2, Vec::new());
        assert_eq!(
            frame.validate(),
            Err(IrError::ObjectIdNotContiguous {
                index: 0,
                id: 7,
                expected: 1,
            })
        );
    }

    #[test]
    fn rejects_events_for_missing_objects() {
        let event = ObjectEvent {
            offset: 0,
            transition: crate::Transition::All(0),
            state: ObjectState::new(2, [0.0, 0.0, 0.0]),
        };
        let frame = frame(vec![ObjectState::new(1, [0.0, 1.0, 0.0])], 2, vec![event]);
        assert_eq!(
            frame.validate(),
            Err(IrError::UnknownEventObject { object: 2 })
        );
    }

    #[test]
    fn snapshot_ramp_reaches_the_payload() {
        let frame = SpatialFrame::new(
            48_000,
            4,
            vec![ObjectState::new(1, [0.0, 1.0, 0.0])],
            Vec::new(),
            PcmBuffer::silent(2, 4),
        )
        .unwrap()
        .with_snapshot_ramp(1536);
        assert_eq!(frame.snapshot, Snapshot::Target(Transition::All(1536)));
        assert_eq!(
            frame.to_payload().snapshot,
            Snapshot::Target(Transition::All(1536))
        );
    }

    #[test]
    fn rejects_zero_sample_rate() {
        let mut frame = frame(Vec::new(), 1, Vec::new());
        frame.sample_rate = 0;
        assert_eq!(frame.validate(), Err(IrError::ZeroSampleRate));
    }
}
