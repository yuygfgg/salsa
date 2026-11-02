//! Speaker-renderer streaming tests without external media files.

use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;
use gstsalsaobjectaudio::Oar;
use objectaudio_ir::{ObjectEvent, ObjectState, SpatialPayload, adm_to_q15};
use objectaudio_render::{SpeakerMetadata, SpeakerRenderer, layout_by_name};

const RATE: u32 = 48_000;
const INPUT_CHANNELS: usize = 2;
const OUTPUT_CHANNELS: usize = 6;

struct Harness {
    renderer: Oar,
    source: gst::Pad,
    sink: gst::Pad,
    buffers: Arc<Mutex<Vec<gst::Buffer>>>,
}

impl Harness {
    fn new() -> Self {
        gst::init().unwrap();
        objectaudio_meta::init().unwrap();
        let renderer: Oar = gst::glib::Object::new();
        let buffers = Arc::new(Mutex::new(Vec::new()));
        let captured = buffers.clone();
        let sink = gst::Pad::builder(gst::PadDirection::Sink)
            .chain_function(move |_, _, buffer| {
                captured.lock().unwrap().push(buffer);
                Ok(gst::FlowSuccess::Ok)
            })
            .event_function(|_, _, _| true)
            .build();
        let source = gst::Pad::builder(gst::PadDirection::Src)
            .query_function(|pad, parent, query| {
                if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                    latency.set(
                        true,
                        gst::ClockTime::from_mseconds(10),
                        Some(gst::ClockTime::from_mseconds(20)),
                    );
                    true
                } else {
                    gst::Pad::query_default(pad, parent, query)
                }
            })
            .build();
        sink.set_active(true).unwrap();
        source.set_active(true).unwrap();
        source.link(&renderer.static_pad("sink").unwrap()).unwrap();
        renderer.static_pad("src").unwrap().link(&sink).unwrap();
        renderer.set_state(gst::State::Playing).unwrap();
        assert!(source.push_event(gst::event::StreamStart::new("oar-test")));
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("channels", INPUT_CHANNELS as i32)
            .field("rate", RATE as i32)
            .field("channel-mask", gst::Bitmask(0))
            .features(["meta:ObjectAudioMeta"])
            .build();
        assert!(source.push_event(gst::event::Caps::new(&caps)));
        let harness = Self {
            renderer,
            source,
            sink,
            buffers,
        };
        harness.segment(0);
        harness
    }

    fn segment(&self, start: u64) {
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        segment.set_start(sample_time(start));
        segment.set_position(sample_time(start));
        assert!(self.source.push_event(gst::event::Segment::new(&segment)));
    }

    fn push(&self, buffer: gst::Buffer) {
        assert_eq!(self.source.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    fn finish(&self) -> Vec<gst::Buffer> {
        assert!(self.source.push_event(gst::event::Eos::new()));
        self.take_buffers()
    }

    fn take_buffers(&self) -> Vec<gst::Buffer> {
        std::mem::take(&mut *self.buffers.lock().unwrap())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.renderer.set_state(gst::State::Null).unwrap();
        self.source.set_active(false).unwrap();
        self.sink.set_active(false).unwrap();
    }
}

fn sample_time(samples: u64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(samples * 1_000_000_000 / u64::from(RATE))
}

fn input(start: u64, samples: usize, value: f32) -> gst::Buffer {
    let pcm: Vec<_> = (0..samples)
        .flat_map(|_| [value, 1.0f32])
        .flat_map(f32::to_le_bytes)
        .collect();
    let mut buffer = gst::Buffer::from_mut_slice(pcm);
    let buf = buffer.get_mut().unwrap();
    buf.set_pts(sample_time(start));
    buf.set_duration(sample_time(start + samples as u64) - sample_time(start));
    buf.set_offset(start);
    buf.set_offset_end(start + samples as u64);
    buffer
}

fn object(position: [f32; 3], gain: f32) -> ObjectState {
    let mut state = ObjectState::new(7, position);
    state.gain = gain;
    state
}

fn add_metadata(
    buffer: &mut gst::Buffer,
    offset: usize,
    state: ObjectState,
    events: Vec<ObjectEvent>,
) {
    add_metadata_with_ramp(buffer, offset, state, events, None);
}

fn add_metadata_with_ramp(
    buffer: &mut gst::Buffer,
    offset: usize,
    state: ObjectState,
    events: Vec<ObjectEvent>,
    snapshot_ramp: Option<u32>,
) {
    let mut payload = SpatialPayload::new(RATE, vec![state], events);
    if let Some(ramp) = snapshot_ramp {
        payload = payload.with_snapshot_ramp(ramp);
    }
    objectaudio_meta::add_spatial(
        buffer.get_mut().unwrap(),
        &payload,
        offset,
        INPUT_CHANNELS * 4,
    )
    .unwrap();
}

fn event(offset: u32, state: ObjectState) -> ObjectEvent {
    ObjectEvent {
        offset,
        ramp_samples: 64,
        object: state.id,
        state,
    }
}

fn pcm(buffers: &[gst::Buffer]) -> Vec<f32> {
    buffers
        .iter()
        .flat_map(|buffer| {
            buffer
                .map_readable()
                .unwrap()
                .as_slice()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn assert_timeline(buffers: &[gst::Buffer], start: u64, total: usize) {
    let mut samples = 0;
    for (index, buffer) in buffers.iter().enumerate() {
        assert_eq!(buffer.size() % (OUTPUT_CHANNELS * 4), 0);
        let count = buffer.size() / (OUTPUT_CHANNELS * 4);
        if index + 1 != buffers.len() {
            assert_eq!(count % 32, 0);
        }
        assert!(
            buffer
                .pts()
                .unwrap()
                .nseconds()
                .abs_diff(sample_time(start + samples as u64).nseconds())
                <= 1
        );
        assert_eq!(buffer.offset(), start + samples as u64);
        samples += count;
        assert_eq!(buffer.offset_end(), start + samples as u64);
        // Separate input PTS and duration conversions can round by one nanosecond.
        let end = buffer.pts().unwrap() + buffer.duration().unwrap();
        assert!(
            end.nseconds()
                .abs_diff(sample_time(start + samples as u64).nseconds())
                <= 1
        );
        assert!(objectaudio_meta::get_spatial(buffer).is_none());
    }
    assert_eq!(samples, total);
}

#[test]
fn snapshot_ramp_controls_the_initial_transition() {
    let harness = Harness::new();
    let mut buffer = input(0, 64, 0.5);
    add_metadata_with_ramp(
        &mut buffer,
        0,
        object([-1.0, 1.0, 0.0], 1.0),
        Vec::new(),
        Some(64),
    );
    harness.push(buffer);
    let buffers = harness.finish();
    let samples = pcm(&buffers);
    let left = |frame: usize| samples[frame * OUTPUT_CHANNELS];
    assert!(
        left(0).abs() < left(63).abs() * 0.1,
        "left gain should ramp from zero: first={} last={}",
        left(0),
        left(63)
    );
    assert!(left(63).abs() > 0.1, "left target was not reached");

    let harness = Harness::new();
    let mut buffer = input(0, 64, 0.5);
    add_metadata_with_ramp(
        &mut buffer,
        0,
        object([-1.0, 1.0, 0.0], 1.0),
        Vec::new(),
        None,
    );
    harness.push(buffer);
    let buffers = harness.finish();
    let samples = pcm(&buffers);
    assert!(
        samples[0].abs() > 0.1,
        "without a snapshot ramp the first sample should already be panned"
    );
}

#[test]
fn fragmented_pcm_and_metadata_match_the_block_renderer() {
    let harness = Harness::new();
    let states = [
        object([-1.0, 1.0, 0.0], 1.0),
        object([1.0, 1.0, 0.0], 0.75),
        object([-1.0, -1.0, 0.0], 0.5),
        object([1.0, -1.0, 0.0], 0.25),
        object([0.0, 1.0, 0.0], 1.0),
    ];
    let mut first = input(96_000, 24, 0.25);
    first
        .get_mut()
        .unwrap()
        .set_flags(gst::BufferFlags::DISCONT);
    add_metadata(
        &mut first,
        0,
        states[0].clone(),
        vec![event(23, states[1].clone())],
    );
    harness.push(first);
    assert!(
        harness.take_buffers().is_empty(),
        "partial block must wait for more PCM"
    );

    let mut second = input(96_024, 1105, 0.25);
    add_metadata(
        &mut second,
        9,
        states[1].clone(),
        vec![event(1090, states[2].clone())],
    );
    harness.push(second);
    let mut third = input(97_129, 408, 0.25);
    add_metadata(
        &mut third,
        4,
        states[3].clone(),
        vec![event(200, states[4].clone())],
    );
    harness.push(third);
    let buffers = harness.finish();
    assert_timeline(&buffers, 96_000, 1537);
    assert!(buffers[0].flags().contains(gst::BufferFlags::DISCONT));
    assert!(
        buffers[1..]
            .iter()
            .all(|buffer| !buffer.flags().contains(gst::BufferFlags::DISCONT))
    );

    let positions: Vec<_> = states
        .iter()
        .map(|state| [adm_to_q15(state.position)])
        .collect();
    let gains: Vec<_> = states.iter().map(|state| [f64::from(state.gain)]).collect();
    let events: Vec<_> = [(0, 0), (23, 1), (33, 1), (1114, 2), (1133, 3), (1329, 4)]
        .into_iter()
        .map(|(offset, index)| SpeakerMetadata {
            offset,
            ramp_samples: 64,
            positions_q15: &positions[index],
            object_gains: Some(&gains[index]),
            region_indices: None,
            height_enabled: None,
        })
        .collect();
    let mut renderer = SpeakerRenderer::new(layout_by_name("5.1").unwrap(), 1);
    let input: Vec<_> = (0..1568).flat_map(|_| [0.25, 1.0]).collect();
    let mut expected = vec![0.0; 1568 * OUTPUT_CHANNELS];
    renderer
        .process(&input, 1568, &events, &mut expected)
        .unwrap();
    let expected: Vec<_> = expected[..1537 * OUTPUT_CHANNELS]
        .iter()
        .map(|value| *value as f32)
        .collect();
    assert_eq!(
        pcm(&buffers),
        expected,
        "reblocking must preserve metadata timing and ramps"
    );
}

#[test]
fn eos_preserves_a_stream_shorter_than_one_block() {
    let harness = Harness::new();
    harness.push(input(96_000, 24, 0.5));
    assert!(harness.take_buffers().is_empty());
    let buffers = harness.finish();
    assert_timeline(&buffers, 96_000, 24);
    assert!(
        pcm(&buffers)
            .as_chunks::<OUTPUT_CHANNELS>()
            .0
            .iter()
            .all(|frame| frame[3] == 0.5)
    );
}

#[test]
fn discont_discards_partial_pcm_and_metadata() {
    let harness = Harness::new();
    let mut before = input(0, 56, 0.75);
    add_metadata(&mut before, 23, object([-1.0, 1.0, 0.0], 1.0), Vec::new());
    harness.push(before);
    assert_timeline(&harness.take_buffers(), 0, 32);
    let mut after = input(96_000, 40, 0.25);
    after
        .get_mut()
        .unwrap()
        .set_flags(gst::BufferFlags::DISCONT);
    harness.push(after);
    let buffers = harness.finish();
    assert_timeline(&buffers, 96_000, 40);
    assert!(buffers[0].flags().contains(gst::BufferFlags::DISCONT));
    for frame in pcm(&buffers).as_chunks::<OUTPUT_CHANNELS>().0 {
        assert_eq!(*frame, [0.0, 0.0, 0.0, 0.25, 0.0, 0.0]);
    }
}

#[test]
fn flush_discards_partial_pcm_and_metadata() {
    let harness = Harness::new();
    let mut before = input(0, 56, 0.75);
    add_metadata(&mut before, 23, object([-1.0, 1.0, 0.0], 1.0), Vec::new());
    harness.push(before);
    assert_timeline(&harness.take_buffers(), 0, 32);
    assert!(harness.source.push_event(gst::event::FlushStart::new()));
    assert!(harness.source.push_event(gst::event::FlushStop::new(true)));
    harness.segment(96_000);
    harness.push(input(96_000, 40, 0.25));
    let buffers = harness.finish();
    assert_timeline(&buffers, 96_000, 40);
    for frame in pcm(&buffers).as_chunks::<OUTPUT_CHANNELS>().0 {
        assert_eq!(*frame, [0.0, 0.0, 0.0, 0.25, 0.0, 0.0]);
    }
}

#[test]
fn segment_drains_partial_pcm_before_the_new_timeline() {
    let harness = Harness::new();
    harness.push(input(0, 24, 0.75));
    harness.segment(96_000);
    let before = harness.take_buffers();
    assert_timeline(&before, 0, 24);
    harness.push(input(96_000, 40, 0.25));
    let after = harness.finish();
    assert_timeline(&after, 96_000, 40);
}

#[test]
fn unknown_timestamps_and_offsets_remain_unknown() {
    let harness = Harness::new();
    let mut buffer = input(0, 24, 0.5);
    let buf = buffer.get_mut().unwrap();
    buf.set_pts(None);
    buf.set_offset(u64::MAX);
    buf.set_offset_end(u64::MAX);
    harness.push(buffer);
    let buffers = harness.finish();
    assert_eq!(buffers.len(), 1);
    assert_eq!(buffers[0].pts(), None);
    assert_eq!(buffers[0].offset(), u64::MAX);
    assert_eq!(buffers[0].offset_end(), u64::MAX);
}

#[test]
fn latency_includes_the_partial_block() {
    let harness = Harness::new();
    let mut query = gst::query::Latency::new();
    assert!(
        harness
            .renderer
            .static_pad("src")
            .unwrap()
            .query(&mut query)
    );
    let (live, min, max) = query.result();
    assert!(live);
    assert_eq!(min, gst::ClockTime::from_mseconds(10) + sample_time(31));
    assert_eq!(
        max,
        Some(gst::ClockTime::from_mseconds(20) + sample_time(31))
    );
}

#[test]
fn metadata_offset_stays_relative_to_the_input_buffer() {
    let harness = Harness::new();
    harness.renderer.set_property("metadata-offset", 16u32);
    harness.push(input(0, 24, 0.0));
    let mut buffer = input(24, 40, 0.0);
    add_metadata(&mut buffer, 9, object([-1.0, 1.0, 0.0], 1.0), Vec::new());
    harness.push(buffer);
    harness.push(input(64, 32, 0.0));
    let buffers = harness.finish();
    assert_timeline(&buffers, 0, 96);
    let rendered = pcm(&buffers);
    // 24 queued samples + 9 snapshot samples + 16 extra samples = 49.
    // The renderer rounds this event to its block boundary at sample 64.
    assert!(
        rendered[..64 * OUTPUT_CHANNELS]
            .iter()
            .all(|value| *value == 0.0)
    );
    assert!(
        rendered[64 * OUTPUT_CHANNELS..]
            .iter()
            .any(|value| *value != 0.0)
    );
}

#[test]
fn incomplete_pcm_frames_are_rejected() {
    let harness = Harness::new();
    let mut buffer = gst::Buffer::with_size(INPUT_CHANNELS * 4 + 1).unwrap();
    buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    assert_eq!(harness.source.push(buffer), Err(gst::FlowError::Error));
    assert!(harness.finish().is_empty());
}
