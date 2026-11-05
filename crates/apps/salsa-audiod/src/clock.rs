//! Map carrier presentation positions to the physical pipeline's running time.

use anyhow::{Context, Result};
use gstreamer as gst;

#[derive(Debug, Clone, Copy)]
pub struct CarrierClock {
    pub position: gst::ClockTime,
    pub host_ns: u64,
}

#[derive(Default)]
pub(crate) struct PlaybackClock {
    file_anchor: Option<CarrierClock>,
    /// Anchor chosen after the carrier timeline was already late. It replaces
    /// the session anchor until the output is restarted, so a single underrun
    /// does not shift every following buffer again.
    rebase_anchor: Option<CarrierClock>,
    reanchors: u64,
    max_late_ns: u64,
}

impl PlaybackClock {
    pub(crate) fn schedule(
        &mut self,
        pts: gst::ClockTime,
        carrier: Option<CarrierClock>,
        host_now: u64,
        running_now: gst::ClockTime,
        allowance_ns: u64,
    ) -> Result<gst::ClockTime> {
        let anchor = if let Some(rebase) = self.rebase_anchor {
            rebase
        } else if let Some(carrier) = carrier {
            carrier
        } else {
            *self.file_anchor.get_or_insert(CarrierClock {
                position: pts,
                host_ns: host_now,
            })
        };
        // The PCM contains the decoder and renderer signal delays. Add only
        // the scheduling allowance here; the device supplies its own latency.
        let deadline = i128::from(anchor.host_ns) + i128::from(pts.nseconds())
            - i128::from(anchor.position.nseconds())
            + i128::from(allowance_ns);
        let mut remaining = deadline - i128::from(host_now);
        if remaining < 0 {
            // A paused/seeked player, a stalled output route or a slow pipeline
            // start can hand us a buffer whose carrier slot is already in the
            // past. Dropping the late gap and re-anchoring is what a normal
            // audio sink does on underrun; exiting here turned every route
            // change into a launchd restart loop.
            let lateness = -remaining;
            self.reanchors = self.reanchors.saturating_add(1);
            self.max_late_ns = self
                .max_late_ns
                .max(u64::try_from(lateness).unwrap_or(u64::MAX));
            let rebase = CarrierClock {
                position: pts,
                host_ns: host_now,
            };
            if carrier.is_some() {
                self.rebase_anchor = Some(rebase);
            } else {
                self.file_anchor = Some(rebase);
            }
            remaining = i128::from(allowance_ns);
        }
        let running = i128::from(running_now.nseconds()) + remaining;
        Ok(gst::ClockTime::from_nseconds(
            u64::try_from(running).context("output timestamp overflow")?,
        ))
    }

    /// Number of late buffers that were rebased and the worst lateness seen.
    pub(crate) fn stats(&self) -> (u64, u64) {
        (self.reanchors, self.max_late_ns)
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioGetCurrentHostTime() -> u64;
    fn AudioConvertHostTimeToNanos(host_time: u64) -> u64;
}

pub(crate) fn host_ticks_to_ns(ticks: u64) -> u64 {
    #[cfg(target_os = "macos")]
    // SAFETY: CoreAudio converts a value and does not access caller memory.
    unsafe {
        AudioConvertHostTimeToNanos(ticks)
    }
    #[cfg(not(target_os = "macos"))]
    {
        ticks
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn host_now_ticks() -> u64 {
    // SAFETY: CoreAudio reads the monotonic host clock without caller memory.
    unsafe { AudioGetCurrentHostTime() }
}

#[cfg(target_os = "macos")]
pub(crate) fn host_now_ns() -> u64 {
    host_ticks_to_ns(host_now_ticks())
}

#[cfg(target_os = "linux")]
pub(crate) fn host_now_ticks() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` only writes the provided timespec.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    }
    (now.tv_sec as u64) * 1_000_000_000 + now.tv_nsec as u64
}

#[cfg(target_os = "linux")]
pub(crate) fn host_now_ns() -> u64 {
    host_now_ticks()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn host_now_ns() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Codec, DelayLedger, RenderMode};

    #[test]
    fn scheduling_uses_the_declared_allowance_and_carrier_host_time() {
        let anchor = CarrierClock {
            position: gst::ClockTime::from_seconds(100),
            host_ns: 2_000_000_000,
        };
        let mut targets = Vec::new();
        for allowance in [50_000_000, 500_000_000] {
            let ledger = DelayLedger::new(
                Codec::Eac3,
                &RenderMode::speaker("7.1.4"),
                48_000,
                allowance,
                10_000_000,
            );
            let target = PlaybackClock::default()
                .schedule(
                    anchor.position,
                    Some(anchor),
                    2_020_000_000,
                    gst::ClockTime::from_mseconds(300),
                    ledger.scheduling_allowance_ns,
                )
                .unwrap()
                .nseconds();
            let scheduled_delay = target - 300_000_000 + 20_000_000;
            assert_eq!(scheduled_delay, allowance);
            assert_eq!(
                ledger.total_presentation_ns(),
                scheduled_delay + 30_687_500 + 10_000_000
            );
            targets.push(target);
        }
        assert_eq!(targets[1] - targets[0], 450_000_000);
    }

    #[test]
    fn late_audio_reanchors_and_keeps_playing() {
        let anchor = CarrierClock {
            position: gst::ClockTime::ZERO,
            host_ns: 100,
        };
        let mut clock = PlaybackClock::default();
        let target = clock
            .schedule(
                gst::ClockTime::ZERO,
                Some(anchor),
                300,
                gst::ClockTime::from_mseconds(1),
                100,
            )
            .unwrap();
        assert_eq!(target.nseconds(), 1_000_100);
        assert_eq!(clock.stats(), (1, 100));

        // The rebased anchor is reused; the next buffer is not rebased again.
        let next = clock
            .schedule(
                gst::ClockTime::from_mseconds(10),
                Some(anchor),
                10_000_000,
                gst::ClockTime::from_mseconds(2),
                100,
            )
            .unwrap();
        assert_eq!(next.nseconds(), 2_000_400);
        assert_eq!(clock.stats(), (1, 100));
    }
}
