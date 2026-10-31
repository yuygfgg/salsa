use crate::{IrError, ObjectEvent, ObjectState, PcmBuffer, SpatialPayload};

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
        let expected_channels = self.objects.len() + 1;
        if expected_channels != self.pcm.channel_count() {
            return Err(IrError::ChannelObjectCountMismatch {
                channels: self.pcm.channel_count(),
                objects: self.objects.len(),
                expected: expected_channels,
            });
        }
        for (index, object) in self.objects.iter().enumerate() {
            let expected = index as u32 + 1;
            if object.id != expected {
                return Err(IrError::ObjectIdNotContiguous {
                    index,
                    id: object.id,
                    expected,
                });
            }
        }
        for event in &self.events {
            if event.offset as usize > self.frames {
                return Err(IrError::EventOutOfRange {
                    offset: event.offset,
                    frames: self.frames,
                });
            }
            if event.object == 0 || event.object as usize > self.objects.len() {
                return Err(IrError::UnknownEventObject {
                    object: event.object,
                });
            }
        }
        Ok(())
    }

    /// Metadata-only snapshot for `ObjectAudioMeta`.
    pub fn to_payload(&self) -> SpatialPayload {
        SpatialPayload::new(self.sample_rate, self.objects.clone(), self.events.clone())
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
                ramp_samples: 16,
                object: 1,
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
            ramp_samples: 0,
            object: 2,
            state: ObjectState::new(2, [0.0, 0.0, 0.0]),
        };
        let frame = frame(vec![ObjectState::new(1, [0.0, 1.0, 0.0])], 2, vec![event]);
        assert_eq!(
            frame.validate(),
            Err(IrError::UnknownEventObject { object: 2 })
        );
    }

    #[test]
    fn rejects_zero_sample_rate() {
        let mut frame = frame(Vec::new(), 1, Vec::new());
        frame.sample_rate = 0;
        assert_eq!(frame.validate(), Err(IrError::ZeroSampleRate));
    }
}
