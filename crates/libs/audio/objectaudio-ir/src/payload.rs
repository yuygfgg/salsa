use serde::{Deserialize, Serialize};

use crate::{IrError, ObjectEvent, ObjectState, PayloadError, ProgramCalibration, Transition};

/// Current `SpatialPayload` wire version.
pub const PAYLOAD_VERSION: u16 = 3;

/// A resolved state and a new transition target are different operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Snapshot {
    #[default]
    State,
    /// Keep the previous stream state; `objects` only declares the layout.
    Continue,
    Target(Transition),
}

/// Metadata carried by `ObjectAudioMeta`.
///
/// The payload is metadata only: PCM stays in the GStreamer buffer. `objects`
/// is the snapshot at `snapshot_offset`; `events` carry state changes
/// whose offsets are relative to the start of the buffer.
///
/// A state snapshot is already evaluated. A target snapshot starts a transition
/// from stream history. No event supplies an implicit snapshot ramp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialPayload {
    pub version: u16,
    pub sample_rate: u32,
    pub objects: Vec<ObjectState>,
    pub events: Vec<ObjectEvent>,
    pub snapshot: Snapshot,
    pub snapshot_offset: i64,
    /// Programme calibration carried by the payload that declared it. A
    /// payload without it keeps the stream's previous controls.
    pub calibration: Option<ProgramCalibration>,
}

impl SpatialPayload {
    pub const VERSION: u16 = PAYLOAD_VERSION;

    pub fn new(sample_rate: u32, objects: Vec<ObjectState>, events: Vec<ObjectEvent>) -> Self {
        Self {
            version: Self::VERSION,
            sample_rate,
            objects,
            events,
            snapshot: Snapshot::State,
            snapshot_offset: 0,
            calibration: None,
        }
    }

    /// Attach programme speaker calibration to this payload.
    pub fn with_calibration(mut self, calibration: ProgramCalibration) -> Self {
        self.calibration = Some(calibration);
        self
    }

    /// Mark the snapshot as an OAMD-style transition target.
    pub fn with_snapshot_ramp(mut self, ramp_samples: u32) -> Self {
        self.snapshot = Snapshot::Target(Transition::All(ramp_samples));
        self
    }

    pub fn with_snapshot_offset(mut self, offset: i64) -> Self {
        self.snapshot_offset = offset;
        self
    }

    /// Move this payload to another buffer origin. Both timestamp kinds move
    /// exactly once; a snapshot offset never changes event offsets.
    pub fn rebase(&mut self, samples: i64) {
        self.snapshot_offset += samples;
        for event in &mut self.events {
            event.offset += samples;
        }
    }

    /// Keep the causal prefix and all events before the slice end. Retaining
    /// negative timestamps preserves in-progress ramps without changing their
    /// targets or durations. Target snapshots still require stream history.
    pub fn slice(&self, start: i64, frames: usize) -> Option<Self> {
        let end = start + frames as i64;
        // Programme calibration is not sample-positioned, so a calibration-only
        // payload still belongs to every slice of the stream.
        if self.calibration.is_none()
            && self.snapshot_offset >= end
            && self.events.iter().all(|event| event.offset >= end)
        {
            return None;
        }
        let mut result = self.clone();
        result.events.retain(|event| event.offset < end);
        result.rebase(-start);
        Some(result)
    }

