//! Hermetic `salsadamfsink` round-trip through a generated DAMF fixture.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gst::glib::prelude::*;
use gst::prelude::*;
use gstreamer as gst;

use objectaudio_ir::{ObjectEvent, ObjectState, SceneTimeline, SpatialPayload, Transition};

fn init() {
    let _ = salsa_gst_registry::init();
}

fn unique_dir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "salsa-gst-damf-sink-{}-{stamp}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write_fixture(dir: &Path, frames: usize) -> PathBuf {
    let manifest = dir.join("in.atmos");
    let mut file = std::fs::File::create(&manifest).unwrap();
    file.write_all(
        br#"version: 0.5.1
presentations:
  - audio: in.atmos.audio
    metadata: in.atmos.metadata
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

    let mut metadata = std::fs::File::create(dir.join("in.atmos.metadata")).unwrap();
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
  - ID: 11
    samplePos: 8
    pos: [0.5, 0.5, 0]
    gain: -6
    rampLength: 4
"#,
        )
        .unwrap();

    let channels = 3usize;
    let data_len = channels * frames * 3;
    let mut caf = std::fs::File::create(dir.join("in.atmos.audio")).unwrap();
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
fn sink_writes_a_readable_damf_presentation() {
    use objectaudio_damf::{CafReader, DamfSource, Manifest, Metadata};

    init();
    let dir = unique_dir();
    let frames = 16usize;
    let manifest = write_fixture(&dir, frames);
    let output = dir.join("out.atmos");
    let pipeline = gst::parse::launch(&format!(
        "salsadamfsrc location=\"{}\" ! salsadamfsink location=\"{}\"",
        manifest.display(),
        output.display()
    ))
    .expect("parse pipeline")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");
    let guard = Guard(pipeline);
    run(guard.0.clone());

    let written = Manifest::load(&output).unwrap();
    assert_eq!(written.beds.len(), 1);
    assert_eq!(written.beds[0].name, "LFE");
    assert_eq!(written.beds[0].id, 3);
    assert_eq!(written.objects.len(), 2);
    assert_eq!(written.objects[0].id, 10);
    assert_eq!(written.objects[1].id, 11);

    let mut caf = CafReader::open(&written.audio).unwrap();
    assert_eq!(caf.info().channels, 3);
    assert_eq!(caf.info().sample_rate, 48_000);
    assert_eq!(caf.total_frames(), Some(frames as u64));
    caf.seek_to_frame(0).unwrap();
    let pcm = caf.read_frames(frames).unwrap();
    assert_eq!(pcm.len(), frames * 3);
    for frame in 0..frames {
        assert!(pcm[frame * 3].abs() < 1e-6, "LFE must remain silent");
        assert!((pcm[frame * 3 + 1] - 0.25).abs() < 1e-6);
        let expected = -0.25 + frame as f32 * 0.01;
        assert!((pcm[frame * 3 + 2] - expected).abs() < 1e-6);
    }

    let metadata = Metadata::load(&written.metadata).unwrap();
    assert_eq!(metadata.sample_rate, 48_000);
    assert!(metadata.events.iter().any(|event| {
        event.id == 10 && event.sample_pos == 0 && event.position == Some([-1.0, 0.0, 0.0])
    }));
    assert!(metadata.events.iter().any(|event| {
        event.id == 11 && event.sample_pos == 0 && event.position == Some([1.0, 0.0, 0.0])
    }));
    assert!(metadata.events.iter().any(|event| {
        event.id == 11
            && event.sample_pos == 8
            && event.position == Some([0.5, 0.5, 0.0])
            && event.ramp_length == Some(4)
            && event.gain.is_some()
    }));

    let mut source = DamfSource::open(&output).unwrap();
    let info = source.info();
    assert_eq!(info.output_channels, 3);
    assert_eq!(info.objects.len(), 2);
    assert_eq!(info.objects[1].position, [1.0, 0.0, 0.0]);
    let block = source.read_block(frames).unwrap().unwrap();
    assert_eq!(block.frames, frames);
    assert!(block.payload.events.iter().any(|event| {
        event.state.id == 2 && event.offset == 8 && event.transition.samples() == 4
    }));
    assert!(source.read_block(frames).unwrap().is_none());

    std::fs::remove_dir_all(dir).unwrap();
}

