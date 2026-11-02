#![forbid(unsafe_code)]
#![doc = "Dolby Atmos Master Format (DAMF) reader for the salsa object-audio IR."]
//!
//! The reader turns a DAMF presentation (`.atmos` manifest, `.atmos.audio`
//! CAF essence and `.atmos.metadata` keyframes) into salsa `SpatialPayload`
//! blocks with `ch0 = LFE` and `ch1..N = objects`. Non-LFE bed channels are
//! expanded into fixed-position objects before they reach a renderer.
//!
//! The crate also provides `CafWriter`, `Metadata::save` and
//! `ManifestSpec`/`write_manifest` for producing DAMF masters.
//!
//! DAMF patches are resolved once into shared scene tracks. Buffer reads keep
//! event boundaries and unfinished ramps; they do not resample the trajectory.

mod caf;
mod error;
mod export;
mod manifest;
mod metadata;
mod scene;
mod source;
mod yaml;

pub use caf::{CafInfo, CafReader, CafWriter, SampleFormat};
pub use error::{DamfError, Result};
pub use manifest::{BedEntry, Manifest, ManifestSpec, ObjectEntry, write_manifest};
pub use metadata::{Event as DamfEvent, Gain, Metadata};
pub use source::{DamfBlock, DamfInfo, DamfObjectInfo, DamfSource, bed_position};

pub use scene::{
    Consumer, ConversionReport, DamfScene, ObjectTrack, ResolvedMetadata, ResolvedObject,
};
