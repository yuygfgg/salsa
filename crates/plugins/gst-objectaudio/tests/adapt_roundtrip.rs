//! End-to-end checks for both `salsaobjectaudioadapt` directions.
//!
//! A minimal fake `DlbObjectAudioMeta` type stands in for the Reference Player
//! when it is not installed. It uses the same C layout as Dolby's meta.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::glib::translate::IntoGlib;
use objectaudio_ir::{ObjectEvent, ObjectState, SpatialPayload};
use objectaudio_meta::{add_spatial, get_spatial};

use gstsalsaobjectaudio::ObjectAudioAdapt;

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

unsafe extern "C" fn transform_meta(
    dest: *mut gst::ffi::GstBuffer,
    meta: *mut gst::ffi::GstMeta,
    _buffer: *mut gst::ffi::GstBuffer,
    type_: glib::ffi::GQuark,
    data: glib::ffi::gpointer,
) -> glib::ffi::gboolean {
    let copy_quark = glib::Quark::from_str("gst-copy").into_glib();
    if type_ != copy_quark || data.is_null() {
        return glib::ffi::GTRUE;
    }
    let source = unsafe { &*(meta as *const FakeDlbMeta) };
    let copy = unsafe { &*(data as *const gst::ffi::GstMetaTransformCopy) };
    let info = unsafe { gst::ffi::gst_meta_get_info(c"DlbObjectAudioMeta".as_ptr()) };
    if info.is_null() {
        return glib::ffi::GFALSE;
    }
    let dest_meta =
        unsafe { gst::ffi::gst_buffer_add_meta(dest, info, ptr::null_mut()) as *mut FakeDlbMeta };
    if dest_meta.is_null() {
        return glib::ffi::GFALSE;
    }
    let data_ptr = if source.payload.is_null() || source.size == 0 {
        ptr::null_mut()
    } else {
        let copy_ptr = unsafe { glib::ffi::g_malloc(source.size) };
        unsafe { ptr::copy_nonoverlapping(source.payload, copy_ptr, source.size) };
        copy_ptr
    };
    unsafe {
        (*dest_meta).payload = data_ptr;
        (*dest_meta).size = source.size;
        (*dest_meta).offset = if copy.region == 0 {
            source.offset
        } else {
            source
                .offset
                .saturating_sub(copy.offset / source.bpf.max(1))
        };
        (*dest_meta).bpf = source.bpf;
    }
    glib::ffi::GTRUE
}

fn ensure_dlb_meta() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if unsafe { !gst::ffi::gst_meta_get_info(c"DlbObjectAudioMeta".as_ptr()).is_null() } {
            return;
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
                Some(transform_meta),
            );
            assert!(!info.is_null());
        }
    });
}

fn init() {
    let _ = gst::init();
    ensure_dlb_meta();
    let _ = objectaudio_meta::init();
    let _ = gst::Element::register(
        None,
        "salsaobjectaudioadapt",
        gst::Rank::PRIMARY,
        ObjectAudioAdapt::static_type(),
    );
}

fn payload() -> SpatialPayload {
    SpatialPayload::new(
        48_000,
        vec![
            ObjectState::new(1, [0.0, 1.0, 0.0]),
            ObjectState::new(2, [1.0, 1.0, 0.0]),
        ],
        vec![ObjectEvent {
            offset: 0,
            ramp_samples: 0,
            object: 1,
            state: ObjectState::new(1, [-1.0, 1.0, 0.0]),
        }],
    )
}

fn run(
    direction: &str,
    feature: &str,
    build: impl FnOnce(&mut gst::BufferRef),
    check: impl Fn(&gst::BufferRef) -> bool + Send + Sync + 'static,
) {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("appsrc").build().unwrap();
    let adapt = gst::ElementFactory::make("salsaobjectaudioadapt")
        .property("direction", direction)
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink").build().unwrap();
    src.set_property("format", gst::Format::Time);
    src.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("channels", 3i32)
            .field("rate", 48_000i32)
            .field("channel-mask", gst::Bitmask(0))
            .features([feature])
            .build(),
    );
    pipeline.add_many([&src, &adapt, &sink]).unwrap();
    gst::Element::link_many([&src, &adapt, &sink]).unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicUsize::new(0));
    let (seen_probe, ok_probe) = (seen.clone(), ok.clone());
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                seen_probe.fetch_add(1, Ordering::Relaxed);
                if check(buffer) {
                    ok_probe.fetch_add(1, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });

    let mut buffer = gst::Buffer::with_size(4 * 3 * 1536).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::from_mseconds(0));
        buffer.set_duration(gst::ClockTime::from_mseconds(32));
        build(buffer);
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let _: gst::FlowReturn = src.emit_by_name("push-buffer", &[&buffer]);
    src.send_event(gst::event::Eos::new());
    let bus = pipeline.bus().unwrap();
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    match message.view() {
        gst::MessageView::Eos(_) => {}
        gst::MessageView::Error(error) => panic!("pipeline error: {error:?}"),
        _ => unreachable!(),
    }
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        seen.load(Ordering::Relaxed) > 0,
        "no buffer reached the sink"
    );
    assert_eq!(ok.load(Ordering::Relaxed), seen.load(Ordering::Relaxed));
}

#[test]
fn to_dolby_encodes_spatial_payload_as_oamd() {
    init();
    run(
        "to-dolby",
        objectaudio_meta::CAPS_FEATURE,
        |buffer| add_spatial(buffer, &payload(), 0, 12).unwrap(),
        |buffer| {
            let payloads = objectaudio_meta::dlb_payloads(buffer);
            !payloads.is_empty()
                && objectaudio_oamd::parse(payloads[0].bytes)
                    .map(|frame| frame.dynamic_objects().count() == 2)
                    .unwrap_or(false)
        },
    );
}

#[test]
fn to_salsa_decodes_oamd_into_spatial_payload() {
    init();
    let bytes = objectaudio_oamd::encode(&payload()).unwrap();
    run(
        "to-salsa",
        objectaudio_meta::DLB_CAPS_FEATURE,
        |buffer| objectaudio_meta::add_dlb_payload(buffer, &bytes, 0, 12).unwrap(),
        |buffer| {
            get_spatial(buffer)
                .map(|snapshot| snapshot.payload.objects.len() == 2)
                .unwrap_or(false)
        },
    );
}
