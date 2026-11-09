//! Parsed frame types for one codec tile and one Opus multistream packet.

use objectaudio_ir::ObjectState;

use crate::descriptor::{
    BaseFold, CodecBinding, CorePannerConfig, ModelAlgorithm, ObservationGroup,
    ReconstructionStage, SignalModel, validate_configuration,
};
use crate::error::{Result, SpatialError};
use crate::params::{SceneParameters, TrajectoryKnot};
use crate::snapshot::SceneSnapshot;

/// One Opus multistream packet plus the elementary substream packets it was
/// assembled from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MultistreamPacket {
    /// Elementary Opus packets in physical substream order.
    pub substreams: Vec<Vec<u8>>,
}

impl MultistreamPacket {
    pub fn new(substreams: Vec<Vec<u8>>) -> Self {
        Self { substreams }
    }

    /// Legacy named constructor: `front, surround, height, center, lfe`.
    pub fn from_legacy(
        front: Vec<u8>,
        surround: Vec<u8>,
        height: Vec<u8>,
        center: Vec<u8>,
        lfe: Vec<u8>,
    ) -> Self {
        Self {
            substreams: vec![front, surround, height, center, lfe],
        }
    }

    pub fn stream(&self, index: usize) -> Option<&[u8]> {
        self.substreams.get(index).map(Vec::as_slice)
    }

    pub fn stream_mut(&mut self, index: usize) -> Option<&mut Vec<u8>> {
        self.substreams.get_mut(index)
    }

    pub fn len(&self) -> usize {
        self.substreams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.substreams.is_empty()
    }

    pub fn front(&self) -> &[u8] {
        self.stream(0).unwrap_or_default()
    }

    pub fn surround(&self) -> &[u8] {
        self.stream(1).unwrap_or_default()
    }

    pub fn height(&self) -> &[u8] {
        self.stream(2).unwrap_or_default()
    }

    pub fn center(&self) -> &[u8] {
        self.stream(3).unwrap_or_default()
    }

    pub fn lfe(&self) -> &[u8] {
        self.stream(4).unwrap_or_default()
    }
}

/// Retained name for the ordered packet collection.
pub type CorePacket = MultistreamPacket;

/// One enhancement record covering a 100 ms tile.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EnhancementTile {
    pub start_sample: u64,
    pub frames: usize,
    pub bytes: Vec<u8>,
}

/// Complete stream configuration carried by the private records.
#[derive(Debug, Clone, PartialEq)]
pub struct SalsaSpatialStreamConfig {
    pub format_version: u16,
    pub profile: u8,
    pub config_id: u32,
    pub object_count: usize,
    pub headroom_gain: f64,
    pub initial_objects: Vec<ObjectState>,
    pub initial_trajectories: Vec<Vec<TrajectoryKnot>>,
    /// Core bitrates indexed by substream role:
    /// front, surround, auxiliary, center, LFE.
    pub core_bitrates: [u32; 5],
    pub beta: f64,
    pub codec: CodecBinding,
    pub signal_models: Vec<SignalModel>,
    pub observation_groups: Vec<ObservationGroup>,
    pub base_fold: Option<BaseFold>,
    pub core_panner: CorePannerConfig,
    pub reconstruction: Vec<ReconstructionStage>,
}

impl Default for SalsaSpatialStreamConfig {
    fn default() -> Self {
        let codec = CodecBinding::initial();
        Self {
            format_version: 4,
            profile: 1,
            config_id: 1,
            object_count: 0,
            headroom_gain: 1.0,
            initial_objects: Vec::new(),
            initial_trajectories: Vec::new(),
            core_bitrates: crate::constants::INITIAL_CORE_BITRATES,
            beta: crate::constants::DEFAULT_BETA,
            signal_models: crate::descriptor::initial_models(),
            observation_groups: crate::descriptor::initial_groups(&codec),
            base_fold: Some(BaseFold::v1()),
            core_panner: CorePannerConfig::default(),
            reconstruction: crate::descriptor::initial_stage(&codec),
            codec,
        }
    }
}

impl SalsaSpatialStreamConfig {
    /// Build a legacy five-substream configuration.
    pub fn legacy() -> Self {
        Self {
            codec: CodecBinding::legacy(),
            signal_models: Vec::new(),
            observation_groups: Vec::new(),
            base_fold: None,
            reconstruction: Vec::new(),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.format_version != 4 {
            return Err(SpatialError::UnsupportedVersion {
                version: self.format_version,
            });
        }
        if self.core_panner.algorithm_version() == 2
            && self.signal_models.iter().any(|model| {
                model.algorithm == ModelAlgorithm::SalsaCorePanner && model.algorithm_version == 1
            })
        {
            return Err(SpatialError::InvalidValue(
                "coding-angle compression requires core-panner model version 2",
            ));
        }
        validate_configuration(
            &self.codec,
            &self.signal_models,
            &self.observation_groups,
            &self.reconstruction,
            self.base_fold.as_ref(),
        )
    }

    /// Observation order as logical indices. Legacy returns the old fixed order.
    pub fn observation_logical(&self) -> Vec<usize> {
        if let Some(stage) = self.reconstruction.first() {
            return stage.observation_logical.clone();
        }
        if self.codec.is_legacy() {
            return vec![0, 1, 2, 3, 4, 5, 6];
        }
        vec![0, 1, 2, 4, 5, 6, 7]
    }

    pub fn fold(&self) -> BaseFold {
        self.base_fold.unwrap_or_else(BaseFold::none)
    }

    /// Whether this build can run the declared required reconstruction stages.
    ///
    /// An unknown model version never substitutes a similarly named panner; it
    /// selects ordinary base playback instead.
    pub fn enhanced_supported(&self) -> bool {
        if self.codec.is_legacy() {
            return true;
        }
        if self.reconstruction.is_empty() {
            return false;
        }
        for stage in &self.reconstruction {
            for group_id in &stage.required_groups {
                let Some(group) = self
                    .observation_groups
                    .iter()
                    .find(|group| group.group_id == *group_id)
                else {
                    return false;
                };
                let Some((model_ref, _)) = group.model_rows() else {
                    return false;
                };
                let Some(model) = self
                    .signal_models
                    .iter()
                    .find(|model| model.model_ref == model_ref)
                else {
                    return false;
                };
                if !model.algorithm.supports_version(model.algorithm_version) {
                    return false;
                }
            }
        }
        true
    }
}

/// A complete codec tile as consumed by the DSP decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct SalsaSpatialFrame {
    pub sample_rate: u32,
    pub start_sample: u64,
    pub frames: usize,
    /// One multistream packet per 20 ms interval.
    pub packets: Vec<MultistreamPacket>,
    pub enhancement: EnhancementTile,
    /// Complete predictor and trajectory state immediately before this tile.
    pub checkpoint: Option<SceneSnapshot>,
    /// Decoded parameter anchors in this tile, when the frame was produced by
    /// the shared parameter decoder rather than the raw parser.
    pub parameters: Option<Vec<SceneParameters>>,
}

/// Ensure a frame's packet count agrees with its declared duration.
pub fn validate_frame_shape(frame: &SalsaSpatialFrame, stream_count: usize) -> Result<()> {
    for packet in &frame.packets {
        if packet.len() != stream_count {
            return Err(SpatialError::InvalidValue(
                "packet substream count disagrees with the stream configuration",
            ));
        }
    }
    Ok(())
}
