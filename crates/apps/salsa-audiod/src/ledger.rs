//! Delay ledger.
//!
//! The ledger keeps signal delay, scheduling allowance and buffer occupancy
//! separate. Signal terms are added once; the parser lookahead is a scheduling
//! requirement, not a presentation delay. Physical-device latency is reported
//! by the output backend and is zero for file/null backends.

use gstreamer as gst;
use serde::Serialize;

use objectaudio_hrtf::FilterbankKind;

use crate::pipeline::{Codec, RenderMode};

#[derive(Debug, Clone, Serialize)]
pub struct DelayLedger {
    pub codec: String,
    pub render: String,
    pub sample_rate: u32,
    /// Signal delay between the carrier AU and decoded PCM samples.
    pub codec_signal_samples: u32,
    /// Signal delay introduced by the renderer (binaural filterbank).
    pub renderer_signal_samples: u32,
    /// Renderer block quantization (processing, not signal delay).
    pub renderer_block_samples: u32,
    /// Parser lookahead required before an AU is emitted (processing).
    pub parser_lookahead_samples: u32,
    /// Fixed allowance for IPC, scheduling and the physical output queue.
    pub scheduling_allowance_ns: u64,
    /// Device-reported physical output latency.
    pub physical_output_latency_ns: u64,
    /// Latency reported by the GStreamer latency query, when available.
    pub queried_latency_ns: Option<u64>,
    pub queried_max_latency_ns: Option<u64>,
}

impl DelayLedger {
    pub fn new(
        codec: Codec,
        render: &RenderMode,
        sample_rate: u32,
        scheduling_allowance_ns: u64,
        physical_output_latency_ns: u64,
    ) -> Self {
        let renderer_signal_samples = match render {
            RenderMode::Speaker { .. } => 0,
            RenderMode::Binaural { filterbank, .. } => {
                let kind = match filterbank {
                    2 => FilterbankKind::Captured,
                    _ => FilterbankKind::Public,
                };
                kind.latency() as u32
            }
        };
        let renderer_block_samples = if render.is_binaural() { 0 } else { 31 };
        Self {
            codec: codec.label().to_string(),
            render: render.label().to_string(),
            sample_rate,
            codec_signal_samples: codec.signal_delay_samples(),
            renderer_signal_samples,
            renderer_block_samples,
            parser_lookahead_samples: codec.parser_lookahead_samples(),
            scheduling_allowance_ns,
            physical_output_latency_ns,
            queried_latency_ns: None,
            queried_max_latency_ns: None,
        }
    }

    pub fn set_query(&mut self, min: gst::ClockTime, max: Option<gst::ClockTime>) {
        if min.nseconds() > 0 {
            self.queried_latency_ns = Some(min.nseconds());
            self.queried_max_latency_ns = max.map(|value| value.nseconds());
        }
    }

    /// Signal delay in decoded PCM samples.
    pub fn total_signal_samples(&self) -> u64 {
        u64::from(self.codec_signal_samples) + u64::from(self.renderer_signal_samples)
    }

    /// Presentation delay from carrier position to audible output.
    pub fn total_presentation_ns(&self) -> u64 {
        let signal_ns = samples_to_ns(self.total_signal_samples(), self.sample_rate);
        signal_ns + self.scheduling_allowance_ns + self.physical_output_latency_ns
    }

    /// The same contract expressed in the HAL device's 192 kHz carrier frames.
    pub fn declared_carrier_frames(&self) -> u64 {
        self.total_presentation_ns()
            .saturating_mul(192_000)
            .div_ceil(1_000_000_000)
    }

    /// Lookahead plus renderer block time that must fit in the scheduling
    /// allowance. A smaller allowance is a configuration error.
    pub fn minimum_allowance_ns(&self) -> u64 {
        let samples =
            u64::from(self.parser_lookahead_samples) + u64::from(self.renderer_block_samples);
        samples_to_ns(samples, self.sample_rate)
    }

    pub fn allowance_is_safe(&self) -> bool {
        self.scheduling_allowance_ns >= self.minimum_allowance_ns()
    }
}

fn samples_to_ns(samples: u64, rate: u32) -> u64 {
    samples.saturating_mul(1_000_000_000) / u64::from(rate.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eac3_speaker_ledger_is_bounded() {
        let ledger = DelayLedger::new(
            Codec::Eac3,
            &RenderMode::speaker("7.1.4"),
            48_000,
            50_000_000,
            0,
        );
        assert_eq!(ledger.total_signal_samples(), 1473);
        assert!(ledger.allowance_is_safe());
        assert_eq!(ledger.declared_carrier_frames(), 15_492);
    }

    #[test]
    fn truehd_binaural_includes_filterbank_latency() {
        let ledger = DelayLedger::new(
            Codec::TrueHd,
            &RenderMode::binaural("/tmp/model"),
            48_000,
            50_000_000,
            0,
        );
        assert_eq!(ledger.total_signal_samples(), 961);
        assert_eq!(ledger.renderer_block_samples, 0);
    }
}
