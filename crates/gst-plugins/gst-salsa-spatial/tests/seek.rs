//! Flushing time seeks through the parse and decoder elements.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::PadProbeReturn;
use gst::prelude::*;
use gstreamer as gst;
use opus::{Application, Channels, Encoder};
use salsa_spatial::{
    EnhancementTile, MultistreamPacket, OggSalsaWriter, SalsaSpatialFrame,
    SalsaSpatialStreamConfig, SceneSnapshot,
};

fn checkpoint(start: u64) -> SceneSnapshot {
    SceneSnapshot {
        start_sample: start,
        object_count: 1,
        active: vec![false],
        trajectories: vec![Vec::new()],
        power: salsa_spatial::ObjectPowerState {
            active: vec![false],
            ell_half_db: vec![[0; salsa_spatial::PARAM_BAND_COUNT]],
        },
        power_model: salsa_spatial::PowerModel {
            base_half_db: vec![[0; salsa_spatial::PARAM_BAND_COUNT]],
            fast_half_db: vec![[0; salsa_spatial::DCT_COEFF_COUNT]],
        },
        correction_half_db: vec![[0; salsa_spatial::PARAM_BAND_COUNT]],
        error_log: [[0; salsa_spatial::PARAM_BAND_COUNT]; salsa_spatial::CORE_CHANNEL_COUNT],
        error_unobservable: [[false; salsa_spatial::PARAM_BAND_COUNT];
            salsa_spatial::CORE_CHANNEL_COUNT],
        correlation_pairs: Vec::new(),
        reset_predictors: false,
    }
}

fn stream(seconds: u64) -> Vec<u8> {
    let config = SalsaSpatialStreamConfig {
        object_count: 1,
        ..Default::default()
    };
    let mut encoders = vec![
        Encoder::new(48_000, Channels::Stereo, Application::LowDelay).unwrap(),
        Encoder::new(48_000, Channels::Stereo, Application::LowDelay).unwrap(),
        Encoder::new(48_000, Channels::Stereo, Application::LowDelay).unwrap(),
        Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap(),
        Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap(),
        Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap(),
    ];
    let make_packet = |encoders: &mut Vec<Encoder>| {
        let mut substreams = Vec::new();
        for (index, encoder) in encoders.iter_mut().enumerate() {
            let channels = if index < 3 { 2 } else { 1 };
            substreams.push(
                encoder
                    .encode_vec_float(&vec![0.0f32; 960 * channels], 8192)
                    .unwrap(),
            );
        }
        MultistreamPacket::new(substreams)
    };
    let (mut writer, mut bytes) = OggSalsaWriter::new(config, 1, 120).unwrap();
    for index in 0..seconds * 10 {
        let start = index * 4800;
        let packets = (0..5).map(|_| make_packet(&mut encoders)).collect();
        let frame = SalsaSpatialFrame {
            sample_rate: 48_000,
            start_sample: start,
            frames: 4800,
            packets,
            enhancement: EnhancementTile {
                start_sample: start,
                frames: 4800,
                bytes: Vec::new(),
            },
            checkpoint: (start % 48_000 == 0).then(|| checkpoint(start)),
            parameters: None,
        };
        bytes.extend(writer.write_frame(&frame).unwrap());
    }
    bytes.extend(writer.finish());
    bytes
}

fn wait_eos(pipeline: &gst::Pipeline) {
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline finished");
    assert!(
        matches!(message.view(), gst::MessageView::Eos(_)),
        "{message:?}"
    );
}

type Captured = Vec<(Option<gst::ClockTime>, usize)>;

struct Output(Arc<Mutex<Captured>>);

impl Output {
    fn frames(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, size)| size / 8)
            .sum()
    }

    fn first_pts(&self) -> Option<gst::ClockTime> {
        self.0.lock().unwrap().first().and_then(|(pts, _)| *pts)
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

#[test]
fn flushing_seek_is_sample_accurate_and_reuses_checkpoints() {
    salsa_gst_registry::init().expect("register salsa elements");
    let path = std::env::temp_dir().join(format!("salsa-seek-{}.ogg", std::process::id()));
    std::fs::write(&path, stream(3)).unwrap();

    let pipeline = gst::parse::launch(&format!(
        "filesrc location=\"{}\" ! salsaspatialparse name=parse ! queue ! salsaspatialdec ! queue ! fakesink name=sink sync=false",
        path.display()
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let parser_buffers = Arc::new(AtomicUsize::new(0));
    let parser_count = parser_buffers.clone();
    pipeline
        .by_name("parse")
        .unwrap()
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if info.buffer().is_some() {
                parser_count.fetch_add(1, Ordering::Relaxed);
            }
            PadProbeReturn::Ok
        });

    let output = Output(Arc::new(Mutex::new(Vec::new())));
    let sink_output = output.0.clone();
    pipeline
        .by_name("sink")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                sink_output
                    .lock()
                    .unwrap()
                    .push((buffer.pts(), buffer.size()));
            }
            PadProbeReturn::Ok
        });

    pipeline.set_state(gst::State::Paused).unwrap();
    let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(20));
    assert!(result.is_ok(), "{result:?}");

    // The first seek has only the initial checkpoint indexed, so it scans
    // forward from the stream start but must still clip to the exact target.
    pipeline
        .seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::from_mseconds(1550),
            gst::SeekType::Set,
            gst::ClockTime::from_seconds(3),
        )
        .unwrap();
    output.clear();
    pipeline.set_state(gst::State::Playing).unwrap();
    wait_eos(&pipeline);
    assert_eq!(
        output.first_pts(),
        Some(gst::ClockTime::from_mseconds(1550))
    );
    assert_eq!(output.frames(), (3_000 - 1_550) * 48);
    pipeline.set_state(gst::State::Paused).unwrap();
    let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(20));
    assert!(result.is_ok(), "{result:?}");

    // The scan above indexed every checkpoint. This seek starts at the 2 s
    // checkpoint and discards preroll output before the requested 2.55 s.
    pipeline
        .seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::from_mseconds(2550),
            gst::SeekType::Set,
            gst::ClockTime::from_seconds(3),
        )
        .unwrap();
    output.clear();
    pipeline.set_state(gst::State::Playing).unwrap();
    wait_eos(&pipeline);
    assert_eq!(
        output.first_pts(),
        Some(gst::ClockTime::from_mseconds(2550))
    );
    assert_eq!(output.frames(), (3_000 - 2_550) * 48);

    pipeline.set_state(gst::State::Null).unwrap();
    let _ = std::fs::remove_file(path);
}
