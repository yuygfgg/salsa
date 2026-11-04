//! `salsahrtfrender` caps and opt-in end-to-end pipeline checks.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer_base::prelude::BaseTransformExtManual;
use gstsalsaobjectaudio::HrtfRender;

fn init() {
    let _ = salsa_gst_registry::init();
}

#[test]
fn sink_caps_require_object_meta() {
    let _ = gst::init();
    let render: HrtfRender = glib::Object::new();
    let given: gst::Caps = "audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
        format=(string)F32LE, layout=(string)interleaved, channels=(int)16, \
        rate=(int)48000"
        .parse()
        .unwrap();
    let queried = render.sink_pad().query_caps(None);
    assert!(!queried.intersect(&given).is_empty());

    let plain: gst::Caps = "audio/x-raw, format=(string)F32LE, layout=(string)interleaved, \
        channels=(int)16, rate=(int)48000"
        .parse()
        .unwrap();
    assert!(queried.intersect(&plain).is_empty());
}

#[test]
fn src_caps_are_stereo_48k() {
    let _ = gst::init();
    let render: HrtfRender = glib::Object::new();
    let caps = render.src_pad().query_caps(None);
    let structure = caps.structure(0).expect("caps");
    assert_eq!(structure.get::<i32>("channels"), Ok(2));
    assert_eq!(structure.get::<i32>("rate"), Ok(48_000));
    assert_eq!(
        structure.get::<gst::Bitmask>("channel-mask"),
        Ok(gst::Bitmask(0x3))
    );
}

#[test]
fn renders_a_real_stream_to_stereo() {
    let (Some(path), Some(model)) = (
        std::env::var_os("SALSA_TEST_EC3"),
        std::env::var_os("SALSA_ROSELLA_MODEL"),
    ) else {
        eprintln!(
            "SALSA_TEST_EC3 / SALSA_ROSELLA_MODEL are unset; skipping the salsahrtfrender pipeline test"
        );
        return;
    };
    init();
    let buffers = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" num-buffers=1 ! salsaeac3jocparse ! salsaeac3jocdec \
         ! salsahrtfrender personalized-headphone=\"{}\" filterbank=2 \
             tail-seconds=0.05 ! fakesink name=sink sync=false",
        path.to_string_lossy(),
        model.to_string_lossy()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let sink = pipeline.by_name("sink").expect("sink");
    let counters = (buffers.clone(), bad.clone());
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(buffer) = info.buffer() {
                counters.0.fetch_add(1, Ordering::Relaxed);
                if buffer.size() % 8 != 0 {
                    counters.1.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(map) = buffer.map_readable() {
                    let (samples, _) = map.as_slice().as_chunks::<4>();
                    for sample in samples {
                        if !f32::from_le_bytes(*sample).is_finite() {
                            counters.1.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            PadProbeReturn::Ok
        },
    );
    pipeline
        .set_state(gst::State::Playing)
        .expect("set playing");
    let bus = pipeline.bus().expect("bus");
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(120),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    if let gst::MessageView::Error(error) = message.view() {
        panic!("pipeline error: {error:?}");
    }
    let caps = sink
        .static_pad("sink")
        .and_then(|pad| pad.current_caps())
        .expect("negotiated caps");
    let structure = caps.structure(0).expect("caps structure");
    assert_eq!(structure.get::<i32>("channels"), Ok(2));
    pipeline.set_state(gst::State::Null).expect("null");
    assert!(buffers.load(Ordering::Relaxed) > 0, "no rendered buffers");
    assert_eq!(bad.load(Ordering::Relaxed), 0, "bad caps or non-finite PCM");
}
