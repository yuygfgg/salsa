//! `salsaspatialparse`: GstBaseParse framing for complete salsa-spatial tiles.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gst::format::{ClockTime, GenericFormattedValue};
use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;

use objectaudio_ir::SpatialParser;
use salsa_spatial::{
    BED_CHANNEL_COUNT, MAX_OBJECTS, SAMPLE_RATE, SalsaSpatialFrame, SalsaSpatialParser,
    TILE_SAMPLES,
};

use crate::framing;

/// Decoder preroll inserted before the requested seek target. It covers the
/// Opus 80 ms recommendation plus the local analysis window.
const SEEK_PREROLL_SAMPLES: u64 = 9_600;

/// One byte-addressed tile in the parse index.
#[derive(Debug, Clone, Copy)]
struct IndexEntry {
    offset: u64,
    checkpoint: bool,
}

/// Seek index and the pending request that produced the current segment.
#[derive(Default)]
struct SeekState {
    /// Tile start sample -> first complete packet offset. Checkpoint tiles are
    /// the only valid recovery points after a seek.
    index: BTreeMap<u64, IndexEntry>,
    /// Absolute source sample presented as stream time zero.
    origin: Option<u64>,
    /// Original target time when a seek was snapped back to a checkpoint.
    pending_segment_start: Option<ClockTime>,
    seek_probe: Option<gst::PadProbeId>,
    segment_probe: Option<gst::PadProbeId>,
}

impl SeekState {
    fn reset(&mut self, clear_index: bool) {
        if clear_index {
            self.index.clear();
            self.origin = None;
        }
        self.pending_segment_start = None;
    }

    fn record_tile(&mut self, frame: &SalsaSpatialFrame, offset: u64) {
        self.origin.get_or_insert(frame.start_sample);
        let entry = self.index.entry(frame.start_sample).or_insert(IndexEntry {
            offset,
            checkpoint: frame.checkpoint.is_some(),
        });
        if frame.checkpoint.is_some() {
            entry.checkpoint = true;
        }
    }

    /// Latest checkpoint at or before `sample`, with its byte offset.
    fn checkpoint_at(&self, sample: u64) -> Option<(u64, u64)> {
        self.index
            .range(..=sample)
            .rev()
            .find_map(|(sample, entry)| entry.checkpoint.then_some((*sample, entry.offset)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapsInfo {
    rate: u32,
    object_count: usize,
}

struct State {
    parser: SalsaSpatialParser,
    caps: Option<CapsInfo>,
    sink_object_count: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            parser: SalsaSpatialParser::new(),
            caps: None,
            sink_object_count: 0,
        }
    }
}

impl State {
    fn reset(&mut self) {
        self.parser = SalsaSpatialParser::new();
        self.caps = None;
    }

    /// Drop transport state after a flushing seek but keep the recovery index
    /// and the codec binding recovered before the seek.
    fn reset_parser(&mut self) {
        let binding = self.parser.reader().binding().cloned();
        self.parser = SalsaSpatialParser::new();
        if let Some(binding) = binding {
            self.parser.set_binding(binding);
        }
    }
}

#[derive(Default)]
pub struct SalsaSpatialParse {
    state: Mutex<Box<State>>,
    seek: Arc<Mutex<SeekState>>,
}

impl SalsaSpatialParse {
    fn push_src_caps(&self, info: CapsInfo) -> bool {
        let src_caps = gst::Caps::builder("audio/x-salsa-spatial")
            .field("framed", true)
            .field("alignment", "frame")
            .field("rate", info.rate as i32)
            .field("channels", BED_CHANNEL_COUNT as i32)
            .field("object-count", info.object_count as i32)
            .build();
        salsa_gst::push_parse_caps!(self, src_caps)
    }

    fn frame_info(&self, frames: Option<&SalsaSpatialFrame>, default_objects: usize) -> CapsInfo {
        let rate = frames
            .map(|frame| frame.sample_rate)
            .filter(|rate| *rate > 0)
            .unwrap_or(SAMPLE_RATE);
        CapsInfo {
            rate,
            object_count: default_objects,
        }
    }

    fn install_seek_probes(&self, pad: &gst::Pad) {
        let seek = self.seek.clone();
        let seek_probe = pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
            let Some(event) = info.event_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            if event.type_() != gst::EventType::Seek {
                return gst::PadProbeReturn::Ok;
            }

            let replacement = {
                let gst::EventView::Seek(seek_event) = event.view() else {
                    return gst::PadProbeReturn::Ok;
                };
                let (rate, flags, start_type, start, stop_type, stop) = seek_event.get();
                if (rate - 1.0).abs() > f64::EPSILON || start_type != gst::SeekType::Set {
                    return gst::PadProbeReturn::Ok;
                }
                let GenericFormattedValue::Time(Some(start_time)) = start else {
                    return gst::PadProbeReturn::Ok;
                };
                let GenericFormattedValue::Time(stop_time) = stop else {
                    return gst::PadProbeReturn::Ok;
                };

                let mut state = salsa_gst::lock(&seek);
                let origin = state.origin.unwrap_or(0);
                let target = origin.saturating_add(samples_from_time(start_time, SAMPLE_RATE));
                let preroll_target = target.saturating_sub(SEEK_PREROLL_SAMPLES).max(origin);
                let Some((checkpoint, _)) = state
                    .checkpoint_at(preroll_target)
                    .or_else(|| state.checkpoint_at(target))
                else {
                    state.pending_segment_start = None;
                    return gst::PadProbeReturn::Ok;
                };
                if checkpoint >= target {
                    state.pending_segment_start = None;
                    return gst::PadProbeReturn::Ok;
                }

                state.pending_segment_start = Some(start_time);
                Some(
                    gst::event::Seek::builder(
                        rate,
                        flags,
                        gst::SeekType::Set,
                        time_from_samples(checkpoint - origin, SAMPLE_RATE),
                        stop_type,
                        stop_time,
                    )
                    .seqnum(event.seqnum())
                    .build(),
                )
            };

            if let Some(new_event) = replacement {
                *event = new_event;
            }
            gst::PadProbeReturn::Ok
        });

        let seek = self.seek.clone();
        let segment_probe = pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            let Some(event) = info.event_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            if event.type_() != gst::EventType::Segment {
                return gst::PadProbeReturn::Ok;
            }

            let replacement = {
                let target = salsa_gst::lock(&seek).pending_segment_start.take();
                let Some(target) = target else {
                    return gst::PadProbeReturn::Ok;
                };
                let gst::EventView::Segment(segment) = event.view() else {
                    return gst::PadProbeReturn::Ok;
                };
                let mut segment = segment.segment().clone();
                let start = match segment.start() {
                    GenericFormattedValue::Time(Some(start)) => start,
                    _ => return gst::PadProbeReturn::Ok,
                };
                let stop = match segment.stop() {
                    GenericFormattedValue::Time(stop) => stop,
                    _ => None,
                };
                if target < start || stop.is_some_and(|stop| target > stop) {
                    return gst::PadProbeReturn::Ok;
                }
                segment.set_start(target);
                segment.set_time(target);
                segment.set_position(target);
                Some(
                    gst::event::Segment::builder(&segment)
                        .seqnum(event.seqnum())
                        .build(),
                )
            };

            if let Some(new_event) = replacement {
                *event = new_event;
            }
            gst::PadProbeReturn::Ok
        });

        let mut seek = salsa_gst::lock(&self.seek);
        seek.seek_probe = seek_probe;
        seek.segment_probe = segment_probe;
    }
}

