#![forbid(unsafe_code)]
#![doc = "TrueHD Atmos adapter: `truehd` access units -> the salsa spatial IR."]

mod decoder;
mod error;
mod frame;

pub use decoder::{MAX_CHANNELS, OBJECT_PRESENTATION, PCM_SCALE, SpatialSnapshot, TrueHdDecoder};
pub use error::TrueHdError;
pub use frame::{OamdPayload, TrueHdAccessUnitParser, TrueHdFrame, TrueHdParser, oamd_payloads};

pub use truehd;
