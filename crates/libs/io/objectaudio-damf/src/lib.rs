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
//! Trajectory handling follows `dolby-atmos-encoder`'s `PosTimeline`: an event
//! holds the previous position until its sample time, then ramps to the new
//! target over `rampLength` samples. `gain`, `active` and `size` are held
//! between keyframes. The source samples that trajectory at block boundaries
//! and emits one shared ramp per block, matching the encoder's OAMD path and
//! keeping the renderer's full-snapshot event model consistent.

mod caf;
mod error;
mod manifest;
mod metadata;
mod source;
mod yaml;

pub use caf::{CafInfo, CafReader, CafWriter, SampleFormat};
pub use error::{DamfError, Result};
pub use manifest::{BedEntry, Manifest, ManifestSpec, ObjectEntry, write_manifest};
pub use metadata::{Event as DamfEvent, Gain, Metadata};
pub use source::{
    DamfBlock, DamfInfo, DamfObjectInfo, DamfSource, ObjectTrack, TrackState, bed_position,
};