fn samples_from_time(time: ClockTime, rate: u32) -> u64 {
    (u128::from(time.nseconds()) * u128::from(rate) / 1_000_000_000) as u64
}

fn time_from_samples(sample: u64, rate: u32) -> ClockTime {
    ClockTime::from_nseconds((u128::from(sample) * 1_000_000_000 / u128::from(rate)) as u64)
}

#[glib::object_subclass]
impl ObjectSubclass for SalsaSpatialParse {
    const NAME: &'static str = "GstSalsaSpatialParse";
    type Type = super::SalsaSpatialParse;
    type ParentType = gst_base::BaseParse;
}

impl ObjectImpl for SalsaSpatialParse {}

impl GstObjectImpl for SalsaSpatialParse {}

impl ElementImpl for SalsaSpatialParse {
    salsa_gst::element_metadata!(
        "salsa-spatial Parser",
        "Codec/Parser/Audio",
        "Frames complete salsa-spatial Opus multistream tiles carrying object audio"
    );

    salsa_gst::pad_templates!({
        let sink_caps: gst::Caps = format!(
            "audio/x-salsa-spatial, rate=(int){SAMPLE_RATE}, \
             channels=(int){BED_CHANNEL_COUNT}, object-count=(int)[0,{MAX_OBJECTS}]"
        )
        .parse()
        .unwrap();
        let src_caps: gst::Caps = format!(
            "audio/x-salsa-spatial, framed=(boolean)true, \
             alignment=(string)frame, rate=(int){SAMPLE_RATE}, \
             channels=(int){BED_CHANNEL_COUNT}, object-count=(int)[0,{MAX_OBJECTS}]"
        )
        .parse()
        .unwrap();
        vec![
            gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap(),
            gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap(),
        ]
    });
}

