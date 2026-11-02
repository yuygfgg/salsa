//! End-to-end checks that need a real JOC stream.
//!
//! Set `SALSA_TEST_EC3` to a file path. The test is skipped when the variable
//! is unset so the default `cargo test` stays hermetic.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;

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

fn run_pipeline(description: &str) -> (Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let metas = Arc::new(AtomicUsize::new(0));
    let buffers = Arc::new(AtomicUsize::new(0));
    let ramps = Arc::new(AtomicUsize::new(0));
    let pipeline = gst::parse::launch(description)
        .expect("parse pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
    let sink = pipeline.by_name("sink").expect("sink");
    let counters = (metas.clone(), buffers.clone(), ramps.clone());
    let pad = sink.static_pad("sink").expect("sink pad");
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(buffer) = info.buffer() {
            counters.1.fetch_add(1, Ordering::Relaxed);
            if let Some(snapshot) = objectaudio_meta::get_spatial(buffer) {
                counters.0.fetch_add(1, Ordering::Relaxed);
                if snapshot.payload.snapshot_ramp_samples.is_some() {
                    counters.2.fetch_add(1, Ordering::Relaxed);
                }
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
    (metas, buffers, ramps)
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
    let (metas, buffers, ramps) = run_pipeline(&pipeline);
    assert!(buffers.load(Ordering::Relaxed) > 0, "no decoded buffers");
    assert_eq!(
        metas.load(Ordering::Relaxed),
        buffers.load(Ordering::Relaxed),
        "every buffer must carry ObjectAudioMeta"
    );
    assert!(
        ramps.load(Ordering::Relaxed) > 0,
        "OAMD snapshot ramp was not carried into SpatialPayload"
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
    let (metas, buffers, _) = run_pipeline(&pipeline);
    assert!(buffers.load(Ordering::Relaxed) > 0, "no decoded buffers");
    assert_eq!(
        metas.load(Ordering::Relaxed),
        0,
        "plain caps select no feature"
    );
}

fn collect_pts(
    rx: &mpsc::Receiver<(u32, Option<gst::ClockTime>)>,
    generation: u32,
    count: usize,
) -> Vec<Option<gst::ClockTime>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut out = Vec::new();
    while out.len() < count {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok((seen, pts)) => {
                if seen == generation {
                    out.push(pts);
                }
            }
            Err(_) => break,
        }
    }
    out
}

#[test]
fn flushing_seek_restarts_the_stream() {
    let Some(path) = std::env::var_os("SALSA_TEST_EC3") else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping the real-stream seek test");
        return;
    };
    init();
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! salsaeac3jocparse ! salsaeac3jocdec \
         ! fakesink name=sink sync=false",
        path.to_string_lossy()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");

    let sink = pipeline.by_name("sink").expect("sink");
    let (pts_tx, pts_rx) = mpsc::channel();
    let generation = Arc::new(AtomicU32::new(0));
    let probe_generation = generation.clone();
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(buffer) = info.buffer() {
                let _ = pts_tx.send((probe_generation.load(Ordering::Relaxed), buffer.pts()));
            }
            PadProbeReturn::Ok
        },
    );

    pipeline.set_state(gst::State::Paused).expect("paused");
    let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(30));
    assert!(result.is_ok(), "pipeline did not preroll: {result:?}");

    let forward = gst::ClockTime::from_seconds(2);
    generation.store(1, Ordering::Relaxed);
    pipeline
        .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, forward)
        .expect("forward seek");
    pipeline.set_state(gst::State::Playing).expect("playing");

    let forward_pts = collect_pts(&pts_rx, 1, 16);
    assert!(
        !forward_pts.is_empty(),
        "no buffers were produced after the forward seek"
    );
    assert!(
        forward_pts
            .iter()
            .any(|pts| pts.is_some_and(|pts| pts >= forward)),
        "PTS did not advance past {forward}: {forward_pts:?}"
    );

    let backward = gst::ClockTime::from_seconds(1);
    generation.store(2, Ordering::Relaxed);
    pipeline
        .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, backward)
        .expect("backward seek");

    let backward_pts = collect_pts(&pts_rx, 2, 16);
    pipeline.set_state(gst::State::Null).expect("null");

    assert!(
        !backward_pts.is_empty(),
        "no buffers were produced after the backward seek"
    );
    assert!(
        backward_pts
            .iter()
            .any(|pts| pts.is_some_and(|pts| pts < forward)),
        "backward seek did not move PTS before {forward}: {backward_pts:?}"
    );
    assert!(
        backward_pts
            .iter()
            .any(|pts| pts.is_some_and(|pts| pts >= backward)),
        "PTS did not reach {backward}: {backward_pts:?}"
    );
}
