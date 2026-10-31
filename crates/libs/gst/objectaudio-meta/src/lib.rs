#![doc = "GStreamer meta and caps support for salsa object audio."]

pub mod c_api;
mod caps;
mod dlb;
mod meta;

pub use caps::{
    CAPS_FEATURE, DLB_CAPS_FEATURE, caps_features, ensure_feature, has_feature, sink_feature_sets,
};
pub use dlb::{
    BorrowedPayload, PayloadSource, add_dlb_payload, dlb_available, dlb_payload, dlb_payloads,
    object_audio_available, object_audio_payload, object_audio_payloads,
};
pub use meta::{
    ObjectAudioMeta, PayloadType, SpatialSnapshot, add_spatial, add_spatial_bytes, get,
    get_spatial, iter, iter_spatial, meta_api_type, register, remove_all, scale_offset,
};
pub use objectaudio_ir::{PayloadError, SpatialPayload};

/// Error returned by the safe meta helpers.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MetaError {
    #[error("payload must not be empty")]
    EmptyPayload,

    #[error("bytes per frame must be non-zero")]
    InvalidBpf,

    #[error("GStreamer refused to add ObjectAudioMeta")]
    AddFailed,

    #[error("DlbObjectAudioMeta is not registered; load a Dolby plugin first")]
    DlbNotRegistered,

    #[error("invalid spatial payload: {0}")]
    Payload(#[from] PayloadError),
}

/// Initialize GStreamer and register the meta type.
pub fn init() -> Result<(), gstreamer::glib::Error> {
    gstreamer::init()?;
    meta::register();
    Ok(())
}
