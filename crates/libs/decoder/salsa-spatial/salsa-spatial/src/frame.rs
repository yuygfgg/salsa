//! Parsed frame types for one 100 ms codec tile.

use objectaudio_ir::{ObjectEvent, ObjectState};

use crate::params::{SceneParameters, TrajectoryKnot};

/// Five Opus packets describing the same 20 ms interval.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CorePacket {
    pub front: Vec<u8>,
    pub surround: Vec<u8>,
    pub height: Vec<u8>,
    pub center: Vec<u8>,
    pub lfe: Vec<u8>,
}

impl CorePacket {
    pub fn stream(&self, index: usize) -> Option<&[u8]> {
        match index {
            0 => Some(&self.front),
            1 => Some(&self.surround),
            2 => Some(&self.height),
            3 => Some(&self.center),
            4 => Some(&self.lfe),
            _ => None,
        }
    }
}

/// One enhancement record covering a 100 ms tile.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EnhancementTile {
    pub start_sample: u64,
    pub frames: usize,
    pub bytes: Vec<u8>,
    pub snapshot: bool,
}

/// Stream configuration recovered from the IAMF sequence header.
#[derive(Debug, Clone, PartialEq)]
pub struct SalsaSpatialStreamConfig {
    pub object_count: usize,
    pub headroom_gain: f64,
    pub initial_objects: Vec<ObjectState>,
    pub initial_trajectories: Vec<Vec<TrajectoryKnot>>,
    pub core_bitrates: [u32; 5],
    pub beta: f64,
}

impl Default for SalsaSpatialStreamConfig {
    fn default() -> Self {
        Self {
            object_count: 0,
            headroom_gain: 1.0,
            initial_objects: Vec::new(),
            initial_trajectories: Vec::new(),
            core_bitrates: [160_000, 128_000, 160_000, 112_000, 24_000],
            beta: crate::constants::DEFAULT_BETA,
        }
    }
}

/// A complete codec tile as consumed by the DSP decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct SalsaSpatialFrame {
    pub sample_rate: u32,
    pub start_sample: u64,
    pub frames: usize,
    pub packets: Vec<CorePacket>,
    pub enhancement: EnhancementTile,
    pub objects: Vec<ObjectState>,
    pub events: Vec<ObjectEvent>,
    pub snapshot: bool,
    /// Decoded parameter anchors in this tile, when the frame was produced by
    /// the shared parameter decoder rather than the raw IAMF parser.
    pub parameters: Option<Vec<SceneParameters>>,
}
