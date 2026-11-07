//! End-to-end checks that need an encoded salsa-spatial stream and the DSP
//! decoder.
//!
//! Set `SALSA_TEST_SPATIAL` to an IAMF file path to run the pipeline check.
//! Without the variable the test returns immediately.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;

#[test]
fn eos_delivers_the_complete_dsp_tail() {
    use salsa_spatial::{
        CorePacket, EnhancementTile, IamfWriter, SalsaSpatialFrame, SalsaSpatialStreamConfig,
    };
    salsa_gst_registry::init().expect("register salsa elements");
    let config = SalsaSpatialStreamConfig {
        object_count: 1,
        ..Default::default()
    };
    let mut writer = IamfWriter::new(&config).unwrap();
    let packet = CorePacket {
        front: vec![0xf8, 0xff, 0xfe],
        surround: vec![0xf8, 0xff, 0xfe],
        height: vec![0xf8, 0xff, 0xfe],
        center: vec![0xf8, 0xff, 0xfe],
        lfe: vec![0xf8, 0xff, 0xfe],
    };
    let mut bytes = Vec::new();
    for (start, frames) in [(0, 4800usize), (4800, 1600)] {
        let frame = SalsaSpatialFrame {
            sample_rate: 48000,
            start_sample: start,
            frames,
            packets: vec![packet.clone(); frames.div_ceil(960)],
            enhancement: EnhancementTile {
                start_sample: start,
                frames,
                bytes: Vec::new(),
                snapshot: false,
            },
            objects: Vec::new(),
            events: Vec::new(),
            snapshot: false,
            parameters: None,
        };
        bytes.extend(writer.write_frame(&frame).unwrap());
    }
    bytes.extend(writer.finish().unwrap());
    let path = std::env::temp_dir().join(format!("salsa-spatial-tail-{}.iamf", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! salsaspatialparse ! salsaspatialdec ! fakesink name=sink sync=false", path.display()
    )).unwrap().downcast::<gst::Pipeline>().unwrap();
    let samples = Arc::new(AtomicUsize::new(0));
    let counter = samples.clone();
    pipeline
        .by_name("sink")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                counter.fetch_add(buffer.size() / (2 * 4), Ordering::Relaxed);
            }
            PadProbeReturn::Ok
        });
    pipeline.set_state(gst::State::Playing).unwrap();
    let result = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    pipeline.set_state(gst::State::Null).unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(
        matches!(result.view(), gst::MessageView::Eos(_)),
        "{result:?}"
    );
    assert_eq!(samples.load(Ordering::Relaxed), 6400);
}

#[test]
fn object_meta_reaches_the_sink() {
    let Some(path) = std::env::var_os("SALSA_TEST_SPATIAL") else {
        return;
    };
    salsa_gst_registry::init().expect("register salsa elements");
    let buffers = Arc::new(AtomicUsize::new(0));
    let metas = Arc::new(AtomicUsize::new(0));
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! salsaspatialparse ! salsaspatialdec ! \
         capsfilter caps=\"audio/x-raw(memory:SystemMemory, \
         meta:ObjectAudioMeta), format=(string)F32LE, layout=(string)interleaved, \
         channels=(int)[1,129], rate=(int)48000\" ! fakesink name=sink sync=false",
        path.to_string_lossy()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let sink = pipeline.by_name("sink").expect("sink");
    let counters = (buffers.clone(), metas.clone());
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(buffer) = info.buffer() {
                counters.0.fetch_add(1, Ordering::Relaxed);
                if objectaudio_meta::get_spatial(buffer).is_some() {
                    counters.1.fetch_add(1, Ordering::Relaxed);
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
    pipeline.set_state(gst::State::Null).expect("null");
    assert!(
        matches!(message.view(), gst::MessageView::Eos(_)),
        "pipeline error: {message:?}"
    );
    assert!(buffers.load(Ordering::Relaxed) > 0, "no decoded buffers");
    assert_eq!(
        metas.load(Ordering::Relaxed),
        buffers.load(Ordering::Relaxed),
        "every object buffer must carry ObjectAudioMeta"
    );
}
