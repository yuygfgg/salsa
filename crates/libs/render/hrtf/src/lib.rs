#![forbid(unsafe_code)]
#![doc = "Rosella personalized-headphone binaural renderer."]
// Indexed loops keep the summation order and table layout stable, and the
// generated coefficient tables are intentionally `const`.
#![allow(
    clippy::large_const_arrays,
    clippy::manual_memcpy,
    clippy::needless_range_loop
)]

mod error;
mod filterbank;
mod model;
mod renderer;
mod tables_public;
mod tables_rosella;
mod timeline;

pub use error::HrtfError;
pub use filterbank::{
    Filterbank, FilterbankKind, HYBRID_BANDS, PUBLIC_LATENCY, QMF_BANDS, QMF_HOP, ROSELLA_LATENCY,
    RosellaFilterbank, create,
};
pub use model::{
    RosellaCaptureMetadata, RosellaDistanceProfile, RosellaModel, load_personalized_headphone,
};
pub use renderer::{
    BLOCK_SAMPLES, DIRECTION_TERMS, FRAME_SAMPLES, OUTPUT_CHANNELS, RosellaProfile,
    RosellaRenderOptions, RosellaRenderer,
};
pub use timeline::{OamdPositionTimeline, align_metadata_sample};
