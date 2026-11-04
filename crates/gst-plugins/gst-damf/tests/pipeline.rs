//! Hermetic `salsadamfsrc` pipeline tests with a generated DAMF fixture.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;

fn init() {
    let _ = salsa_gst_registry::init();
}

fn unique_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("salsa-gst-damf-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write_fixture(dir: &Path, frames: usize) -> PathBuf {
    let manifest = dir.join("master.atmos");
    let mut file = std::fs::File::create(&manifest).unwrap();
    file.write_all(
        br#"version: 0.5.1
presentations:
  - audio: master.atmos.audio
    metadata: master.atmos.metadata
    offset: 0.0
    bedInstances:
      - channels:
          - channel: LFE
            ID: 3
    objects:
      - ID: 10
      - ID: 11
"#,
    )
    .unwrap();

    let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
    metadata
        .write_all(
            br#"sampleRate: 48000
events:
  - ID: 3
    samplePos: 0
    gain: 0
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
  - ID: 11
    samplePos: 0
    pos: [1, 0, 0]
"#,
        )
        .unwrap();

    // 3 channels (LFE, object 10, object 11).
    let channels = 3usize;
    let data_len = channels * frames * 3;
    let mut caf = std::fs::File::create(dir.join("master.atmos.audio")).unwrap();
    caf.write_all(b"caff").unwrap();
    caf.write_all(&1u16.to_be_bytes()).unwrap();
    caf.write_all(&0u16.to_be_bytes()).unwrap();
    caf.write_all(b"desc").unwrap();
    caf.write_all(&32u64.to_be_bytes()).unwrap();
    caf.write_all(&48_000f64.to_be_bytes()).unwrap();
    caf.write_all(b"lpcm").unwrap();
    caf.write_all(&0u32.to_be_bytes()).unwrap();
    caf.write_all(&((channels * 3) as u32).to_be_bytes())
        .unwrap();
    caf.write_all(&1u32.to_be_bytes()).unwrap();
    caf.write_all(&(channels as u32).to_be_bytes()).unwrap();
    caf.write_all(&24u32.to_be_bytes()).unwrap();
    caf.write_all(b"data").unwrap();
    caf.write_all(&((data_len + 4) as i64).to_be_bytes())
        .unwrap();
    caf.write_all(&0u32.to_be_bytes()).unwrap();
    for frame in 0..frames {
        let values = [0.0f32, 0.25, -0.25 + frame as f32 * 0.01];
        for value in values {
            let value = (value * 8_388_608.0).round() as i32;
            caf.write_all(&value.to_be_bytes()[1..4]).unwrap();
        }
    }
    manifest
}

struct Guard(gst::Pipeline);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn run(pipeline: gst::Pipeline) {
    pipeline.set_state(gst::State::Playing).expect("playing");
    let bus = pipeline.bus().expect("bus");
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    match message.view() {
        gst::MessageView::Eos(_) => {}
        gst::MessageView::Error(error) => panic!("pipeline error: {error:?}"),
        _ => unreachable!(),
    }
}

#[test]
fn source_attaches_object_metadata() {
    init();
    let dir = unique_dir();
    let manifest = write_fixture(&dir, 8);
    let pipeline = gst::parse::launch(&format!(
        "salsadamfsrc location=\"{}\" num-buffers=1 ! \
         capsfilter caps=\"audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta), \
         format=(string)F32LE, layout=(string)interleaved, channels=(int)3, rate=(int)48000\" \
         ! fakesink name=sink sync=false",
        manifest.display()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let guard = Guard(pipeline);
    let sink = guard.0.by_name("sink").expect("sink");
    let buffers = Arc::new(AtomicUsize::new(0));
    let metas = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let counters = (buffers.clone(), metas.clone(), bad.clone());
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                counters.0.fetch_add(1, Ordering::Relaxed);
                if objectaudio_meta::get(buffer).is_some() {
                    counters.1.fetch_add(1, Ordering::Relaxed);
                }
                if buffer.size() != 8 * 3 * 4 {
                    counters.2.fetch_add(1, Ordering::Relaxed);
                }
            }
            PadProbeReturn::Ok
        });
    run(guard.0.clone());
    assert!(buffers.load(Ordering::Relaxed) > 0, "no buffers");
    assert!(metas.load(Ordering::Relaxed) > 0, "no ObjectAudioMeta");
    assert_eq!(bad.load(Ordering::Relaxed), 0, "unexpected buffer size");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn source_supports_flushing_seek() {
    init();
    let dir = unique_dir();
    // 2 seconds at 48 kHz.
    let manifest = write_fixture(&dir, 96_000);
    let pipeline = gst::parse::launch(&format!(
        "salsadamfsrc location=\"{}\" ! fakesink name=sink sync=false",
        manifest.display()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let guard = Guard(pipeline);
    let sink = guard.0.by_name("sink").expect("sink");
    let (sender, receiver) = mpsc::channel();
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                let _ = sender.send(buffer.to_owned());
            }
            PadProbeReturn::Ok
        });

    guard.0.set_state(gst::State::Paused).expect("paused");
    let (result, _, _) = guard.0.state(gst::ClockTime::from_seconds(20));
    assert!(result.is_ok(), "pipeline did not preroll: {result:?}");
    assert_eq!(
        guard.0.query_duration::<gst::ClockTime>(),
        Some(gst::ClockTime::from_seconds(2))
    );
    guard
        .0
        .seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::ClockTime::from_seconds(1),
        )
        .expect("flushing seek");
    while receiver.try_recv().is_ok() {}
    guard.0.set_state(gst::State::Playing).expect("playing");

    let mut first = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while first.is_none() && std::time::Instant::now() < deadline {
        if let Ok(buffer) = receiver.recv_timeout(std::time::Duration::from_millis(50)) {
            first = Some(buffer);
        }
    }
    let first = first.expect("no buffer after seek");
    assert_eq!(first.pts(), Some(gst::ClockTime::from_seconds(1)));
    assert_eq!(first.offset(), 48_000);
    assert!(first.flags().contains(gst::BufferFlags::DISCONT));

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn source_feeds_the_speaker_renderer() {
    init();
    let dir = unique_dir();
    let manifest = write_fixture(&dir, 8);
    let pipeline = gst::parse::launch(&format!(
        "salsadamfsrc location=\"{}\" num-buffers=1 ! salsaoar layout=5.1 \
         ! fakesink name=sink sync=false",
        manifest.display()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let guard = Guard(pipeline);
    let sink = guard.0.by_name("sink").expect("sink");
    let buffers = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let counters = (buffers.clone(), bad.clone());
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                counters.0.fetch_add(1, Ordering::Relaxed);
                if buffer.size() % (6 * 4) != 0 {
                    counters.1.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(map) = buffer.map_readable() {
                    for sample in map.as_slice().as_chunks::<4>().0 {
                        if !f32::from_le_bytes(*sample).is_finite() {
                            counters.1.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
            PadProbeReturn::Ok
        });
    run(guard.0.clone());
    assert!(buffers.load(Ordering::Relaxed) > 0, "no rendered buffers");
    assert_eq!(bad.load(Ordering::Relaxed), 0, "invalid rendered output");
    std::fs::remove_dir_all(dir).unwrap();
}
