//! The non-blocking decoder tap.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;

use super::timeline::{DEFAULT_TRAIL_SECONDS, TapEvent, Timeline, VisualSnapshot};

/// Failures while attaching the visual tap to a decoder.
#[derive(Debug, thiserror::Error)]
pub enum TapError {
    #[error("the decoder has no static src pad")]
    NoSrcPad,
    #[error("GStreamer refused the pad probe")]
    ProbeRejected,
}

/// One bounded queue shared with the streaming thread.
///
/// The probe only ever calls `try_send`; a full queue drops the newest frame
/// and increments a counter instead of blocking the audio pipeline.
struct Shared {
    sender: SyncSender<TapEvent>,
    enabled: AtomicBool,
    epoch: AtomicU64,
    caps_channels: AtomicU32,
    caps_rate: AtomicU32,
    dropped: AtomicU64,
    segment_pending: AtomicBool,
}

impl Shared {
    fn send(&self, event: TapEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn on_buffer(&self, buffer: &gst::BufferRef) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let Some(pts) = buffer.pts() else {
            return;
        };
        let snapshots = objectaudio_meta::iter_spatial(buffer);
        let channels = snapshots
            .first()
            .map_or(self.caps_channels.load(Ordering::Relaxed), |snapshot| {
                (snapshot.bpf / 4) as u32
            });
        let payloads: Vec<_> = snapshots
            .into_iter()
            .map(|snapshot| snapshot.payload)
            .collect();
        let levels = levels_from_buffer(buffer, channels as usize);
        let pending_segment = self.segment_pending.load(Ordering::Relaxed);
        let mut discont = buffer.flags().contains(gst::BufferFlags::DISCONT);
        if pending_segment && !payloads.is_empty() {
            self.segment_pending.store(false, Ordering::Relaxed);
            discont = true;
        }
        self.send(TapEvent::Buffer {
            epoch: self.epoch.load(Ordering::Relaxed),
            pts_ns: pts.nseconds(),
            payloads,
            levels,
            discont,
        });
    }

    fn on_event(&self, event: &gst::Event) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let epoch = self.epoch.load(Ordering::Relaxed);
        match event.view() {
            gst::EventView::Caps(caps) => {
                let Some(structure) = caps.structure() else {
                    return;
                };
                let channels = int_field(structure, "channels").unwrap_or(0).max(0) as u32;
                let rate = int_field(structure, "rate").unwrap_or(0).max(0) as u32;
                self.caps_channels.store(channels, Ordering::Relaxed);
                self.caps_rate.store(rate, Ordering::Relaxed);
                self.send(TapEvent::Caps {
                    epoch,
                    channels,
                    rate,
                });
            }
            gst::EventView::FlushStart(_) => self.send(TapEvent::Flush { epoch, stop: false }),
            gst::EventView::FlushStop(_) => self.send(TapEvent::Flush { epoch, stop: true }),
            gst::EventView::Segment(_) | gst::EventView::StreamStart(_) => {
                self.send(TapEvent::Segment { epoch })
            }
            gst::EventView::Eos(_) => self.send(TapEvent::Eos { epoch }),
            _ => {}
        }
    }
}

fn int_field(structure: &gst::StructureRef, name: &str) -> Option<i32> {
    if let Ok(value) = structure.get::<i32>(name) {
        return Some(value);
    }
    if let Ok(range) = structure.get::<gst::IntRange<i32>>(name) {
        return Some(range.max());
    }
    None
}

fn levels_from_buffer(buffer: &gst::BufferRef, channels: usize) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }
    let Ok(map) = buffer.map_readable() else {
        return vec![0.0; channels];
    };
    let data = map.as_slice();
    let frame_bytes = channels * 4;
    let frames = data.len() / frame_bytes;
    if frames == 0 {
        return vec![0.0; channels];
    }
    // Bound the work for very wide object streams. Levels are a visualization,
    // not a measurement, so subsampling is acceptable. Keep decoder channel
    // order: LFE first, followed by the objects.
    let stride = if channels > 256 {
        16
    } else if channels > 64 {
        4
    } else {
        1
    };
    let mut sums = vec![0.0f64; channels];
    for frame in (0..frames).step_by(stride) {
        let base = frame * frame_bytes;
        for (channel, sum) in sums.iter_mut().enumerate() {
            let offset = base + channel * 4;
            let sample = f32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            if sample.is_finite() {
                *sum += f64::from(sample) * f64::from(sample);
            }
        }
    }
    let count = frames.div_ceil(stride) as f64;
    sums.iter().map(|sum| (sum / count).sqrt() as f32).collect()
}

struct Attachment {
    pad: gst::Pad,
    id: gst::PadProbeId,
}

