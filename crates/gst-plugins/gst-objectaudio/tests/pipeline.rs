//! Opt-in end-to-end decoder and speaker-renderer checks.
//!
//! Set `SALSA_TEST_EC3` or `SALSA_TEST_TRUEHD` to a real stream. Tests without
//! a sample path are skipped, so the default matrix stays hermetic.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;

fn init() {
    let _ = salsa_gst_registry::init();
}

struct PlayingPipeline(gst::Pipeline);

impl Drop for PlayingPipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn seek_through_renderer(variable: &str, decoder: &str, targets_ms: &[u64]) {
    let Some(path) = std::env::var_os(variable) else {
        eprintln!("{variable} is unset; skipping the real-stream renderer seek test");
        return;
    };
    init();
    let guard = PlayingPipeline(
        gst::parse::launch(&format!(
            "filesrc location=\"{}\" blocksize=1048576 ! {decoder} \
             ! salsaoar layout=7.1.4 ! fakesink name=sink sync=false",
            path.to_string_lossy()
        ))
        .expect("parse pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline"),
    );
    let pipeline = &guard.0;
    let (sender, receiver) = mpsc::channel();
    pipeline
        .by_name("sink")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                let _ = sender.send(buffer.to_owned());
            }
            PadProbeReturn::Ok
        });
    let bus = pipeline.bus().unwrap();

    for &target_ms in targets_ms {
        pipeline.set_state(gst::State::Paused).expect("paused");
        let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(30));
        assert!(result.is_ok(), "pipeline did not preroll: {result:?}");
        while receiver.try_recv().is_ok() {}

        let start = gst::ClockTime::from_mseconds(target_ms);
        pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, start)
            .expect("accurate flushing seek");
        pipeline.set_state(gst::State::Playing).expect("playing");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut buffers = Vec::new();
        while buffers.len() < 16 && std::time::Instant::now() < deadline {
            if let Some(message) =
                bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])
            {
                panic!("renderer stopped after seek: {message:?}");
            }
            if let Ok(buffer) = receiver.recv_timeout(std::time::Duration::from_millis(50)) {
                buffers.push(buffer);
            }
        }
        assert_eq!(
            buffers.len(),
            16,
            "seek must resume sustained rendered output"
        );
        assert!(buffers[0].flags().contains(gst::BufferFlags::DISCONT));
        let mut samples = 0;
        for buffer in buffers.iter() {
            assert_eq!(buffer.size() % (12 * 4), 0);
            let pts = buffer.pts().expect("rendered PTS");
            let expected = start + gst::ClockTime::from_nseconds(samples * 1_000_000_000 / 48_000);
            assert!(pts.nseconds().abs_diff(expected.nseconds()) <= 1);
            let frames = (buffer.size() / (12 * 4)) as u64;
            assert_eq!(frames % 32, 0, "complete renderer blocks before EOS");
            let duration = gst::ClockTime::from_nseconds(frames * 1_000_000_000 / 48_000);
            assert!(
                buffer
                    .duration()
                    .unwrap()
                    .nseconds()
                    .abs_diff(duration.nseconds())
                    <= 1
            );
            samples += frames;
            let map = buffer.map_readable().unwrap();
            assert!(
                map.as_slice()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|bytes| { f32::from_le_bytes(*bytes).is_finite() })
            );
        }
    }
}

#[test]
fn truehd_flushing_seek_reaches_the_speaker_renderer() {
    seek_through_renderer(
        "SALSA_TEST_TRUEHD",
        "salsatruehdparse ! salsatruehdatmosdec",
        &[2_000, 1_001],
    );
}

#[test]
fn eac3_flushing_seek_reaches_the_speaker_renderer() {
    seek_through_renderer(
        "SALSA_TEST_EC3",
        "salsaeac3jocparse ! salsaeac3jocdec",
        &[2_001, 1_001],
    );
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
