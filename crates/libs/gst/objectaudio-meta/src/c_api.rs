//! C ABI mirrors of the Dolby `DlbObjectAudioMeta` helpers.

use std::ffi::c_void;
use std::ptr;
use std::slice;

use gst::glib;
use gst::glib::translate::IntoGlib;
use gstreamer as gst;

use crate::meta::{self, ObjectAudioMeta};

/// GType of the `ObjectAudioMeta` API.
///
/// # Safety
/// Requires a GStreamer runtime and is normally called once during registration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn object_audio_meta_api_get_type() -> glib::ffi::GType {
    meta::meta_api_type().into_glib()
}

/// Registered `GstMetaInfo`; returns null only if registration failed.
///
/// # Safety
/// Requires a GStreamer runtime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn object_audio_meta_get_info() -> *const gst::ffi::GstMetaInfo {
    meta::meta_info()
}

/// Add a meta carrying an already-encoded `SpatialPayload`.
///
/// # Safety
/// `buffer` must be a valid writable `GstBuffer`; `payload` must point to
/// `size` readable bytes and stay valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn object_audio_meta_add(
    buffer: *mut gst::ffi::GstBuffer,
    payload: *const c_void,
    size: usize,
    offset: usize,
    bpf: usize,
) -> *mut ObjectAudioMeta {
    if buffer.is_null() || payload.is_null() || size == 0 || bpf == 0 {
        return ptr::null_mut();
    }
    let bytes = unsafe { slice::from_raw_parts(payload.cast::<u8>(), size) };
    let Ok(decoded) = objectaudio_ir::SpatialPayload::decode(bytes) else {
        return ptr::null_mut();
    };
    // Version 2 stores authoritative timestamps in the payload. The ABI field
    // is a nonnegative mirror for foreign callers, not another time origin.
    if offset != decoded.snapshot_offset.max(0) as usize {
        return ptr::null_mut();
    }
    unsafe {
        meta::add_raw(
            buffer,
            bytes,
            offset,
            bpf,
            u32::from(objectaudio_ir::SpatialPayload::VERSION),
            crate::PayloadType::Spatial.as_u32(),
        )
    }
}

/// Number of `ObjectAudioMeta` instances on `buffer`.
///
/// # Safety
/// `buffer` must be a valid `GstBuffer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn object_audio_meta_get_n(buffer: *mut gst::ffi::GstBuffer) -> u32 {
    if buffer.is_null() {
        return 0;
    }
    unsafe { gst::ffi::gst_buffer_get_n_meta(buffer, meta::meta_api_type().into_glib()) }
}

/// First `ObjectAudioMeta` on `buffer`.
///
/// # Safety
/// `buffer` must be a valid `GstBuffer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn object_audio_meta_get(
    buffer: *mut gst::ffi::GstBuffer,
) -> *mut ObjectAudioMeta {
    if buffer.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        gst::ffi::gst_buffer_get_meta(buffer, meta::meta_api_type().into_glib())
            as *mut ObjectAudioMeta
    }
}

/// Iterate `ObjectAudioMeta` on `buffer`, mirroring `gst_buffer_iterate_meta_filtered`.
///
/// # Safety
/// `buffer` must be a valid `GstBuffer`; `state` must point to a `gpointer`
/// initialized to null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn object_audio_meta_iterate(
    buffer: *mut gst::ffi::GstBuffer,
    state: *mut glib::ffi::gpointer,
) -> *mut ObjectAudioMeta {
    if buffer.is_null() || state.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        gst::ffi::gst_buffer_iterate_meta_filtered(buffer, state, meta::meta_api_type().into_glib())
            as *mut ObjectAudioMeta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objectaudio_ir::{ObjectState, SpatialPayload};

    #[test]
    fn c_abi_roundtrip() {
        let _ = gst::init();
        let mut buffer = gst::Buffer::with_size(64).unwrap();
        let payload = SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [0.5, 0.5, 0.0])],
            Vec::new(),
        );
        let payload = payload.with_snapshot_offset(4);
        let bytes = payload.encode().unwrap();
        let raw = buffer.get_mut().unwrap().as_mut_ptr();

        let meta = unsafe { object_audio_meta_add(raw, bytes.as_ptr().cast(), bytes.len(), 4, 2) };
        assert!(!meta.is_null());
        assert_eq!(unsafe { object_audio_meta_get_n(raw) }, 1);
        assert_eq!(unsafe { object_audio_meta_get(raw) }, meta);

        let mut state: glib::ffi::gpointer = ptr::null_mut();
        assert_eq!(unsafe { object_audio_meta_iterate(raw, &mut state) }, meta);
        assert!(unsafe { object_audio_meta_iterate(raw, &mut state) }.is_null());

        let meta = unsafe { &*meta };
        assert_eq!(meta.payload(), bytes);
        assert_eq!(meta.offset, 4);
        assert_eq!(meta.bpf, 2);
        assert_eq!(SpatialPayload::decode(meta.payload()).unwrap(), payload);
    }
}
