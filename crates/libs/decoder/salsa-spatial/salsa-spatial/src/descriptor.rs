//! Signal descriptors, codec binding and configuration validation.
//!
//! This module owns the semantic model for the Opus multistream profile:
//! core-first logical channels, auxiliary observation groups, versioned signal
//! models, the private `logical_to_coded` permutation and the base fold. It is
//! deliberately independent of any container or Opus API. The codec boundary
//! resolves physical coded channels from these logical descriptors.

use crate::constants::{
    CORE_LOGICAL_COUNT, INITIAL_AUX_CHANNELS, INITIAL_CHANNEL_MAPPING, INITIAL_CODED_CHANNELS,
    INITIAL_COUPLED_COUNT, INITIAL_LOGICAL_COUNT, INITIAL_LOGICAL_TO_CODED, INITIAL_STREAM_COUNT,
    MAPPING_FAMILY, MAX_AUXILIARY_CHANNELS, MAX_CODED_CHANNELS, PUBLIC_OUTPUT_CHANNELS,
};
use crate::error::{Result, SpatialError};

/// A versioned projection model. Identifiers select a supported algorithm;
/// they never name executable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalModel {
    pub model_ref: u32,
    pub algorithm: ModelAlgorithm,
    pub algorithm_version: u16,
}

/// Supported projection algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAlgorithm {
    /// The shared 5.1.2 core panner. Version 1 has ordered rows
    /// `L, R, C, Ls, Rs, H_L, H_R`.
    SalsaCorePanner,
    /// A syntactically valid model that this decoder does not implement.
    Unknown(u8),
}

impl ModelAlgorithm {
    pub fn tag(self) -> u8 {
        match self {
            Self::SalsaCorePanner => 1,
            Self::Unknown(tag) => tag,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::SalsaCorePanner),
            0 => None,
            other => Some(Self::Unknown(other)),
        }
    }

    pub fn supports_version(self, version: u16) -> bool {
        match self {
            Self::SalsaCorePanner => version == 1,
            Self::Unknown(_) => false,
        }
    }
}

/// Ordered row identifiers of the version 1 core panner model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRow {
    L,
    R,
    C,
    Ls,
    Rs,
    Hl,
    Hr,
}

impl ModelRow {
    pub fn tag(self) -> u8 {
        match self {
            Self::L => 0,
            Self::R => 1,
            Self::C => 2,
            Self::Ls => 3,
            Self::Rs => 4,
            Self::Hl => 5,
            Self::Hr => 6,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::L),
            1 => Some(Self::R),
            2 => Some(Self::C),
            3 => Some(Self::Ls),
            4 => Some(Self::Rs),
            5 => Some(Self::Hl),
            6 => Some(Self::Hr),
            _ => None,
        }
    }

    /// Row index in the version 1 panner column.
    pub fn index(self) -> usize {
        self.tag() as usize
    }
}

/// Input domain of an observation group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDomain {
    /// The stream's ordered object table and reconstructed trajectories.
    SceneObjects,
    /// A residual against an earlier reconstruction stage.
    ObjectResidual { stage_id: u8 },
}

impl InputDomain {
    pub fn tag(self) -> u8 {
        match self {
            Self::SceneObjects => 1,
            Self::ObjectResidual { .. } => 2,
        }
    }
}

/// Transform applied to the input domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    /// Select ordered rows of a versioned model.
    ModelRows { model_ref: u32, rows: Vec<ModelRow> },
}

impl Projection {
    pub fn tag(&self) -> u8 {
        match self {
            Self::ModelRows { .. } => 1,
        }
    }
}

/// One ordered auxiliary observation group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationGroup {
    pub group_id: u32,
    /// Ordered Salsa logical channel indices.
    pub logical_channels: Vec<usize>,
    pub input: InputDomain,
    pub projection: Projection,
}

impl ObservationGroup {
    pub fn model_rows(&self) -> Option<(u32, &[ModelRow])> {
        match &self.projection {
            Projection::ModelRows { model_ref, rows } => Some((*model_ref, rows.as_slice())),
        }
    }
}

/// One reconstruction stage. Requirement is a property of the stage, not of a
/// group globally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconstructionStage {
    pub stage_id: u8,
    /// Full-range base observations in reconstruction order, as logical
    /// indices. The shared silence and LFE are never observations.
    pub base_logical: Vec<usize>,
    /// Groups that this stage requires.
    pub required_groups: Vec<u32>,
    /// Complete observation order as logical indices, base first.
    pub observation_logical: Vec<usize>,
}

