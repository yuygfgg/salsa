use std::ffi::c_void;
use std::ptr;

use gstreamer as gst;
use gstreamer::glib;
use objectaudio_ir::{ObjectState, SpatialPayload};
use objectaudio_meta::{
    PayloadSource, add_spatial, dlb_available, dlb_payload, object_audio_payload,
};

#[repr(C)]
struct FakeDlbMeta {
    meta: gst::ffi::GstMeta,
    payload: *mut c_void,
    size: usize,
    offset: usize,
    bpf: usize,
}

unsafe extern "C" fn init_meta(
    meta: *mut gst::ffi::GstMeta,
    _params: glib::ffi::gpointer,
    _buffer: *mut gst::ffi::GstBuffer,
) -> glib::ffi::gboolean {
    let meta = unsafe { &mut *(meta as *mut FakeDlbMeta) };
    meta.payload = ptr::null_mut();
    meta.size = 0;
    meta.offset = 0;
    meta.bpf = 1;
    glib::ffi::GTRUE
}

unsafe extern "C" fn free_meta(meta: *mut gst::ffi::GstMeta, _buffer: *mut gst::ffi::GstBuffer) {
    let meta = unsafe { &mut *(meta as *mut FakeDlbMeta) };
    if !meta.payload.is_null() {
        unsafe { glib::ffi::g_free(meta.payload) };
    }
}

fn dlb_info() -> *const gst::ffi::GstMetaInfo {
    let existing = unsafe { gst::ffi::gst_meta_get_info(c"DlbObjectAudioMeta".as_ptr()) };
    if !existing.is_null() {
        return existing;
    }

    unsafe {
        let tags: [*const std::ffi::c_char; 1] = [ptr::null()];
        let api = gst::ffi::gst_meta_api_type_register(
            c"DlbObjectAudioMetaAPI".as_ptr(),
            tags.as_ptr().cast_mut(),
        );
        let info = gst::ffi::gst_meta_register(
            api,
            c"DlbObjectAudioMeta".as_ptr(),
            std::mem::size_of::<FakeDlbMeta>(),
            Some(init_meta),
            Some(free_meta),
            None,
        );
        assert!(!info.is_null());
        info
    }
}

#[test]
fn reads_dlb_payload_without_registering_it_here() {
    let _ = gst::init();
    let info = dlb_info();
    let mut buffer = gst::Buffer::with_size(1024).unwrap();

    let bytes = b"oamd-from-dolby";
    unsafe {
        let raw = gst::ffi::gst_buffer_add_meta(
            buffer.get_mut().unwrap().as_mut_ptr(),
            info,
            ptr::null_mut(),
        ) as *mut FakeDlbMeta;
        assert!(!raw.is_null());

        let data = glib::ffi::g_malloc(bytes.len());
        assert!(!data.is_null());
        ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());

        (*raw).payload = data;
        (*raw).size = bytes.len();
        (*raw).offset = 96;
        (*raw).bpf = 8;
    }

    assert!(dlb_available());
    let dlb = dlb_payload(&buffer).unwrap();
    assert_eq!(dlb.source, PayloadSource::DlbObjectAudio);
    assert_eq!(dlb.bytes, bytes);
    assert_eq!(dlb.offset, 96);
    assert_eq!(dlb.bpf, 8);
}

#[test]
fn salsa_meta_is_independent_of_dolby() {
    let _ = gst::init();
    let mut buffer = gst::Buffer::with_size(1024).unwrap();
    let payload = SpatialPayload::new(
        48_000,
        vec![ObjectState::new(1, [0.5, 0.5, 0.0])],
        Vec::new(),
    );
    add_spatial(buffer.get_mut().unwrap(), &payload, 4).unwrap();

    let borrowed = object_audio_payload(&buffer).unwrap();
    assert_eq!(borrowed.source, PayloadSource::ObjectAudio);
    assert_eq!(borrowed.offset, 0);
    assert_eq!(borrowed.bpf, 4);
}
