//! Parse-element framing against real `OggSalsaWriter` output.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;
use objectaudio_ir::ObjectState;
use salsa_spatial::{
    EnhancementTile, MultistreamPacket, OggSalsaWriter, SalsaSpatialFrame, SalsaSpatialStreamConfig,
};

fn stream_bytes() -> Vec<u8> {
    let config = SalsaSpatialStreamConfig {
        object_count: 2,
        initial_objects: vec![
            ObjectState::new(1, [-0.5, 0.5, 0.0]),
            ObjectState::new(2, [0.5, 0.5, 0.0]),
        ],
        ..Default::default()
    };
    let packet = MultistreamPacket::new(vec![
        vec![0xfc, 0x01],
        vec![0xfc, 0x02],
        vec![0xfc, 0x03],
        vec![0xf8, 0x04],
        vec![0xf8, 0x05],
        vec![0xf8, 0x06],
    ]);
    let frame = SalsaSpatialFrame {
        sample_rate: 48_000,
        start_sample: 0,
        frames: 4_800,
        packets: vec![packet; 5],
        enhancement: EnhancementTile {
            start_sample: 0,
            frames: 4_800,
            bytes: Vec::new(),
        },
        checkpoint: None,
        parameters: None,
    };
    let (mut writer, mut bytes) = OggSalsaWriter::new(config.clone(), 1, 120).expect("writer");
    bytes.extend(writer.write_frame(&frame).expect("first tile"));
    bytes.extend(writer.write_frame(&frame).expect("second tile"));
    bytes.extend(writer.finish());
    bytes
}

#[test]
fn parser_emits_one_buffer_per_complete_tile() {
    salsa_gst_registry::init().expect("register salsa elements");
    let path =
        std::env::temp_dir().join(format!("salsa-spatial-parser-{}.ogg", std::process::id()));
    std::fs::write(&path, stream_bytes()).expect("write stream");
    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! salsaspatialparse name=parse ! fakesink name=sink sync=false",
        path.to_string_lossy()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let parse = pipeline.by_name("parse").expect("parse element");
    let buffers = Arc::new(AtomicUsize::new(0));
    let counters = buffers.clone();
    parse.static_pad("src").expect("src pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if info.buffer().is_some() {
                counters.fetch_add(1, Ordering::Relaxed);
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
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    let caps = parse.static_pad("src").and_then(|pad| pad.current_caps());
    pipeline.set_state(gst::State::Null).expect("null");
    let _ = std::fs::remove_file(&path);
    assert!(
        matches!(message.view(), gst::MessageView::Eos(_)),
        "pipeline error: {message:?}"
    );
    assert_eq!(
        buffers.load(Ordering::Relaxed),
        2,
        "the final short tile must be drained"
    );
    let caps = caps.expect("src caps");
    let structure = caps.structure(0).expect("src structure");
    assert_eq!(structure.get::<bool>("framed"), Ok(true));
    assert_eq!(structure.get::<&str>("alignment"), Ok("frame"));
    assert_eq!(structure.get::<i32>("object-count"), Ok(2));
}
