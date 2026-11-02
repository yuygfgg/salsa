#![forbid(unsafe_code)]
#![doc = "E-AC-3 JOC parse stage: framing, EMDF, OAMD and ID14."]

mod bed;
mod bit;
mod emdf;
mod error;
mod frame;
pub mod huffman_tables;
mod joc;
mod joc_bands;
mod transport;

pub use bed::{
    BedPlan, Position, acmod_channel_count, acmod_positions, bed_channels_for_core,
    chanmap_extra_pair, compute_bed_plan, compute_bed_plan_for_config, expand_chanmap,
    expected_pair, is_core_stream, position_bit, position_name,
};
pub use error::{Eac3JocError, Result};
pub use frame::{Eac3JocFrame, Eac3JocParser};
pub use joc::{
    CORE_CHANNELS, FRAME_SAMPLES, JocObject, JocParams, MAX_CORE_CHANNELS, MAX_DPOINTS,
    MAX_OBJECTS, MAX_PARAMETER_BANDS, check_id14_padding, num_bands_for_index,
    num_channels_for_config, parse_id14, parse_syncframe, parse_syncframe_optional,
    score_container_with_frame,
};
pub use joc_bands::parameter_band_map;
pub use transport::{
    Codec, FrameReader, SYNCWORD, SyncFrame, SyncFrameHeader, parse_syncframe_header,
};
