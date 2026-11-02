use serde::{Deserialize, Serialize};

/// The state of one dynamic object.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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

    pub fn effective_gain(&self) -> f32 {
        if self.enabled { self.gain } else { 0.0 }
    }
}

impl Default for ObjectState {
    fn default() -> Self {
        Self::new(0, [0.0, 0.0, 0.0])
    }
}

/// Which continuous properties move toward a target. Discrete properties,
/// including `enabled`, change at the event start, never halfway through it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transition {
    #[default]
    /// Apply an evaluated state before any ramp target at the same sample.
    Step,
    Position(u32),
    All(u32),
}

impl Transition {
    pub fn samples(self) -> u32 {
        match self {
            Self::Step => 0,
            Self::Position(samples) | Self::All(samples) => samples,
        }
    }
}

/// A target change relative to the PCM buffer start. Negative offsets carry
/// the causal prefix of a sliced buffer; they are not clamped to zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectEvent {
    /// Sample offset relative to the start of the PCM buffer.
    pub offset: i64,
    pub transition: Transition,
    /// The target state also carries the event's sole object identifier.
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