impl BaseParseImpl for SalsaSpatialParse {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::SalsaSpatialParse = borrowed.as_ref();
        object.set_min_frame_size(1);
        object.set_has_timing_info(true);
        salsa_gst::lock(&self.state).reset();
        {
            let mut seek = salsa_gst::lock(&self.seek);
            seek.reset(true);
            seek.seek_probe = None;
            seek.segment_probe = None;
        }
        let pad = object.src_pad();
        self.install_seek_probes(pad);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let borrowed = self.obj();
        let object: &super::SalsaSpatialParse = borrowed.as_ref();
        let pad = object.src_pad();
        {
            let mut seek = salsa_gst::lock(&self.seek);
            if let Some(probe) = seek.seek_probe.take() {
                pad.remove_probe(probe);
            }
            if let Some(probe) = seek.segment_probe.take() {
                pad.remove_probe(probe);
            }
            seek.reset(true);
        }
        salsa_gst::lock(&self.state).reset();
        Ok(())
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let structure = caps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(gst::CAT_RUST, "empty sink caps"))?;
        if structure.name() != "audio/x-salsa-spatial" {
            return Err(gst::loggable_error!(
                gst::CAT_RUST,
                "salsaspatialparse requires audio/x-salsa-spatial"
            ));
        }
        let object_count = structure
            .get::<i32>("object-count")
            .ok()
            .filter(|count| *count >= 0)
            .map_or(0, |count| (count as usize).min(MAX_OBJECTS));
        let mut state = salsa_gst::lock(&self.state);
        state.reset();
        state.sink_object_count = object_count;
        drop(state);
        salsa_gst::lock(&self.seek).reset(true);
        Ok(())
    }

    fn handle_frame(
        &self,
        mut frame: gst_base::BaseParseFrame,
    ) -> Result<(gst::FlowSuccess, u32), gst::FlowError> {
        let Some(buffer) = frame.buffer() else {
            return Ok((gst::FlowSuccess::Ok, 0));
        };
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            // Keep the already negotiated stream caps and recovery index across
            // a seek; only the transport parser state is dropped.
            salsa_gst::lock(&self.state).reset_parser();
        }
        let borrowed = self.obj();
        let object: &super::SalsaSpatialParse = borrowed.as_ref();
        let draining = object.is_draining();
        let (size, parsed, info, changed, has_audio, output) = {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            if map.is_empty() {
                return Ok((gst::FlowSuccess::Ok, 0));
            }
            let size = match framing::tile_bytes(&map) {
                Some(size) => size,
                // BaseParse calls us with the remaining bytes while draining,
                // so a short final tile is emitted here rather than dropped.
                None if draining => map.len(),
                None => return Ok((gst::FlowSuccess::Ok, 0)),
            };
            if size == 0 || size > map.len() {
                return Ok((gst::FlowSuccess::Ok, 0));
            }

            let mut state = salsa_gst::lock(&self.state);
            let parsed = state.parser.push_bytes(&map[..size]).map_err(|error| {
                gst::error!(
                    gst::CAT_RUST,
                    "salsaspatialparse: parse failed at {}: {error}",
                    frame.offset()
                );
                gst::FlowError::Error
            })?;
            // Never renegotiate caps from a partial buffer that did not
            // produce a tile: the private configuration may not be parsed yet
            // and the sink caps object count can still be its placeholder zero.
            let (info, changed) = if let Some(tile) = parsed.first() {
                let default_objects = state
                    .parser
                    .config()
                    .map(|config| config.object_count)
                    .filter(|count| *count > 0)
                    .or_else(|| state.caps.map(|caps| caps.object_count))
                    .filter(|count| *count > 0)
                    .unwrap_or(state.sink_object_count);
                let info = self.frame_info(Some(tile), default_objects);
                let changed = state.caps != Some(info);
                if changed {
                    state.caps = Some(info);
                }
                (info, changed)
            } else {
                (
                    state.caps.unwrap_or(CapsInfo {
                        rate: SAMPLE_RATE,
                        object_count: state.sink_object_count,
                    }),
                    false,
                )
            };
            drop(state);

            let mut seek = salsa_gst::lock(&self.seek);
            for tile in &parsed {
                seek.record_tile(tile, frame.offset());
            }
            let origin = seek.origin.unwrap_or(0);
            let output = parsed.first().map(|tile| {
                let pts = time_from_samples(tile.start_sample.saturating_sub(origin), SAMPLE_RATE);
                let duration = time_from_samples(tile.frames as u64, SAMPLE_RATE);
                (map[..size].to_vec(), pts, duration)
            });
            let has_audio = framing::has_audio_page(&map[..size]);
            (size, parsed, info, changed, has_audio, output)
        };
        if parsed.is_empty() && !has_audio {
            // Header-only or end-of-stream Ogg pages carry no complete tile.
            // Consume them without producing an empty output buffer.
            return Ok((gst::FlowSuccess::Ok, size as u32));
        }
        if changed && !self.push_src_caps(info) {
            return Err(gst::FlowError::NotNegotiated);
        }

        if info.rate > 0 {
            let samples = parsed
                .first()
                .map(|frame| frame.frames)
                .filter(|frames| *frames > 0)
                .unwrap_or(TILE_SAMPLES);
            object.set_frame_rate(gst::Fraction::new(info.rate as i32, samples as i32), 0, 0);
        }

        if let Some((data, pts, duration)) = output {
            let mut buffer = gst::Buffer::from_mut_slice(data);
            {
                let buffer = buffer.get_mut().expect("writable parse output");
                buffer.set_pts(pts);
                buffer.set_dts(pts);
                buffer.set_duration(duration);
            }
            frame.set_output_buffer(buffer);
        }

        object.finish_frame(frame, size as u32)?;
        Ok((gst::FlowSuccess::Ok, 0))
    }
}
