#![forbid(unsafe_code)]
#![doc = "Salsa spatial audio codec syntax, parameter state and IAMF framing."]

pub mod bands;
pub mod constants;
pub mod entropy;
pub mod error;
pub mod frame;
pub mod iamf;
pub mod opus_ext;
pub mod params;
pub mod parser;
pub mod snapshot;

pub use bands::{bin_weight, parameter_band_for_bin, parameter_band_range};
pub use constants::*;
pub use entropy::{AdaptiveModel, EntropyContexts, EntropyDecoder, EntropyEncoder};
pub use error::{Result, SpatialError};
pub use frame::{CorePacket, EnhancementTile, SalsaSpatialFrame, SalsaSpatialStreamConfig};
pub use iamf::{IamfReader, IamfWriter};
pub use params::{
    BandCorrection, CoreErrorEnvelope, CorrelationPair, CovarianceAnchor, DctBasis,
    ObjectPowerState, PowerModel, SceneParameters, TrajectoryAnchor, TrajectoryKnot,
};
pub use parser::SalsaSpatialParser;
pub use snapshot::SceneSnapshot;

/// Codec name used by reports and configuration.
pub const CODEC_NAME: &str = "salsa-spatial";
/// Experiment format version carried by enhancement framing.
pub const ENHANCEMENT_FORMAT_VERSION: u16 = 1;
/// Identifier used by the experimental Opus extension.
pub const OPUS_EXTENSION_EXPERIMENT_ID: u32 = 0x5341_4C53; // "SALS"