/// Fixed 5-by-2 fold applied by the encoder before Opus coding:
/// `b = g + A h`, where `g` holds the five ground-plane rows and `h` the two
/// height rows. `D_base = D_ground + A * D_height`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaseFold {
    pub preset: u8,
    /// Five rows (`FL, FR, FC, SL, SR`) by two columns (`H_L, H_R`).
    pub matrix: [[f64; 2]; 5],
}

impl BaseFold {
    pub const PRESET_V1: u8 = 1;

    /// The version 1 fold preset: each height row contributes to its own front
    /// and side channels at half gain.
    pub const fn v1() -> Self {
        Self {
            preset: Self::PRESET_V1,
            matrix: [
                [0.5, 0.0], // FL <- H_L
                [0.0, 0.5], // FR <- H_R
                [0.0, 0.0], // FC
                [0.5, 0.0], // SL <- H_L
                [0.0, 0.5], // SR <- H_R
            ],
        }
    }

    /// Apply the fold to five ground rows and two height rows.
    pub fn apply(&self, ground: [f64; 5], height: [f64; 2]) -> [f64; 5] {
        std::array::from_fn(|row| {
            ground[row] + self.matrix[row][0] * height[0] + self.matrix[row][1] * height[1]
        })
    }

    /// The identity fold (A = 0) used by the legacy five-substream core.
    pub const fn none() -> Self {
        Self {
            preset: 0,
            matrix: [[0.0; 2]; 5],
        }
    }
}

/// Private codec binding at the Opus boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecBinding {
    pub output_channels: usize,
    pub mapping_family: u8,
    pub stream_count: usize,
    pub coupled_count: usize,
    /// Public family-1 speaker map for the declared output channels.
    pub channel_mapping: Vec<u8>,
    /// Private permutation: logical channel `i` is coded channel
    /// `logical_to_coded[i]`.
    pub logical_to_coded: Vec<u8>,
}

impl CodecBinding {
    /// The initial nine-coded-channel, six-substream profile.
    pub fn initial() -> Self {
        Self {
            output_channels: PUBLIC_OUTPUT_CHANNELS,
            mapping_family: MAPPING_FAMILY,
            stream_count: INITIAL_STREAM_COUNT,
            coupled_count: INITIAL_COUPLED_COUNT,
            channel_mapping: INITIAL_CHANNEL_MAPPING.to_vec(),
            logical_to_coded: INITIAL_LOGICAL_TO_CODED.to_vec(),
        }
    }

    /// The legacy five-substream core. Auxiliary observations are absent.
    pub fn legacy() -> Self {
        Self {
            output_channels: 2,
            mapping_family: 0,
            stream_count: 5,
            coupled_count: 3,
            channel_mapping: Vec::new(),
            logical_to_coded: Vec::new(),
        }
    }

    pub fn is_legacy(&self) -> bool {
        self.stream_count == 5 && self.coupled_count == 3 && self.logical_to_coded.is_empty()
    }

    /// Number of coded channels reconstructed by the substreams.
    pub fn coded_channel_count(&self) -> usize {
        self.stream_count + self.coupled_count
    }

    /// Number of auxiliary channels carried after the six core logical signals.
    pub fn auxiliary_channel_count(&self) -> usize {
        self.logical_to_coded
            .len()
            .saturating_sub(CORE_LOGICAL_COUNT + 1)
    }

    pub fn logical_channel_count(&self) -> usize {
        self.logical_to_coded.len()
    }

    pub fn silence_logical_index(&self) -> usize {
        self.logical_channel_count().saturating_sub(1)
    }

    /// Derive the public family-1 speaker map from the private binding:
    /// `[T[0], T[2], T[1], T[4], T[5], T[Z], T[Z], T[3]]`.
    pub fn derive_public_mapping(&self) -> Result<Vec<u8>> {
        if self.is_legacy() {
            return Err(SpatialError::InvalidValue(
                "legacy binding has no multistream public map",
            ));
        }
        let t = &self.logical_to_coded;
        if t.len() < CORE_LOGICAL_COUNT {
            return Err(SpatialError::InvalidValue(
                "logical_to_coded is shorter than the core",
            ));
        }
        let z = self.silence_logical_index();
        let silence = *t
            .get(z)
            .ok_or(SpatialError::InvalidValue("missing silence binding"))?;
        Ok(vec![t[0], t[2], t[1], t[4], t[5], silence, silence, t[3]])
    }