    /// Validate the payload before it reaches a renderer.
    pub fn validate(&self) -> Result<(), IrError> {
        if self.version != Self::VERSION {
            return Err(IrError::UnsupportedPayloadVersion {
                version: self.version,
                supported: Self::VERSION,
            });
        }
        if self.sample_rate == 0 {
            return Err(IrError::ZeroSampleRate);
        }
        if let Some(calibration) = &self.calibration {
            calibration.validate()?;
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
            if event.state.id == 0 || event.state.id as usize > self.objects.len() {
                return Err(IrError::UnknownEventObject {
                    object: event.state.id,
                });
            }
        }
        for state in self
            .objects
            .iter()
            .chain(self.events.iter().map(|event| &event.state))
        {
            let ObjectState {
                id,
                position,
                gain,
                enabled: _,
                size,
                spread,
            } = *state;
            for (property, valid) in [
                ("position", position.iter().all(|value| value.is_finite())),
                ("gain", gain.is_finite() && gain >= 0.0),
                (
                    "size",
                    size.is_none_or(|value| value.is_finite() && value >= 0.0),
                ),
                (
                    "spread",
                    spread.is_none_or(|value| value.is_finite() && value >= 0.0),
                ),
            ] {
                if !valid {
                    return Err(IrError::InvalidObjectProperty {
                        object: id,
                        property,
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate metadata against the PCM format at a consumer boundary.
    pub fn validate_layout(&self, sample_rate: u32, channels: usize) -> Result<(), IrError> {
        self.validate()?;
        if self.sample_rate != sample_rate {
            return Err(IrError::SampleRateMismatch {
                expected: sample_rate,
                actual: self.sample_rate,
            });
        }
        let expected = self.objects.len() + 1;
        if channels != expected {
            return Err(IrError::ChannelObjectCountMismatch {
                channels,
                objects: self.objects.len(),
                expected,
            });
        }
        Ok(())
    }

    /// Encode into the compact `ObjectAudioMeta` wire format.
    pub fn encode(&self) -> Result<Vec<u8>, PayloadError> {
        self.validate()
            .map_err(|error| PayloadError::Encode(error.to_string()))?;
        postcard::to_allocvec(self).map_err(|error| PayloadError::Encode(error.to_string()))
    }

    /// Decode `ObjectAudioMeta` bytes and reject unknown versions.
    pub fn decode(bytes: &[u8]) -> Result<Self, PayloadError> {
        let payload: Self =
            postcard::from_bytes(bytes).map_err(|error| PayloadError::Decode(error.to_string()))?;
        if payload.version != Self::VERSION {
            return Err(PayloadError::UnsupportedVersion {
                version: payload.version,
                supported: Self::VERSION,
            });
        }
        payload
            .validate()
            .map_err(|error| PayloadError::Decode(error.to_string()))?;
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn payload(objects: usize, events: usize) -> SpatialPayload {
        SpatialPayload::new(
            48_000,
            (0..objects)
                .map(|id| ObjectState::new(id as u32 + 1, [0.5, 0.25, 0.0]))
                .collect(),
            (0..events)
                .map(|index| ObjectEvent {
                    offset: index as i64 * 32,
                    transition: crate::Transition::All(32),
                    state: ObjectState::new(1, [0.0, 1.0, 0.0]),
                })
                .collect(),
        )
    }

    #[test]
    fn roundtrips() {
        let source = payload(15, 3).with_snapshot_ramp(1536);
        let bytes = source.encode().unwrap();
        assert_eq!(SpatialPayload::decode(&bytes).unwrap(), source);
    }

    #[test]
    fn rejects_an_unknown_version() {
        let bytes = payload(1, 0).encode().unwrap();
        let mut decoded = SpatialPayload::decode(&bytes).unwrap();
        decoded.version = PAYLOAD_VERSION + 1;
        let bytes = postcard::to_allocvec(&decoded).unwrap();
        assert_eq!(
            SpatialPayload::decode(&bytes),
            Err(PayloadError::UnsupportedVersion {
                version: PAYLOAD_VERSION + 1,
                supported: PAYLOAD_VERSION,
            })
        );
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = SpatialPayload::decode(&bytes);
        }
    }
    #[test]
    fn calibration_roundtrips_and_is_validated() {
        let mut calibration = ProgramCalibration {
            warp_mode: 2,
            global_trim_mode: 2,
            ..ProgramCalibration::default()
        };
        calibration.configs[4] =
            crate::TrimRecord::explicit(Some(1.5), Some(-6.0), None, Some(-0.5), Some(0.25));
        calibration.bypass = vec![false, true];
        let mut source = payload(2, 0);
        source.calibration = Some(calibration.clone());
        let bytes = source.encode().unwrap();
        let decoded = SpatialPayload::decode(&bytes).unwrap();
        assert_eq!(decoded.calibration, Some(calibration.clone()));
        assert_eq!(decoded.version, SpatialPayload::VERSION);

        let mut invalid = calibration.clone();
        invalid.warp_mode = 4;
        source.calibration = Some(invalid);
        assert!(source.encode().is_err());
    }

    #[test]
    fn slicing_keeps_programme_calibration() {
        let mut source = payload(1, 0).with_snapshot_offset(128);
        source.calibration = Some(ProgramCalibration::default());
        let sliced = source
            .slice(0, 32)
            .expect("calibration is not sample-positioned");
        assert!(sliced.calibration.is_some());
    }
}
