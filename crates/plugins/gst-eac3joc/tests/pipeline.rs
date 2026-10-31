//! End-to-end checks that need a real JOC stream.
//!
//! Set `SALSA_TEST_EC3` to a file path. The test is skipped when the variable
//! is unset so the default `cargo test` stays hermetic.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;
use gstsalsaeac3joc::{Eac3JocDec, Eac3JocParse};

fn init() {
    let _ = gst::init();
    let _ = objectaudio_meta::init();
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
}

fn run_pipeline(description: &str) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let metas = Arc::new(AtomicUsize::new(0));
    let buffers = Arc::new(AtomicUsize::new(0));
    let pipeline = gst::parse::launch(description)
        .expect("parse pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
    let sink = pipeline.by_name("sink").expect("sink");
    let counters = (metas.clone(), buffers.clone());
    let pad = sink.static_pad("sink").expect("sink pad");
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(buffer) = info.buffer() {
            counters.1.fetch_add(1, Ordering::Relaxed);
            if objectaudio_meta::get(buffer).is_some() {
                counters.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        PadProbeReturn::Ok
    });
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
    pipeline.set_state(gst::State::Null).expect("null");
    (metas, buffers)
}

#[test]
fn object_meta_reaches_the_sink() {
    let Some(path) = std::env::var_os("SALSA_TEST_EC3") else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping the real-stream pipeline test");
        return;
    };
    init();
    let pipeline = format!(
        "filesrc location=\"{}\" num-buffers=16 ! salsaeac3jocparse ! salsaeac3jocdec ! \
         capsfilter name=filter caps=\"audio/x-raw(memory:SystemMemory, \
         meta:ObjectAudioMeta), format=(string)F32LE, layout=(string)interleaved, \
         channels=(int)[1,16], rate=(int)48000\" ! fakesink name=sink sync=false",
        path.to_string_lossy()
    );
    let (metas, buffers) = run_pipeline(&pipeline);
    assert!(buffers.load(Ordering::Relaxed) > 0, "no decoded buffers");
    assert_eq!(
        metas.load(Ordering::Relaxed),
        buffers.load(Ordering::Relaxed),
        "every buffer must carry ObjectAudioMeta"
    );
}

#[test]
fn wav_fallback_negotiates_plain_raw() {
    let Some(path) = std::env::var_os("SALSA_TEST_EC3") else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping the real-stream pipeline test");
        return;
    };
    init();
    let pipeline = format!(
        "filesrc location=\"{}\" num-buffers=16 ! salsaeac3jocparse ! salsaeac3jocdec ! \
         fakesink name=sink sync=false",
        path.to_string_lossy()
    );
    let (metas, buffers) = run_pipeline(&pipeline);
    assert!(buffers.load(Ordering::Relaxed) > 0, "no decoded buffers");
    assert_eq!(
        metas.load(Ordering::Relaxed),
        0,
        "plain caps select no feature"
    );
}
