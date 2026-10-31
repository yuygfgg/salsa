//! Opt-in end-to-end `salsaeac3jocdec ! salsaoar` check.
//!
//! Set `SALSA_TEST_EC3` to a JOC stream. The test is skipped otherwise, so the
//! default matrix stays hermetic.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;
use gstsalsaeac3joc::{Eac3JocDec, Eac3JocParse};
use gstsalsaobjectaudio::Oar;

fn init() {
    let _ = gst::init();
    let _ = gst::Element::register(
        None,
        "salsaeac3jocparse",
        gst::Rank::PRIMARY,
        Eac3JocParse::static_type(),
    );
    let _ = gst::Element::register(
        None,
        "salsaeac3jocdec",
        gst::Rank::PRIMARY,
        Eac3JocDec::static_type(),
    );
    let _ = gst::Element::register(None, "salsaoar", gst::Rank::PRIMARY, Oar::static_type());
}

#[test]
fn object_audio_reaches_the_speaker_renderer() {
    let Some(path) = std::env::var_os("SALSA_TEST_EC3") else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping the real-stream salsaoar test");
        return;
    };
    init();
    let buffers = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" num-buffers=24 ! salsaeac3jocparse ! salsaeac3jocdec \
         ! salsaoar layout=7.1.4 ! fakesink name=sink sync=false",
        path.to_string_lossy()
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
                if buffer.size() % (12 * 4) != 0 {
                    counters.1.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(map) = buffer.map_readable() {
                    let (samples, _) = map.as_slice().as_chunks::<4>();
                    for sample in samples {
                        let value = f32::from_le_bytes(*sample);
                        if !value.is_finite() {
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
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    match message.view() {
        gst::MessageView::Eos(_) => {}
        gst::MessageView::Error(error) => panic!("pipeline error: {error:?}"),
        _ => unreachable!(),
    }
    let caps = sink
        .static_pad("sink")
        .and_then(|pad| pad.current_caps())
        .expect("negotiated caps");
    let structure = caps.structure(0).expect("caps structure");
    assert_eq!(structure.get::<i32>("channels"), Ok(12));
    pipeline.set_state(gst::State::Null).expect("null");
    assert!(buffers.load(Ordering::Relaxed) > 0, "no rendered buffers");
    assert_eq!(bad.load(Ordering::Relaxed), 0, "bad caps or non-finite PCM");
}