/// A bounded, lossy bridge from a decoder src pad to a UI timeline.
///
/// `snapshot_at` must be called from one thread (normally the UI thread). The
/// probe closure itself never locks the timeline and never blocks.
pub struct VisualTap {
    shared: Arc<Shared>,
    receiver: Mutex<Receiver<TapEvent>>,
    timeline: Mutex<Timeline>,
    attachment: Mutex<Option<Attachment>>,
}

impl VisualTap {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(8);
        Self {
            shared: Arc::new(Shared {
                sender,
                enabled: AtomicBool::new(true),
                epoch: AtomicU64::new(0),
                caps_channels: AtomicU32::new(0),
                caps_rate: AtomicU32::new(48_000),
                dropped: AtomicU64::new(0),
                segment_pending: AtomicBool::new(false),
            }),
            receiver: Mutex::new(receiver),
            timeline: Mutex::new(Timeline::default()),
            attachment: Mutex::new(None),
        }
    }

    /// Attach to the decoder's static src pad. Any previous probe is removed;
    /// the last snapshot is kept and marked stale until new data arrives.
    pub fn attach_decoder(&self, decoder: &gst::Element) -> Result<(), TapError> {
        self.detach();
        let pad = decoder.static_pad("src").ok_or(TapError::NoSrcPad)?;
        let shared = self.shared.clone();
        let id = pad
            .add_probe(
                gst::PadProbeType::BUFFER
                    | gst::PadProbeType::EVENT_DOWNSTREAM
                    | gst::PadProbeType::EVENT_FLUSH,
                move |_pad, info| {
                    if let Some(buffer) = info.buffer() {
                        shared.on_buffer(buffer);
                    }
                    if let Some(event) = info.event() {
                        shared.on_event(event);
                    }
                    gst::PadProbeReturn::Ok
                },
            )
            .ok_or(TapError::ProbeRejected)?;
        *self.attachment.lock().unwrap() = Some(Attachment { pad, id });
        self.timeline.lock().unwrap().mark_stale();
        Ok(())
    }

    pub fn detach(&self) {
        if let Some(attachment) = self.attachment.lock().unwrap().take() {
            attachment.pad.remove_probe(attachment.id);
        }
        self.timeline.lock().unwrap().mark_stale();
    }

    /// Enable or disable probing. Disabling is an atomic check in the probe,
    /// so it takes effect without touching the audio thread.
    pub fn set_enabled(&self, enabled: bool) {
        self.shared.enabled.store(enabled, Ordering::Relaxed);
        if enabled {
            self.reset();
        }
    }

    pub fn enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Relaxed)
    }

    /// Keep the last snapshot until the next buffer, then start a new segment.
    /// This is used when a pipeline is rebuilt for an output switch.
    pub fn begin_segment(&self) {
        self.shared.segment_pending.store(true, Ordering::Relaxed);
        self.timeline.lock().unwrap().mark_stale();
    }

    /// Drop the timeline and invalidate in-flight events.
    pub fn reset(&self) {
        self.shared.segment_pending.store(false, Ordering::Relaxed);
        let epoch = self.shared.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.timeline.lock().unwrap().reset(epoch);
    }

    /// Number of frames the bounded queue had to drop while the UI was busy.
    pub fn dropped_frames(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    pub fn snapshot_at(&self, time: gst::ClockTime) -> Option<VisualSnapshot> {
        self.drain();
        self.timeline
            .lock()
            .unwrap()
            .snapshot_at(time.nseconds(), DEFAULT_TRAIL_SECONDS)
    }

    fn drain(&self) {
        let receiver = self.receiver.lock().unwrap();
        let mut timeline = self.timeline.lock().unwrap();
        while let Ok(event) = receiver.try_recv() {
            timeline.handle(event);
        }
    }
}

