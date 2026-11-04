//! JOC reconstruction kernel: analysis QMF, object matrix, synthesis QMF, LFE.
//!
//! The transforms use a scalar `rustfft` path.

#![allow(
    clippy::needless_range_loop,
    clippy::manual_range_contains,
    clippy::manual_memcpy,
    clippy::too_many_arguments
)]

use std::f64::consts::PI;
use std::sync::Arc;

use eac3joc::{FRAME_SAMPLES, JocParams, MAX_CORE_CHANNELS, MAX_OBJECTS};
use rustfft::num_complex::Complex64;
use rustfft::{Fft, FftPlanner};

use crate::qmf_tables as tables;

/// Output channels: the LFE plus every JOC object.
pub const OUTPUT_CHANNELS: usize = MAX_OBJECTS + 1;
const LFE_DELAY: usize = 1217;

const DC_A: [f64; 21] = [
    -0.0006242550443857908,
    -0.0019234686624258757,
    -0.0042654648423194885,
    -0.008168308064341545,
    -0.014327201060950756,
    -0.023759860545396805,
    -0.03757232800126076,
    -0.05577569454908371,
    -0.07568276673555374,
    -0.09172472357749939,
    -0.5979374051094055,
    -0.09172472357749939,
    -0.07568276673555374,
    -0.05577569454908371,
    -0.03757232800126076,
    -0.023759860545396805,
    -0.014327201060950756,
    -0.008168308064341545,
    -0.0042654648423194885,
    -0.0019234686624258757,
    -0.0006242550443857908,
];

const DC_B: [f64; 21] = [
    0.0013996040215715766,
    0.003839150769636035,
    0.007512642536312342,
    0.012419373728334904,
    0.018367428332567215,
    0.0249701626598835,
    0.03167900815606117,
    0.03785000368952751,
    0.04283412545919418,
    0.04607561603188515,
    0.047200120985507965,
    0.04607561603188515,
    0.04283412545919418,
    0.03785000368952751,
    0.03167900815606117,
    0.0249701626598835,
    0.018367428332567215,
    0.012419373728334904,
    0.007512642536312342,
    0.003839150769636035,
    0.0013996040215715766,
];

