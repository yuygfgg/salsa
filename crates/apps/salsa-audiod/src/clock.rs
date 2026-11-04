//! Map carrier presentation positions to the physical pipeline's running time.

use anyhow::{Context, Result, bail};
use gstreamer as gst;

#[derive(Debug, Clone, Copy)]
pub struct CarrierClock {
    pub position: gst::ClockTime,
    pub host_ns: u64,
}

#[derive(Default)]
pub(crate) struct PlaybackClock {
    file_anchor: Option<CarrierClock>,
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
        let anchor = carrier.unwrap_or_else(|| {
            *self.file_anchor.get_or_insert(CarrierClock {
                position: pts,
                host_ns: host_now,
            })
        });
        // The PCM contains the decoder and renderer signal delays. Add only
        // the scheduling allowance here; the device supplies its own latency.
        let deadline = i128::from(anchor.host_ns) + i128::from(pts.nseconds())
            - i128::from(anchor.position.nseconds())
            + i128::from(allowance_ns);
        let remaining = deadline - i128::from(host_now);
        if remaining < 0 {
            bail!(
                "physical output missed its playback deadline by {:.3} ms",
                -remaining as f64 / 1e6
            );
        }
        let running = i128::from(running_now.nseconds()) + remaining;
        Ok(gst::ClockTime::from_nseconds(
            u64::try_from(running).context("output timestamp overflow")?,
        ))
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
pub(crate) fn host_now_ns() -> u64 {
    // SAFETY: CoreAudio reads the monotonic host clock without caller memory.
    host_ticks_to_ns(unsafe { AudioGetCurrentHostTime() })
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
    fn late_audio_fails_without_reanchoring_the_contract() {
        let anchor = CarrierClock {
            position: gst::ClockTime::ZERO,
            host_ns: 100,
        };
        assert!(
            PlaybackClock::default()
                .schedule(
                    gst::ClockTime::ZERO,
                    Some(anchor),
                    300,
                    gst::ClockTime::ZERO,
                    100
                )
                .is_err()
        );
    }
}