    /// Coded channel that carries the mono center substream (output-map entry 1).
    pub fn center_coded_channel(&self) -> Result<usize> {
        self.channel_mapping
            .get(1)
            .map(|value| *value as usize)
            .ok_or(SpatialError::InvalidValue("missing center output mapping"))
    }

    /// Substream index of the metadata carrier. Coupled substreams occupy coded
    /// indices `0..2*M`; mono substream `j` occupies `2*M + j`.
    pub fn center_substream_index(&self) -> Result<usize> {
        if self.is_legacy() {
            // Legacy five-stream order: front, surround, height, center, lfe.
            return Ok(3);
        }
        let center = self.center_coded_channel()?;
        let mono_start = 2 * self.coupled_count;
        if center < mono_start || center >= self.coded_channel_count() {
            return Err(SpatialError::InvalidValue(
                "center channel is not a mono substream",
            ));
        }
        Ok(center - self.coupled_count)
    }

    /// Map coded index back to logical index, if the coded channel is bound.
    pub fn logical_for_coded(&self, coded: usize) -> Option<usize> {
        self.logical_to_coded
            .iter()
            .position(|value| *value as usize == coded)
    }

    /// Validate the binding against the standard Opus header and the profile.
    pub fn validate(&self) -> Result<()> {
        if self.is_legacy() {
            return Ok(());
        }
        if self.mapping_family != MAPPING_FAMILY {
            return Err(SpatialError::InvalidValue("mapping family must be 1"));
        }
        if self.output_channels != PUBLIC_OUTPUT_CHANNELS {
            return Err(SpatialError::InvalidValue(
                "initial profile declares eight output channels",
            ));
        }
        if self.channel_mapping.len() != self.output_channels {
            return Err(SpatialError::InvalidValue(
                "public channel map length does not match output channels",
            ));
        }
        if self.stream_count == 0 || self.stream_count < self.coupled_count {
            return Err(SpatialError::InvalidValue("invalid Opus stream counts"));
        }
        let coded = self.coded_channel_count();
        if coded > MAX_CODED_CHANNELS {
            return Err(SpatialError::InvalidValue(
                "coded channels exceed the Opus limit",
            ));
        }
        if self.logical_to_coded.len() < CORE_LOGICAL_COUNT + 1 {
            return Err(SpatialError::InvalidValue(
                "logical_to_coded must include the core and shared silence",
            ));
        }
        if self.auxiliary_channel_count() > MAX_AUXILIARY_CHANNELS {
            return Err(SpatialError::InvalidValue(
                "auxiliary capacity exceeds the profile limit",
            ));
        }
        if self.logical_to_coded.len() != coded {
            return Err(SpatialError::InvalidValue(
                "logical_to_coded length must equal the coded channel count",
            ));
        }
        let mut seen = vec![false; coded];
        for &value in &self.logical_to_coded {
            let index = value as usize;
            if index >= coded || seen[index] {
                return Err(SpatialError::InvalidValue(
                    "logical_to_coded is not a permutation",
                ));
            }
            seen[index] = true;
        }
        let mut public_seen = vec![false; coded];
        for &value in &self.channel_mapping {
            let index = value as usize;
            if index >= coded {
                return Err(SpatialError::InvalidValue(
                    "public channel map references an unknown coded channel",
                ));
            }
            public_seen[index] = true;
        }
        // Auxiliary coded channels must be hidden from ordinary output. With
        // the initial two-channel group these are coded 4 and 5.
        let aux_count = self.auxiliary_channel_count();
        for logical in CORE_LOGICAL_COUNT..CORE_LOGICAL_COUNT + aux_count {
            let coded_index = self.logical_to_coded[logical] as usize;
            if public_seen.get(coded_index).copied().unwrap_or(false) {
                return Err(SpatialError::InvalidValue(
                    "auxiliary coded channel appears in the public map",
                ));
            }
        }
        if self.derive_public_mapping()? != self.channel_mapping {
            return Err(SpatialError::InvalidValue(
                "public channel map disagrees with the derived map",
            ));
        }
        let silence = self.silence_logical_index();
        if self.channel_mapping[5] != self.logical_to_coded[silence]
            || self.channel_mapping[6] != self.logical_to_coded[silence]
        {
            return Err(SpatialError::InvalidValue(
                "rear outputs must share the silence channel",
            ));
        }
        Ok(())
    }

    /// Number of coupled auxiliary pairs implied by the coded layout.
    pub fn auxiliary_coupled_pairs(&self) -> usize {
        let aux = self.auxiliary_channel_count();
        let pairs = self.coupled_count.saturating_sub(2);
        aux.min(2 * pairs) / 2
    }
}

