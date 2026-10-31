use serde::{Deserialize, Serialize};

/// The state of one dynamic object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectState {
    /// Decoder-defined object identifier.
    pub id: u32,
    /// ADM normalized Cartesian position (x right, y forward, z up).
    pub position: [f32; 3],
    /// Linear amplitude gain. `0.0` is silent.
    pub gain: f32,
    pub enabled: bool,
    /// Optional object size, reserved for Atmos-style rendering.
    pub size: Option<f32>,
    /// Optional object spread, reserved for Atmos-style rendering.
    pub spread: Option<f32>,
}

impl ObjectState {
    pub fn new(id: u32, position: [f32; 3]) -> Self {
        Self {
            id,
            position,
            gain: 1.0,
            enabled: true,
            size: None,
            spread: None,
        }
    }
}

impl Default for ObjectState {
    fn default() -> Self {
        Self::new(0, [0.0, 0.0, 0.0])
    }
}

/// A state change that starts at `offset` samples into the frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectEvent {
    /// Sample offset relative to the start of the PCM buffer.
    pub offset: u32,
    /// Ramp length in samples. `0` means an immediate change.
    pub ramp_samples: u32,
    /// Object identifier matching [`ObjectState::id`].
    pub object: u32,
    pub state: ObjectState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_enabled_and_unity_gain() {
        let state = ObjectState::new(7, [0.5, 0.5, 0.0]);
        assert_eq!(state.id, 7);
        assert!(state.enabled);
        assert_eq!(state.gain, 1.0);
        assert_eq!(state.size, None);
    }
}