fn clamp_unit(value: f64) -> f64 {
    value.clamp(-1.0, 1.0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelError {
    InvalidCoreChannels(usize),
    InvalidBedLength { expected: usize, actual: usize },
    InvalidLfeLength { expected: usize, actual: usize },
    InvalidOutputLength { expected: usize, actual: usize },
    InvalidParameters(String),
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCoreChannels(count) => {
                write!(f, "core channel count must be 5 or 7, got {count}")
            }
            Self::InvalidBedLength { expected, actual } => {
                write!(f, "bed PCM has {actual} samples, expected {expected}")
            }
            Self::InvalidLfeLength { expected, actual } => {
                write!(f, "LFE PCM has {actual} samples, expected {expected}")
            }
            Self::InvalidOutputLength { expected, actual } => {
                write!(f, "output has {actual} samples, expected {expected}")
            }
            Self::InvalidParameters(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for KernelError {}

/// The stateful JOC kernel.
pub struct JocKernel {
    fft: Arc<dyn Fft<f64>>,
    rotation_sin: [f64; 64],
    rotation_cos: [f64; 64],

    analysis_fifo: [[[f64; 64]; 9]; MAX_CORE_CHANNELS],
    analysis_delay: [[[f32; 64]; 10]; 5],
    analysis_phase: f32,
    surround_delay: [[[Complex64; 64]; 10]; 2],
    surround_history: [[Complex64; 20]; 2],
    phase_shift_enabled: bool,
    lfe_delay: [f64; LFE_DELAY],
    matrix_previous: [[[f64; 64]; MAX_CORE_CHANNELS]; MAX_OBJECTS],
    synthesis_state: [[f64; 640]; MAX_OBJECTS],

    x: [[[Complex64; 24]; 64]; MAX_CORE_CHANNELS],
    z: [[[Complex64; 24]; 64]; MAX_OBJECTS],
}

impl Default for JocKernel {
    fn default() -> Self {
        Self::new()
    }
}

impl JocKernel {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(64);
        let mut rotation_sin = [0.0; 64];
        let mut rotation_cos = [0.0; 64];
        for index in 0..64 {
            let theta = PI * index as f64 / 128.0;
            rotation_sin[index] = 0.5 * theta.sin();
            rotation_cos[index] = 0.5 * theta.cos();
        }
        Self {
            fft,
            rotation_sin,
            rotation_cos,
            analysis_fifo: [[[0.0; 64]; 9]; MAX_CORE_CHANNELS],
            analysis_delay: [[[0.0; 64]; 10]; 5],
            analysis_phase: -0.5,
            surround_delay: [[[Complex64::new(0.0, 0.0); 64]; 10]; 2],
            surround_history: [[Complex64::new(0.0, 0.0); 20]; 2],
            phase_shift_enabled: true,
            lfe_delay: [0.0; LFE_DELAY],
            matrix_previous: [[[0.0; 64]; MAX_CORE_CHANNELS]; MAX_OBJECTS],
            synthesis_state: [[0.0; 640]; MAX_OBJECTS],
            x: [[[Complex64::new(0.0, 0.0); 24]; 64]; MAX_CORE_CHANNELS],
            z: [[[Complex64::new(0.0, 0.0); 24]; 64]; MAX_OBJECTS],
        }
    }

    /// Reset all streaming state. The first frame after a reset adopts the new
    /// scalar without an intra-frame transition.
    pub fn reset(&mut self) {
        self.analysis_fifo = [[[0.0; 64]; 9]; MAX_CORE_CHANNELS];
        self.analysis_delay = [[[0.0; 64]; 10]; 5];
        self.analysis_phase = -0.5;
        self.surround_delay = [[[Complex64::new(0.0, 0.0); 64]; 10]; 2];
        self.surround_history = [[Complex64::new(0.0, 0.0); 20]; 2];
        self.lfe_delay = [0.0; LFE_DELAY];
        self.matrix_previous = [[[0.0; 64]; MAX_CORE_CHANNELS]; MAX_OBJECTS];
        self.synthesis_state = [[0.0; 640]; MAX_OBJECTS];
        self.x = [[[Complex64::new(0.0, 0.0); 24]; 64]; MAX_CORE_CHANNELS];
        self.z = [[[Complex64::new(0.0, 0.0); 24]; 64]; MAX_OBJECTS];
    }

    /// Render one audio frame.
    ///
    /// `bed` is planar `core_channels * 1536`, `lfe` is 1536 samples and
    /// `output` is planar `16 * 1536` with LFE in channel 0 and JOC objects in
    /// channels 1..15.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        bed: &[f32],
        lfe: Option<&[f32]>,
        params: &JocParams,
        output_scale: f32,
        output: &mut [f32],
    ) -> Result<(), KernelError> {
        let core_channels = usize::from(params.n_channels);
        if core_channels != 5 && core_channels != MAX_CORE_CHANNELS {
            return Err(KernelError::InvalidCoreChannels(core_channels));
        }
        let expected_bed = core_channels * FRAME_SAMPLES;
        if bed.len() != expected_bed {
            return Err(KernelError::InvalidBedLength {
                expected: expected_bed,
                actual: bed.len(),
            });
        }
        if let Some(lfe) = lfe
            && lfe.len() != FRAME_SAMPLES
        {
            return Err(KernelError::InvalidLfeLength {
                expected: FRAME_SAMPLES,
                actual: lfe.len(),
            });
        }
        if output.len() != OUTPUT_CHANNELS * FRAME_SAMPLES {
            return Err(KernelError::InvalidOutputLength {
                expected: OUTPUT_CHANNELS * FRAME_SAMPLES,
                actual: output.len(),
            });
        }
        if !params.clipgain.is_finite() || !output_scale.is_finite() {
            return Err(KernelError::InvalidParameters(
                "clipgain and output scale must be finite".into(),
            ));
        }
        for object in 0..MAX_OBJECTS {
            if params.present_mask & (1 << object) == 0 {
                continue;
            }
            let info = &params.objects[object];
            if eac3joc::parameter_band_map(info.n_bands).is_none() {
                return Err(KernelError::InvalidParameters(format!(
                    "unsupported parameter-band count {}",
                    info.n_bands
                )));
            }
            if !(1..=2).contains(&info.n_dpoints) {
                return Err(KernelError::InvalidParameters(
                    "n_dpoints must be 1 or 2".into(),
                ));
            }
            if info.slope_idx > 1 {
                return Err(KernelError::InvalidParameters(
                    "slope_idx must be 0 or 1".into(),
                ));
            }
        }

        output.fill(0.0);
        self.phase_shift_enabled = params.phase_shift();
        let core_channels_f32 = core_channels as f32;
        let _ = core_channels_f32;
        self.analysis(bed, core_channels, params.clipgain);
        self.render_lfe(lfe, output_scale, output);
        self.process_objects(params, output_scale, output);
        Ok(())
    }

    fn analysis(&mut self, bed: &[f32], core_channels: usize, clipgain: f64) {
        // 0.0625 = 1/16; the x16 at the output cancels it, leaving joc_clipgain.
        let scalar = (0.0625f64 * clipgain) as f32;
        let mut phase_old = self.analysis_phase;
        if phase_old < 0.0 {
            phase_old = scalar;
        }
        for channel in 0..core_channels {
            self.analysis_channel(bed, channel, core_channels, phase_old, scalar);
        }
        self.analysis_phase = scalar;
        self.surround_post();
    }

    fn analysis_channel(
        &mut self,
        bed: &[f32],
        channel: usize,
        _core_channels: usize,
        phase_old: f32,
        phase_new: f32,
    ) {
        let ramp = phase_old != phase_new;
        let step = (phase_new - phase_old) / 256.0f32;
        let mut scaled = [0.0f32; 64];
        let mut delayed = [0.0f32; 64];
        for timeslot in 0..24 {
            for sample in 0..64 {
                let frame_sample = timeslot * 64 + sample;
                let mut gain = phase_new;
                if ramp && frame_sample < 256 {
                    gain = phase_old + frame_sample as f32 * step;
                }
                scaled[sample] = bed[channel * FRAME_SAMPLES + frame_sample] * gain;
            }
            let delay_lane = if channel < 3 { channel } else { channel - 2 };
            let ring = if channel < 3 || channel >= 5 {
                delayed.copy_from_slice(&self.analysis_delay[delay_lane][0]);
                let lanes = &mut self.analysis_delay[delay_lane];
                lanes.copy_within(1..10, 0);
                lanes[9] = scaled;
                &delayed
            } else {
                &scaled
            };
            let ring_copy = *ring;
            self.analysis_slot(channel, &ring_copy, timeslot);
        }
    }

    fn analysis_slot(&mut self, channel: usize, ring: &[f32; 64], timeslot: usize) {
        let mut v36 = [0.0f64; 64];
        let mut v40 = [0.0f64; 64];
        let fifo = &self.analysis_fifo[channel];
        for sample in 0..64 {
            v36[sample] = fifo[0][sample] * tables::KANALYSIS_WINDOW[8 * 64 + sample]
                + fifo[2][sample] * tables::KANALYSIS_WINDOW[6 * 64 + sample]
                + fifo[4][sample] * tables::KANALYSIS_WINDOW[4 * 64 + sample]
                + fifo[6][sample] * tables::KANALYSIS_WINDOW[2 * 64 + sample]
                + fifo[8][sample] * tables::KANALYSIS_WINDOW[sample];
            v40[sample] = fifo[1][sample] * tables::KANALYSIS_WINDOW[7 * 64 + sample]
                + fifo[3][sample] * tables::KANALYSIS_WINDOW[5 * 64 + sample]
                + fifo[5][sample] * tables::KANALYSIS_WINDOW[3 * 64 + sample]
                + fifo[7][sample] * tables::KANALYSIS_WINDOW[64 + sample]
                + f64::from(ring[sample]) * tables::KANALYSIS_WINDOW[9 * 64 + sample];
        }

        let mut frequency = [Complex64::new(0.0, 0.0); 64];
        for k in 0..64 {
            let source = 63 - k;
            let re = v40[source];
            let im = v36[source];
            let a = self.rotation_sin[k];
            let b = self.rotation_cos[k];
            frequency[k] = Complex64::new(im * a - re * b, im * b + re * a);
        }
        self.fft.process(&mut frequency);
        const SCALE: f64 = 1.0 / 64.0;
        for k in 0..32 {
            self.x[channel][2 * k][timeslot] =
                Complex64::new(frequency[k].re * SCALE, -frequency[k].im * SCALE);
            self.x[channel][2 * k + 1][timeslot] =
                Complex64::new(frequency[63 - k].re * SCALE, frequency[63 - k].im * SCALE);
        }

        let fifo = &mut self.analysis_fifo[channel];
        for history in (1..9).rev() {
            fifo[history] = fifo[history - 1];
        }
        for sample in 0..64 {
            fifo[0][sample] = f64::from(ring[sample]);
        }
    }

    fn surround_post(&mut self) {
        for surround in 0..2 {
            let channel = surround + 3;
            for group in (0..24).step_by(4) {
                let mut current = [[Complex64::new(0.0, 0.0); 64]; 4];
                let mut dc_buffer = [Complex64::new(0.0, 0.0); 24];
                for slot in 0..4 {
                    for band in 0..64 {
                        current[slot][band] = self.x[channel][band][group + slot];
                        let delayed = self.surround_delay[surround][slot][band];
                        self.x[channel][band][group + slot] = if self.phase_shift_enabled {
                            Complex64::new(delayed.im, -delayed.re)
                        } else {
                            delayed
                        };
                    }
                }
                dc_buffer[..20].copy_from_slice(&self.surround_history[surround]);
                for slot in 0..4 {
                    dc_buffer[20 + slot] = current[slot][0];
                }
                if self.phase_shift_enabled {
                    for slot in 0..4 {
                        let mut sum = Complex64::new(0.0, 0.0);
                        for tap in 0..21 {
                            let sample = dc_buffer[slot + tap];
                            let cr = DC_B[tap];
                            let ci = DC_A[tap];
                            sum.re += sample.re * cr - sample.im * ci;
                            sum.im += sample.re * ci + sample.im * cr;
                        }
                        self.x[channel][0][group + slot] =
                            Complex64::new(2.0 * sum.re, 2.0 * sum.im);
                    }
                }
                self.surround_history[surround].copy_from_slice(&dc_buffer[4..24]);
                self.surround_delay[surround].copy_within(4..10, 0);
                for slot in 0..4 {
                    self.surround_delay[surround][6 + slot] = current[slot];
                }
            }
        }
    }

    fn matrix_object(&mut self, object: usize, params: &JocParams) {
        self.z[object] = [[Complex64::new(0.0, 0.0); 24]; 64];
        let info = &params.objects[object];
        let bands = info.n_bands;
        let points = info.n_dpoints;
        let slope = info.slope_idx;
        let map = eac3joc::parameter_band_map(bands).expect("validated");
        let core_channels = usize::from(params.n_channels);
        for channel in 0..core_channels {
            for subband in 0..64 {
                let parameter_band = usize::from(map[subband]);
                let previous = self.matrix_previous[object][channel][subband];
                let target0 = info.dq[0][channel][parameter_band];
                let target1 = if points == 2 {
                    info.dq[1][channel][parameter_band]
                } else {
                    target0
                };
                let mut last = previous;
                for timeslot in 0..24 {
                    let coefficient = if slope == 0 {
                        if points == 1 {
                            let alpha = (timeslot + 1) as f64 / 24.0;
                            previous * (1.0 - alpha) + target0 * alpha
                        } else if timeslot < 12 {
                            let alpha = (timeslot + 1) as f64 / 12.0;
                            previous * (1.0 - alpha) + target0 * alpha
                        } else {
                            let alpha = (timeslot - 11) as f64 / 12.0;
                            target0 * (1.0 - alpha) + target1 * alpha
                        }
                    } else if points == 1 {
                        if timeslot < usize::from(info.offset_ts[0]) {
                            previous
                        } else {
                            target0
                        }
                    } else {
                        let mut value = if timeslot < usize::from(info.offset_ts[0]) {
                            previous
                        } else {
                            target0
                        };
                        if timeslot >= usize::from(info.offset_ts[1]) {
                            value = target1;
                        }
                        value
                    };
                    let x = self.x[channel][subband][timeslot];
                    self.z[object][subband][timeslot].re += x.re * coefficient;
                    self.z[object][subband][timeslot].im += x.im * coefficient;
                    last = coefficient;
                }
                self.matrix_previous[object][channel][subband] = last;
            }
        }
    }

    fn synthesis_object(&mut self, object: usize, output_scale: f32, output: &mut [f32]) {
        let mut frequency = [Complex64::new(0.0, 0.0); 64];
        let mut rotated = [0.0f64; 128];
        let mut pcm64 = [0.0f64; 64];
        let destination = (object + 1) * FRAME_SAMPLES;
        for timeslot in 0..24 {
            for k in 0..32 {
                let even = self.z[object][2 * k][timeslot];
                let odd = self.z[object][2 * k + 1][timeslot];
                frequency[k] = Complex64::new(even.re, -even.im);
                frequency[63 - k] = odd;
            }
            self.fft.process(&mut frequency);
            for k in 0..64 {
                let re = frequency[k].re;
                let im = frequency[k].im;
                let sin = self.rotation_sin[k];
                let cos = self.rotation_cos[k];
                rotated[2 * k] = 2.0 * (re * cos + im * sin);
                rotated[2 * k + 1] = 2.0 * (im * cos - re * sin);
            }
            qmf5_step(&mut self.synthesis_state[object], &rotated, &mut pcm64);
            for sample in 0..64 {
                let clipped = clamp_unit(16.0 * pcm64[sample]);
                output[destination + timeslot * 64 + sample] = clipped as f32 * output_scale;
            }
        }
    }

    fn process_objects(&mut self, params: &JocParams, output_scale: f32, output: &mut [f32]) {
        for object in 0..MAX_OBJECTS {
            if params.present_mask & (1 << object) == 0 {
                continue;
            }
            self.matrix_object(object, params);
            self.synthesis_object(object, output_scale, output);
        }
    }

    fn render_lfe(&mut self, lfe: Option<&[f32]>, output_scale: f32, output: &mut [f32]) {
        let Some(lfe) = lfe else {
            return;
        };
        for sample in 0..LFE_DELAY {
            output[sample] = clamp_unit(self.lfe_delay[sample]) as f32 * output_scale;
        }
        for sample in LFE_DELAY..FRAME_SAMPLES {
            output[sample] = clamp_unit(f64::from(lfe[sample - LFE_DELAY])) as f32 * output_scale;
        }
        for sample in 0..LFE_DELAY {
            self.lfe_delay[sample] = f64::from(lfe[sample + (FRAME_SAMPLES - LFE_DELAY)]);
        }
    }
}

fn qmf5_step(state: &mut [f64; 640], rotated: &[f64; 128], output: &mut [f64; 64]) {
    for block in 0..16 {
        for lane in 0..4 {
            let sample = block * 4 + lane;
            let state_base = block * 36 + lane;
            let even = rotated[block * 8 + lane * 2];
            let odd = rotated[block * 8 + lane * 2 + 1];
            output[sample] = 2.0 * (tables::KQMF5_WINDOW[sample] * even + state[state_base]);

            state[state_base] = tables::KQMF5_WINDOW[64 + sample] * odd + state[state_base + 4];
            for slot in 0..7 {
                let alternating = if slot & 1 == 0 { even } else { odd };
                state[state_base + (slot + 1) * 4] = tables::KQMF5_WINDOW[(slot + 2) * 64 + sample]
                    * alternating
                    + state[state_base + (slot + 2) * 4];
            }
            state[state_base + 8 * 4] = tables::KQMF5_WINDOW[9 * 64 + sample] * odd;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_produces_silence() {
        let params = JocParams::silent();
        let mut kernel = JocKernel::new();
        let bed = vec![0.0f32; 5 * FRAME_SAMPLES];
        let lfe = vec![0.0f32; FRAME_SAMPLES];
        let mut output = vec![0.0f32; OUTPUT_CHANNELS * FRAME_SAMPLES];
        kernel
            .process(&bed, Some(&lfe), &params, 1.0, &mut output)
            .unwrap();
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert_eq!(output[0], 0.0);
    }

    #[test]
    fn rejects_a_bad_core_channel_count() {
        let mut params = JocParams::silent();
        params.n_channels = 6;
        let mut kernel = JocKernel::new();
        let bed = vec![0.0f32; 6 * FRAME_SAMPLES];
        let mut output = vec![0.0f32; OUTPUT_CHANNELS * FRAME_SAMPLES];
        assert_eq!(
            kernel.process(&bed, None, &params, 1.0, &mut output),
            Err(KernelError::InvalidCoreChannels(6))
        );
    }

    #[test]
    fn lfe_is_delayed_by_1217_samples() {
        let params = JocParams::silent();
        let mut kernel = JocKernel::new();
        let bed = vec![0.0f32; 5 * FRAME_SAMPLES];
        let mut lfe = vec![0.0f32; FRAME_SAMPLES];
        lfe[0] = 0.5;
        let mut output = vec![0.0f32; OUTPUT_CHANNELS * FRAME_SAMPLES];
        kernel
            .process(&bed, Some(&lfe), &params, 1.0, &mut output)
            .unwrap();
        assert!(output[..LFE_DELAY].iter().all(|value| *value == 0.0));
        assert!((output[LFE_DELAY] - 0.5).abs() < 1e-6);
    }
}