impl Default for VisualTap {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VisualTap {
    fn drop(&mut self) {
        if let Ok(attachment) = self.attachment.get_mut()
            && let Some(attachment) = attachment.take()
        {
            attachment.pad.remove_probe(attachment.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer_app as gst_app;
    use objectaudio_ir::{ObjectEvent, ObjectState, SpatialPayload};
    use std::time::{Duration, Instant};

    fn init() {
        let _ = gst::init();
    }

    fn caps() -> gst::Caps {
        gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("channels", 3i32)
            .field("rate", 48_000i32)
            .field("channel-mask", gst::Bitmask(0))
            .build()
    }

    fn frame_buffer(frames: usize, channels: &[f32]) -> gst::Buffer {
        let data: Vec<u8> = (0..frames)
            .flat_map(|_| channels.iter().flat_map(|sample| sample.to_le_bytes()))
            .collect();
        gst::Buffer::from_slice(data)
    }

    #[test]
    fn levels_keep_lfe_and_objects_in_decoder_channel_order() {
        init();
        for channels in [vec![0.5, 0.0, 0.0], vec![0.0, 0.25, -0.125], vec![0.5]] {
            let buffer = frame_buffer(32, &channels);
            let expected: Vec<_> = channels.iter().map(|level| level.abs()).collect();
            assert_eq!(levels_from_buffer(&buffer, channels.len()), expected);
        }
    }

    #[test]
    fn missing_and_invalid_samples_do_not_produce_levels() {
        init();
        let empty = frame_buffer(0, &[0.0; 3]);
        assert_eq!(levels_from_buffer(&empty, 3), vec![0.0; 3]);
        assert!(levels_from_buffer(&empty, 0).is_empty());
        let invalid = frame_buffer(32, &[f32::NAN, f32::INFINITY, f32::NEG_INFINITY]);
        assert_eq!(levels_from_buffer(&invalid, 3), vec![0.0; 3]);
    }

    #[test]
    fn a_full_queue_drops_without_blocking() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shared = Shared {
            sender,
            enabled: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
            caps_channels: AtomicU32::new(0),
            caps_rate: AtomicU32::new(48_000),
            dropped: AtomicU64::new(0),
            segment_pending: AtomicBool::new(false),
        };
        shared.send(TapEvent::Eos { epoch: 0 });
        let started = Instant::now();
        for _ in 0..1000 {
            shared.send(TapEvent::Eos { epoch: 0 });
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn a_test_buffer_updates_the_timeline() {
        init();
        let (sender, receiver) = mpsc::sync_channel(8);
        let shared = Shared {
            sender,
            enabled: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
            caps_channels: AtomicU32::new(3),
            caps_rate: AtomicU32::new(48_000),
            dropped: AtomicU64::new(0),
            segment_pending: AtomicBool::new(false),
        };
        let mut buffer = frame_buffer(32, &[0.5, 0.25, 0.125]);
        let payload = SpatialPayload::new(
            48_000,
            vec![
                ObjectState::new(1, [0.0, 0.0, 0.0]),
                ObjectState::new(2, [1.0, 0.0, 0.0]),
            ],
            vec![ObjectEvent {
                offset: 16,
                transition: objectaudio_ir::Transition::All(0),
                state: ObjectState::new(1, [1.0, 1.0, 0.0]),
            }],
        );
        objectaudio_meta::add_spatial(buffer.get_mut().unwrap(), &payload, 12).unwrap();
        objectaudio_meta::add_spatial(
            buffer.get_mut().unwrap(),
            &payload.with_snapshot_offset(32),
            12,
        )
        .unwrap();
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_mseconds(100));
        shared.on_buffer(&buffer);
        let event = receiver.recv().unwrap();
        let TapEvent::Buffer {
            payloads,
            levels,
            pts_ns,
            ..
        } = event
        else {
            panic!("expected a buffer event");
        };
        assert_eq!(pts_ns, 100_000_000);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0].objects.len(), 2);
        assert_eq!(payloads[1].snapshot_offset, 32);
        assert!(receiver.try_recv().is_err());
        assert_eq!(levels, vec![0.5, 0.25, 0.125]);
    }

    #[test]
    fn attaching_to_an_appsrc_produces_a_snapshot() {
        init();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("appsrc").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &sink]).unwrap();
        gst::Element::link_many([&src, &sink]).unwrap();
        src.set_property("caps", caps());
        let tap = VisualTap::new();
        tap.attach_decoder(&src).expect("attach");
        pipeline.set_state(gst::State::Playing).unwrap();

        let payload = SpatialPayload::new(
            48_000,
            vec![
                ObjectState::new(1, [-0.5, 0.5, 0.0]),
                ObjectState::new(2, [0.5, 0.5, 0.5]),
            ],
            Vec::new(),
        );
        let mut buffer = frame_buffer(32, &[0.01, 0.0, 0.1]);
        objectaudio_meta::add_spatial(buffer.get_mut().unwrap(), &payload, 12).unwrap();
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_seconds(1));
        src.downcast_ref::<gst_app::AppSrc>()
            .unwrap()
            .push_buffer(buffer)
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut snapshot = None;
        while Instant::now() < deadline {
            snapshot = tap.snapshot_at(gst::ClockTime::from_seconds(1));
            if snapshot.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let snapshot = snapshot.expect("snapshot");
        assert_eq!(snapshot.objects.len(), 2);
        assert!(snapshot.objects[1].position[2] > 0.4);
        assert!((snapshot.lfe_level - 1.0 / 3.0).abs() < 1e-4);
        assert_eq!(snapshot.objects[0].level, 0.0);
        assert!((snapshot.objects[1].level - 2.0 / 3.0).abs() < 1e-4);
        let _ = pipeline.set_state(gst::State::Null);
    }
}
