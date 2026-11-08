#![forbid(unsafe_code)]
#![doc = "Salsa spatial audio codec syntax, Opus multistream framing and parameter state."]

pub mod bands;
pub mod constants;
pub mod descriptor;
pub mod entropy;
pub mod error;
pub mod frame;
pub mod multistream;
pub mod ogg;
pub mod opus_ext;
pub mod params;
pub mod record;
pub mod snapshot;
pub mod stream;

pub use bands::{bin_weight, parameter_band_for_bin, parameter_band_range};
pub use constants::*;
pub use descriptor::{
    BaseFold, CodecBinding, CorePannerConfig, InputDomain, ModelAlgorithm, ModelRow,
    ObservationGroup, Projection, ReconstructionStage, SignalModel,
};
pub use entropy::{AdaptiveModel, EntropyContexts, EntropyDecoder, EntropyEncoder};
pub use error::{Result, SpatialError};
pub use frame::{
    CorePacket, EnhancementTile, MultistreamPacket, SalsaSpatialFrame, SalsaSpatialStreamConfig,
};
pub use multistream::{pack_owned as pack_multistream, split as split_multistream};
pub use ogg::{OggOpusReader, OggOpusWriter, OpusHead};
pub use params::{
    BandCorrection, CoreErrorEnvelope, CorrelationPair, CovarianceAnchor, DctBasis,
    ObjectPowerState, PowerModel, SceneParameters, TrajectoryAnchor, TrajectoryKnot,
};
pub use record::{
    Fragment, GroupActivity, Reassembler, Record, TileRecord, decode_configuration,
    encode_configuration, parse_records, write_record,
};
pub use snapshot::SceneSnapshot;
pub use stream::{MultistreamReader, MultistreamWriter, OggSalsaWriter, SalsaSpatialParser};

/// Codec name used by reports and configuration.
pub const CODEC_NAME: &str = "salsa-spatial";
/// Experiment format version carried by enhancement framing.
pub const ENHANCEMENT_FORMAT_VERSION: u16 = 1;
/// Identifier used by the experimental Opus extension.
pub const OPUS_EXTENSION_EXPERIMENT_ID: u32 = 0x5341_4C53; // "SALS"