/// Validate the complete descriptor set. This is the section 5.6 checklist.
pub fn validate_configuration(
    binding: &CodecBinding,
    signal_models: &[SignalModel],
    groups: &[ObservationGroup],
    stages: &[ReconstructionStage],
    fold: Option<&BaseFold>,
) -> Result<()> {
    binding.validate()?;
    if binding.is_legacy() {
        return Ok(());
    }

    // 1. Unique model and group identifiers.
    let mut model_ids = Vec::new();
    for model in signal_models {
        if model_ids.contains(&model.model_ref) {
            return Err(SpatialError::InvalidValue("duplicate signal model id"));
        }
        model_ids.push(model.model_ref);
    }
    let mut group_ids = Vec::new();
    for group in groups {
        if group_ids.contains(&group.group_id) {
            return Err(SpatialError::InvalidValue("duplicate observation group id"));
        }
        group_ids.push(group.group_id);
    }

    // 3. Input domains supported by the selected stage, plus 7. bindings.
    let mut bound_aux = vec![false; binding.auxiliary_channel_count()];
    for group in groups {
        match group.input {
            InputDomain::SceneObjects => {}
            InputDomain::ObjectResidual { stage_id } => {
                if !stages.iter().any(|stage| stage.stage_id == stage_id) {
                    return Err(SpatialError::InvalidValue(
                        "residual input references an unknown stage",
                    ));
                }
            }
        }
        let Some((model_ref, rows)) = group.model_rows() else {
            return Err(SpatialError::InvalidValue("unsupported projection"));
        };
        let _model = signal_models
            .iter()
            .find(|model| model.model_ref == model_ref)
            .ok_or(SpatialError::InvalidValue(
                "model reference does not resolve",
            ))?;
        if rows.len() != group.logical_channels.len() {
            return Err(SpatialError::InvalidValue(
                "projection row count disagrees with the binding",
            ));
        }
        for logical in &group.logical_channels {
            let aux_index = logical
                .checked_sub(CORE_LOGICAL_COUNT)
                .filter(|index| *index < binding.auxiliary_channel_count())
                .ok_or(SpatialError::InvalidValue(
                    "auxiliary binding is outside the auxiliary range",
                ))?;
            if bound_aux[aux_index] {
                return Err(SpatialError::InvalidValue(
                    "auxiliary channel belongs to more than one group",
                ));
            }
            bound_aux[aux_index] = true;
        }
    }
    if bound_aux.iter().any(|bound| !bound) {
        return Err(SpatialError::InvalidValue(
            "an auxiliary logical channel has no group",
        ));
    }

    // 8. The public map is checked by CodecBinding::validate.

    // 9. Required stage inputs are present and have a defined observation order.
    for stage in stages {
        for &logical in &stage.observation_logical {
            if logical >= binding.silence_logical_index() {
                return Err(SpatialError::InvalidValue(
                    "observation refers to the silence channel",
                ));
            }
        }
        if stage.base_logical.len() != 5 {
            return Err(SpatialError::InvalidValue(
                "the base observation set must have five full-range rows",
            ));
        }
        for &group_id in &stage.required_groups {
            if !group_ids.contains(&group_id) {
                return Err(SpatialError::InvalidValue(
                    "reconstruction stage requires a missing group",
                ));
            }
        }
    }

    // 10. The fold is a finite 5-by-2 matrix.
    if let Some(fold) = fold {
        for row in fold.matrix {
            for value in row {
                if !value.is_finite() {
                    return Err(SpatialError::InvalidValue("fold matrix is not finite"));
                }
            }
        }
    }
    Ok(())
}

/// The initial descriptor set from sections 5.4 through 5.5.
pub fn initial_models() -> Vec<SignalModel> {
    vec![SignalModel {
        model_ref: 1,
        algorithm: ModelAlgorithm::SalsaCorePanner,
        algorithm_version: 1,
    }]
}

pub fn initial_groups(binding: &CodecBinding) -> Vec<ObservationGroup> {
    if binding.is_legacy() || binding.auxiliary_channel_count() == 0 {
        return Vec::new();
    }
    let aux = binding.auxiliary_channel_count().min(INITIAL_AUX_CHANNELS);
    let logical: Vec<usize> = (CORE_LOGICAL_COUNT..CORE_LOGICAL_COUNT + aux).collect();
    let rows = if aux == 2 {
        vec![ModelRow::Hl, ModelRow::Hr]
    } else {
        vec![ModelRow::Hl]
    };
    vec![ObservationGroup {
        group_id: 1,
        logical_channels: logical,
        input: InputDomain::SceneObjects,
        projection: Projection::ModelRows { model_ref: 1, rows },
    }]
}

