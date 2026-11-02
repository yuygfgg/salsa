#![forbid(unsafe_code)]
#![doc = "Format-independent spatial audio IR used by the salsa decoders and renderers."]

mod conversion;
mod error;
mod frame;
mod object;
mod payload;
mod pcm;
mod pipeline;
mod speaker;
mod timeline;

pub use conversion::ConversionReport;
pub use error::{IrError, PayloadError};
pub use frame::SpatialFrame;
pub use object::{ObjectEvent, ObjectState, Transition};
pub use payload::{Snapshot, SpatialPayload, payload_version};
pub use pcm::PcmBuffer;
pub use pipeline::{SpatialDecoder, SpatialParser};
pub use speaker::{
    BED_CHANNEL_COUNT, BED_CHANNELS, STANDARD_LAYOUTS, SpeakerPosition, StandardLayout, adm_to_q15,
    adm_to_q15_f64, layout_name_for, q15_to_adm,
};
pub use timeline::{SceneTimeline, StateTrack, align_sample};

/// The control grid shared by metadata writers, decoders and renderers.
pub const METADATA_BLOCK_SAMPLES: usize = 32;
