#![forbid(unsafe_code)]
//! App-specific visual tap: bounded decoder probe plus timeline interpolation.
//!
//! This is intentionally part of the preview demo, not a general-purpose
//! library. The app is the only consumer.

mod tap;
mod timeline;

pub use tap::{TapError, VisualTap};
pub use timeline::{
    DEFAULT_TRAIL_SECONDS, HISTORY_SECONDS, MAX_KEYFRAMES_PER_OBJECT, MAX_TRAIL_POINTS,
    ObjectVisual, STALE_AFTER_NS, VisualSnapshot, VisualState,
};

use gstreamer as gst;

/// Thin wrapper that couples the tap to the app's visualizer switch.
pub struct VisualBridge {
    tap: VisualTap,
    enabled: bool,
}

impl VisualBridge {
    pub fn new(enabled: bool) -> Self {
        let tap = VisualTap::new();
        tap.set_enabled(enabled);
        Self { tap, enabled }
    }

    pub fn attach(&self, decoder: &gst::Element) -> Result<(), TapError> {
        self.tap.attach_decoder(decoder)
    }

    pub fn detach(&self) {
        self.tap.detach();
    }

    pub fn reset(&self) {
        self.tap.reset();
    }

    pub fn begin_segment(&self) {
        self.tap.begin_segment();
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.tap.set_enabled(enabled);
    }

    pub fn snapshot(&self, position: gst::ClockTime) -> Option<VisualSnapshot> {
        if !self.enabled {
            return None;
        }
        self.tap.snapshot_at(position)
    }

    pub fn dropped_frames(&self) -> u64 {
        self.tap.dropped_frames()
    }
}
