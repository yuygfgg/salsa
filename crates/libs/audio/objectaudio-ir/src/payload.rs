use serde::{Deserialize, Serialize};

use crate::{IrError, ObjectEvent, ObjectState, PayloadError};

/// Current `SpatialPayload` wire version.
pub const PAYLOAD_VERSION: u16 = 1;

/// The version of [`SpatialPayload`] in this build.
pub fn payload_version() -> u16 {
    PAYLOAD_VERSION
}

/// Metadata carried by `ObjectAudioMeta`.
///
/// The payload is metadata only: PCM stays in the GStreamer buffer. `objects`
/// is the snapshot at the payload's sample offset; `events` carry state changes
/// whose offsets are relative to the start of the buffer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialPayload {
    pub version: u16,
    pub sample_rate: u32,
    pub objects: Vec<ObjectState>,
    pub events: Vec<ObjectEvent>,
}

impl SpatialPayload {
    pub const VERSION: u16 = PAYLOAD_VERSION;

    pub fn new(sample_rate: u32, objects: Vec<ObjectState>, events: Vec<ObjectEvent>) -> Self {
        Self {
            version: Self::VERSION,
            sample_rate,
            objects,
            events,
        }
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
        Ok(())
    }

    /// Encode into the compact `ObjectAudioMeta` wire format.
    pub fn encode(&self) -> Result<Vec<u8>, PayloadError> {
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
        if payload.sample_rate == 0 {
            return Err(PayloadError::Decode("sample rate must be non-zero".into()));
        }
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
                    offset: index as u32 * 32,
                    ramp_samples: 32,
                    object: 1,
                    state: ObjectState::new(1, [0.0, 1.0, 0.0]),
                })
                .collect(),
        )
    }

    #[test]
    fn roundtrips() {
        let source = payload(15, 3);
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
}