struct Export {
    dir: PathBuf,
    sink: gst::Element,
    source: gst::Pad,
    frames: u64,
    channels: usize,
}

impl Export {
    fn new(channels: usize) -> Self {
        init();
        let dir = unique_dir();
        let sink = gst::ElementFactory::make("salsadamfsink")
            .property("location", dir.join("out.atmos").to_str().unwrap())
            .property("async", false)
            .build()
            .unwrap();
        let source = gst::Pad::builder(gst::PadDirection::Src).build();
        source.set_active(true).unwrap();
        source.link(&sink.static_pad("sink").unwrap()).unwrap();
        sink.set_state(gst::State::Playing).unwrap();
        assert!(source.push_event(gst::event::StreamStart::new("damf-export-test")));
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("channels", channels as i32)
            .field("rate", 48_000i32)
            .field("channel-mask", gst::Bitmask(0))
            .features(["meta:ObjectAudioMeta"])
            .build();
        assert!(source.push_event(gst::event::Caps::new(&caps)));
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        assert!(source.push_event(gst::event::Segment::new(&segment)));
        Self {
            dir,
            sink,
            source,
            frames: 0,
            channels,
        }
    }

    fn push(&mut self, payload: &SpatialPayload, frames: usize) {
        let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; frames * self.channels * 4]);
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(
            self.frames * 1_000_000_000 / 48_000,
        ));
        buffer_ref.set_duration(gst::ClockTime::from_nseconds(
            frames as u64 * 1_000_000_000 / 48_000,
        ));
        objectaudio_meta::add_spatial(buffer_ref, payload, self.channels * 4).unwrap();
        assert_eq!(self.source.push(buffer), Ok(gst::FlowSuccess::Ok));
        self.frames += frames as u64;
    }

    fn finish(&self) -> SceneTimeline {
        assert!(self.source.push_event(gst::event::Eos::new()));
        let mut source = objectaudio_damf::DamfSource::open(self.dir.join("out.atmos")).unwrap();
        assert_eq!(source.info().total_frames, Some(self.frames));
        let block = source.read_block(self.frames as usize).unwrap().unwrap();
        SceneTimeline::from_payload(&block.payload, 0).unwrap()
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        let _ = self.sink.set_state(gst::State::Null);
        let _ = self.source.set_active(false);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn assert_trajectory(actual: &SceneTimeline, expected: &SceneTimeline, start: i64, frames: usize) {
    assert_eq!(actual.object_count(), expected.object_count());
    for sample in 0..frames as i64 {
        for index in 0..expected.object_count() {
            let actual = actual.state_at(index, sample);
            let expected = expected.state_at(index, start + sample);
            assert_eq!(
                actual.enabled, expected.enabled,
                "sample {sample}, object {index}"
            );
            for (property, actual, expected) in [
                ("x", actual.position[0], expected.position[0]),
                ("y", actual.position[1], expected.position[1]),
                ("z", actual.position[2], expected.position[2]),
                ("gain", actual.gain, expected.gain),
                (
                    "size",
                    actual.size.unwrap_or(0.0),
                    expected.size.unwrap_or(0.0),
                ),
            ] {
                assert!(
                    (actual - expected).abs() < 1e-6,
                    "sample {sample}, object {index}, {property}: got {actual}, expected {expected}"
                );
            }
        }
    }
}

fn ramp_payload(transition: Transition, offset: i64) -> SpatialPayload {
    let initial = ObjectState {
        size: Some(0.2),
        ..ObjectState::new(1, [-1.0, 0.0, 0.0])
    };
    let target = ObjectState {
        position: [1.0, 1.0, 1.0],
        gain: 0.25,
        size: Some(0.8),
        ..initial
    };
    SpatialPayload::new(
        48_000,
        vec![initial],
        vec![ObjectEvent {
            offset,
            transition,
            state: target,
        }],
    )
}

#[test]
fn sink_preserves_all_property_ramps() {
    let payload = ramp_payload(Transition::All(64), 32);
    let expected = SceneTimeline::from_payload(&payload, 0).unwrap();
    assert_eq!(expected.state_at(0, 48).gain, 0.8125);
    let mut export = Export::new(2);
    export.push(&payload, 128);
    assert_trajectory(&export.finish(), &expected, 0, 128);
}

#[test]
fn sink_preserves_motion_after_source_seek() {
    let mut export = Export::new(3);
    let manifest = write_fixture(&export.dir, 128);
    std::fs::write(export.dir.join("in.atmos.metadata"),
        "sampleRate: 48000\nevents:\n  - ID: 10\n    samplePos: 0\n    pos: [-1, 0, 0]\n  - ID: 10\n    samplePos: 16\n    pos: [1, 0, 0]\n    rampLength: 64\n  - ID: 11\n    samplePos: 0\n    pos: [0, 1, 0]\n"
    ).unwrap();
    let mut source = objectaudio_damf::DamfSource::open(manifest).unwrap();
    let full = source.read_block(128).unwrap().unwrap();
    let expected = SceneTimeline::from_payload(&full.payload, 0).unwrap();
    source.seek(32).unwrap();
    assert_eq!(expected.state_at(0, 32).position[0], -0.5);
    while let Some(block) = source.read_block(17).unwrap() {
        export.push(&block.payload, block.frames);
    }
    assert_trajectory(&export.finish(), &expected, 32, 96);
}

#[test]
fn sink_evaluates_negative_offsets_before_export() {
    for transition in [Transition::Position(64), Transition::All(64)] {
        let payload = ramp_payload(transition, 16);
        let expected = SceneTimeline::from_payload(&payload, 0).unwrap();
        let sliced = payload.slice(32, 96).unwrap();
        assert_eq!(sliced.events[0].offset, -16);
        assert_eq!(expected.state_at(0, 32).position[0], -0.5);
        let mut export = Export::new(2);
        export.push(&sliced, 96);
        assert_trajectory(&export.finish(), &expected, 32, 96);
    }
}

#[test]
fn sink_preserves_ramps_across_replayed_slice_prefixes() {
    for transition in [Transition::Position(64), Transition::All(64)] {
        let payload = ramp_payload(transition, 16);
        let expected = SceneTimeline::from_payload(&payload, 0).unwrap();
        for block_frames in [1, 7, 32, 47] {
            let mut export = Export::new(2);
            for start in (0..128).step_by(block_frames) {
                let frames = block_frames.min(128 - start);
                export.push(&payload.slice(start as i64, frames).unwrap(), frames);
            }
            assert_trajectory(&export.finish(), &expected, 0, 128);
        }
    }
}

#[test]
fn sink_preserves_target_snapshots_and_interrupted_ramps() {
    let payload = ramp_payload(Transition::All(64), 32);
    let initial = SpatialPayload::new(48_000, payload.objects.clone(), Vec::new());
    let target = SpatialPayload::new(48_000, vec![payload.events[0].state], Vec::new())
        .with_snapshot_ramp(64);
    let mut interrupted = ramp_payload(Transition::Position(16), 0);
    interrupted.objects[0].gain = 0.0;
    interrupted.events[0].state = interrupted.objects[0];
    interrupted.snapshot = objectaudio_ir::Snapshot::Continue;
    let mut expected = SceneTimeline::from_payload(&initial, 0).unwrap();
    expected.submit(&target, 32).unwrap();
    expected.submit(&interrupted, 65).unwrap();
    let mut export = Export::new(2);
    export.push(&initial, 32);
    export.push(&target, 33);
    export.push(&interrupted, 63);
    assert_trajectory(&export.finish(), &expected, 0, 128);
}

#[test]
fn sink_preserves_first_target_fade_from_silence() {
    let payload = SpatialPayload::new(
        48_000,
        ramp_payload(Transition::Step, 0).objects,
        Vec::new(),
    )
    .with_snapshot_ramp(64);
    let expected = SceneTimeline::from_payload(&payload, 0).unwrap();
    let mut export = Export::new(2);
    export.push(&payload, 96);
    assert_trajectory(&export.finish(), &expected, 0, 96);
}

#[test]
fn sink_preserves_a_later_state_and_target_at_the_same_time() {
    let initial = SpatialPayload::new(48_000, vec![ObjectState::new(1, [0.0; 3])], Vec::new());
    let update = ramp_payload(Transition::Position(32), 0);
    let mut expected = SceneTimeline::from_payload(&initial, 0).unwrap();
    expected.submit(&update, 32).unwrap();
    let mut export = Export::new(2);
    export.push(&initial, 32);
    export.push(&update, 64);
    assert_trajectory(&export.finish(), &expected, 0, 96);
}