pub fn initial_stage(binding: &CodecBinding) -> Vec<ReconstructionStage> {
    if binding.is_legacy() {
        return Vec::new();
    }
    let groups = initial_groups(binding);
    let mut base = vec![0, 1, 2, 4, 5];
    let mut observation = base.clone();
    let mut required = Vec::new();
    for group in &groups {
        observation.extend_from_slice(&group.logical_channels);
        required.push(group.group_id);
    }
    base.shrink_to_fit();
    vec![ReconstructionStage {
        stage_id: 1,
        base_logical: base,
        required_groups: required,
        observation_logical: observation,
    }]
}

/// Logical indices in the reconstruction observation order for the active
/// stage: `FL, FR, FC, SL, SR, H_L, H_R`. `None` for the legacy layout.
pub fn observation_logical_indices(binding: &CodecBinding) -> Option<Vec<usize>> {
    if binding.is_legacy() {
        return None;
    }
    let stage = initial_stage(binding).into_iter().next()?;
    Some(stage.observation_logical)
}

/// Initial descriptor-capacity test: `K = 2`.
pub fn initial_logical_count() -> usize {
    INITIAL_LOGICAL_COUNT
}

/// Validate that a reconstructed configuration has a complete initial layout.
pub fn is_initial_profile(binding: &CodecBinding) -> bool {
    binding.coded_channel_count() == INITIAL_CODED_CHANNELS
        && binding.logical_channel_count() == INITIAL_LOGICAL_COUNT
        && binding.stream_count == INITIAL_STREAM_COUNT
        && binding.coupled_count == INITIAL_COUPLED_COUNT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_binding_matches_the_proposal() {
        let binding = CodecBinding::initial();
        binding.validate().unwrap();
        assert_eq!(binding.coded_channel_count(), 9);
        assert_eq!(binding.auxiliary_channel_count(), 2);
        assert_eq!(
            binding.derive_public_mapping().unwrap(),
            vec![0, 6, 1, 2, 3, 8, 8, 7]
        );
        assert_eq!(binding.center_substream_index().unwrap(), 3);
        assert_eq!(
            observation_logical_indices(&binding).unwrap(),
            vec![0, 1, 2, 4, 5, 6, 7]
        );
    }

    #[test]
    fn duplicated_silence_is_required() {
        let mut binding = CodecBinding::initial();
        binding.channel_mapping = vec![0, 6, 1, 2, 3, 7, 7, 8];
        assert!(binding.validate().is_err());
    }

    #[test]
    fn permutation_and_hidden_channels_are_checked() {
        let mut binding = CodecBinding::initial();
        binding.logical_to_coded = vec![0, 1, 2, 3, 4, 5, 6, 7, 6];
        assert!(binding.validate().is_err());

        let mut binding = CodecBinding::initial();
        binding.channel_mapping[2] = 4; // expose the auxiliary pair
        assert!(binding.validate().is_err());
    }

    #[test]
    fn fold_preset_is_finite_and_adds_height_energy() {
        let fold = BaseFold::v1();
        let base = fold.apply([0.0; 5], [1.0, 0.0]);
        assert!(base[0] > 0.0 || base[3] > 0.0);
        let config = crate::frame::SalsaSpatialStreamConfig::default();
        config.validate().unwrap();
    }

    #[test]
    fn unknown_model_version_disables_enhancement_without_rejection() {
        let mut config = crate::frame::SalsaSpatialStreamConfig::default();
        config.signal_models[0].algorithm_version = 2;
        config.validate().unwrap();
        assert!(!config.enhanced_supported());

        config.signal_models[0].algorithm = ModelAlgorithm::Unknown(9);
        config.validate().unwrap();
        assert!(!config.enhanced_supported());
    }

    #[test]
    fn group_and_row_counts_must_agree() {
        let binding = CodecBinding::initial();
        let mut groups = initial_groups(&binding);
        groups[0].projection = Projection::ModelRows {
            model_ref: 1,
            rows: vec![ModelRow::Hl],
        };
        assert!(
            validate_configuration(
                &binding,
                &initial_models(),
                &groups,
                &initial_stage(&binding),
                Some(&BaseFold::v1())
            )
            .is_err()
        );
    }
}
