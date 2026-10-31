//! Opt-in TrueHD Atmos pipeline checks.
//!
//! Set `SALSA_TEST_TRUEHD` to a `.mlp`/`.thd` file. The tests are skipped when
//! the variable is unset so the default `cargo test` stays hermetic.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;

use gstsalsaobjectaudio::{HrtfRender, Oar};
use gstsalsatruehd::{TrueHdAtmosDec, TrueHdParse};

fn init() {
    let _ = gst::init();
    let _ = objectaudio_meta::init();
    let _ = gst::Element::register(
        None,
        "salsatruehdparse",
        gst::Rank::PRIMARY,
        TrueHdParse::static_type(),
    );
    let _ = gst::Element::register(
        None,
        "salsatruehdatmosdec",
        gst::Rank::PRIMARY,
        TrueHdAtmosDec::static_type(),
    );
    let _ = gst::Element::register(None, "salsaoar", gst::Rank::PRIMARY, Oar::static_type());
    let _ = gst::Element::register(
        None,
        "salsahrtfrender",
        gst::Rank::PRIMARY,
        HrtfRender::static_type(),
    );
}

fn sample_path() -> Option<String> {
    std::env::var_os("SALSA_TEST_TRUEHD").map(|path| path.to_string_lossy().into_owned())
}

fn decode_prefix(extra: &str, blocks: u32) -> gst::Pipeline {
    decode_prefix_with(extra, 1_048_576, blocks)
}

fn decode_prefix_with(extra: &str, blocksize: u32, blocks: u32) -> gst::Pipeline {
    let path = sample_path().expect("SALSA_TEST_TRUEHD");
    let description = format!(
        "filesrc location=\"{path}\" blocksize={blocksize} num-buffers={blocks} \
         ! salsatruehdparse ! salsatruehdatmosdec {extra}"
    );
    gst::parse::launch(&description)
        .expect("parse pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline")
}

fn parse_pipeline() -> gst::Pipeline {
    let path = sample_path().expect("SALSA_TEST_TRUEHD");
    gst::parse::launch(&format!(
        "filesrc location=\"{path}\" num-buffers=2 ! salsatruehdparse \
         ! salsatruehdatmosdec ! fakesink name=sink sync=false"
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline")
}

#[test]
fn parser_output_reaches_the_decoder() {
    if sample_path().is_none() {
        eprintln!("SALSA_TEST_TRUEHD is unset; skipping the TrueHD parser test");
        return;
    }
    init();
    let buffers = Arc::new(AtomicUsize::new(0));
    let pipeline = parse_pipeline();
    let sink = pipeline.by_name("sink").expect("sink");
    let counter = buffers.clone();
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if info.buffer().is_some() {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            PadProbeReturn::Ok
        },
    );
    let _ = run(pipeline);
    assert!(buffers.load(Ordering::Relaxed) > 0, "no decoded buffers");
}

fn run(pipeline: gst::Pipeline) -> Option<gst::Caps> {
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
    match message.view() {
        gst::MessageView::Eos(_) => {}
        gst::MessageView::Error(error) => panic!("pipeline error: {error:?}"),
        _ => unreachable!(),
    }
    let caps = pipeline
        .by_name("sink")
        .and_then(|sink| sink.static_pad("sink"))
        .and_then(|pad| pad.current_caps());
    pipeline.set_state(gst::State::Null).expect("null");
    caps
}

#[test]
fn object_meta_reaches_the_sink() {
    if sample_path().is_none() {
        eprintln!("SALSA_TEST_TRUEHD is unset; skipping the TrueHD pipeline test");
        return;
    }
    init();
    let buffers = Arc::new(AtomicUsize::new(0));
    let metas = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let pipeline = decode_prefix(
        "! capsfilter caps=\"audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
         format=(string)F32LE, layout=(string)interleaved, channels=(int)16, \
         rate=(int)48000\" ! fakesink name=sink sync=false",
        2,
    );
    let sink = pipeline.by_name("sink").expect("sink");
    let counters = (buffers.clone(), metas.clone(), bad.clone());
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(buffer) = info.buffer() {
                counters.0.fetch_add(1, Ordering::Relaxed);
                if objectaudio_meta::get(buffer).is_some() {
                    counters.1.fetch_add(1, Ordering::Relaxed);
                }
                if buffer.size() % (4 * 16 * 32) != 0 {
                    counters.2.fetch_add(1, Ordering::Relaxed);
                }
            }
            PadProbeReturn::Ok
        },
    );
    let _ = run(pipeline);
    assert!(buffers.load(Ordering::Relaxed) > 0, "no decoded buffers");
    assert!(metas.load(Ordering::Relaxed) > 0, "no OAMD metadata");
    assert_eq!(
        bad.load(Ordering::Relaxed),
        0,
        "every buffer must be a whole renderer block"
    );
}

#[test]
fn speaker_renderer_consumes_the_truehd_ir() {
    if sample_path().is_none() {
        eprintln!("SALSA_TEST_TRUEHD is unset; skipping the TrueHD speaker test");
        return;
    }
    init();
    let bad = Arc::new(AtomicUsize::new(0));
    let pipeline = decode_prefix("! salsaoar layout=7.1.4 ! fakesink name=sink sync=false", 2);
    let sink = pipeline.by_name("sink").expect("sink");
    let bad_counter = bad.clone();
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(buffer) = info.buffer() {
                if buffer.size() % (12 * 4) != 0 {
                    bad_counter.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(map) = buffer.map_readable() {
                    let (samples, _) = map.as_slice().as_chunks::<4>();
                    for sample in samples {
                        if !f32::from_le_bytes(*sample).is_finite() {
                            bad_counter.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            PadProbeReturn::Ok
        },
    );
    let caps = run(pipeline).expect("negotiated caps");
    assert_eq!(caps.structure(0).unwrap().get::<i32>("channels"), Ok(12));
    assert_eq!(
        bad.load(Ordering::Relaxed),
        0,
        "bad layout or non-finite PCM"
    );
}

#[test]
fn binaural_renderer_consumes_the_truehd_ir() {
    let (Some(_), Some(model)) = (
        sample_path(),
        std::env::var_os("SALSA_ROSELLA_MODEL").map(|path| path.to_string_lossy().into_owned()),
    ) else {
        eprintln!("SALSA_TEST_TRUEHD / SALSA_ROSELLA_MODEL are unset; skipping");
        return;
    };
    init();
    let bad = Arc::new(AtomicUsize::new(0));
    // The debug HRTF path is ~50x slower than release; keep this test short.
    let pipeline = decode_prefix_with(
        &format!(
            "! salsahrtfrender personalized-headphone=\"{model}\" filterbank=2 \
             ! fakesink name=sink sync=false"
        ),
        65_536,
        1,
    );
    let sink = pipeline.by_name("sink").expect("sink");
    let bad_counter = bad.clone();
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(buffer) = info.buffer() {
                if buffer.size() % (2 * 4) != 0 {
                    bad_counter.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(map) = buffer.map_readable() {
                    let (samples, _) = map.as_slice().as_chunks::<4>();
                    for sample in samples {
                        if !f32::from_le_bytes(*sample).is_finite() {
                            bad_counter.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            PadProbeReturn::Ok
        },
    );
    let caps = run(pipeline).expect("negotiated caps");
    assert_eq!(caps.structure(0).unwrap().get::<i32>("channels"), Ok(2));
    assert_eq!(
        bad.load(Ordering::Relaxed),
        0,
        "bad layout or non-finite PCM"
    );
}
