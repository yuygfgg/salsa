// SPDX-License-Identifier: MPL-2.0
//! Silero VAD v6 through tract-onnx.
//!
//! The model is the static OpenVINO export used by `flexaudio-vad` (MIT,
//! Silero Team). It consumes 16 kHz mono 512-sample frames plus a 64-sample
//! context and a two-layer LSTM state.

use std::sync::Arc;

use crate::ensure;
use crate::error::{Context, Result};
use rayon::prelude::*;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use tract_onnx::prelude::*;
use tract_onnx::tract_core::plan::SimplePlan;

const MODEL_BYTES: &[u8] = include_bytes!("../assets/silero_vad_openvino_16k.onnx");
const FRAME_SAMPLES: usize = 512;
const CONTEXT_SAMPLES: usize = 64;
const STATE_SAMPLES: usize = 2 * 128;

type Plan = SimplePlan<TypedFact, Box<dyn TypedOp>>;

struct SileroVad {
    plan: Arc<Plan>,
}

fn uniquify_names<F: Fact, O>(nodes: &mut [Node<F, O>]) {
    let mut seen = std::collections::HashSet::new();
    for (index, node) in nodes.iter_mut().enumerate() {
        if !seen.insert(node.name.clone()) {
            node.name = format!("{}#{index}", node.name);
            seen.insert(node.name.clone());
        }
    }
}

fn accept_duplicate_names(result: TractResult<()>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("duplicate name") {
                Ok(())
            } else {
                Err(error.into())
            }
        }
    }
}

impl SileroVad {
    fn load() -> Result<Self> {
        let mut model = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(MODEL_BYTES))
            .context("parsing the embedded Silero VAD model")?
            .with_input_fact(0, f32::fact([1, CONTEXT_SAMPLES + FRAME_SAMPLES]).into())?
            .with_input_fact(1, f32::fact([2, 1, 128]).into())?;
        // tract 0.23 can create duplicate temporary Const names while expanding
        // this model's Scan. Make names unique before each optimization stage,
        // and accept the (already completed) debug compaction complaint.
        uniquify_names(&mut model.nodes);
        let mut typed = model.into_typed()?;
        uniquify_names(&mut typed.nodes);
        accept_duplicate_names(typed.declutter())?;
        uniquify_names(&mut typed.nodes);
        accept_duplicate_names(typed.optimize())?;
        uniquify_names(&mut typed.nodes);
        Ok(Self {
            plan: typed.into_runnable()?,
        })
    }

    fn infer(
        &self,
        input: &[f32; CONTEXT_SAMPLES + FRAME_SAMPLES],
        state: &[f32],
    ) -> Result<(f32, Vec<f32>)> {
        let input = tract_ndarray::Array2::from_shape_vec(
            (1, CONTEXT_SAMPLES + FRAME_SAMPLES),
            input.to_vec(),
        )?;
        let state = tract_ndarray::Array3::from_shape_vec((2, 1, 128), state.to_vec())?;
        let outputs = self.plan.run(tvec!(
            input.into_tensor().into(),
            state.into_tensor().into()
        ))?;
        let probability = outputs[0].to_plain_array_view::<f32>()?[[0, 0]];
        let next_state = outputs[1]
            .to_plain_array_view::<f32>()?
            .iter()
            .copied()
            .collect();
        Ok((probability, next_state))
    }
}

const VAD_RATE: u32 = 16_000;

struct SourceVad {
    state: Vec<f32>,
    context: Vec<f32>,
    pending: Vec<f32>,
    smoothed: f32,
    frames: usize,
    smooth_alpha: f64,
}

impl SourceVad {
    fn new(smooth_ms: f64) -> Self {
        let alpha = 1.0
            - (-(FRAME_SAMPLES as f64) / (f64::from(VAD_RATE) * smooth_ms.max(1e-6) / 1000.0))
                .exp();
        Self {
            state: vec![0.0; STATE_SAMPLES],
            context: vec![0.0; CONTEXT_SAMPLES],
            pending: Vec::with_capacity(FRAME_SAMPLES),
            smoothed: 0.0,
            frames: 0,
            smooth_alpha: alpha,
        }
    }

    fn update_probability(&mut self, probability: f32) {
        if self.frames == 0 {
            self.smoothed = probability;
        } else {
            self.smoothed = ((1.0 - self.smooth_alpha) * f64::from(self.smoothed)
                + self.smooth_alpha * f64::from(probability)) as f32;
        }
        self.frames += 1;
    }

    fn push_iter(&mut self, model: &SileroVad, samples: impl Iterator<Item = f32>) -> Result<f32> {
        self.pending.extend(samples);

        let mut offset = 0;
        while self.pending.len() - offset >= FRAME_SAMPLES {
            let mut input = [0.0f32; CONTEXT_SAMPLES + FRAME_SAMPLES];
            input[..CONTEXT_SAMPLES].copy_from_slice(&self.context);
            input[CONTEXT_SAMPLES..].copy_from_slice(&self.pending[offset..offset + FRAME_SAMPLES]);
            let (probability, state) = model.infer(&input, &self.state)?;
            self.context
                .copy_from_slice(&input[CONTEXT_SAMPLES + FRAME_SAMPLES - CONTEXT_SAMPLES..]);
            self.state = state;
            self.update_probability(probability);
            offset += FRAME_SAMPLES;
        }
        if offset > 0 {
            self.pending.drain(..offset);
        }
        Ok(self.smoothed)
    }
}

/// Per-source streaming Silero VAD.
pub struct VadBank {
    model: SileroVad,
    sources: Vec<SourceVad>,
    resampler: Option<Fft<f64>>,
    n_sources: usize,
    hop: usize,
}

impl VadBank {
    pub fn new(n_sources: usize, smooth_ms: f64, hop: usize, sample_rate: u32) -> Result<Self> {
        ensure!(n_sources > 0, "VAD needs at least one source");
        let resampler = if sample_rate == VAD_RATE {
            None
        } else {
            Some(Fft::<f64>::new(
                sample_rate as usize,
                VAD_RATE as usize,
                hop,
                1,
                n_sources,
                FixedSync::Input,
            )?)
        };
        Ok(Self {
            model: SileroVad::load()?,
            sources: (0..n_sources).map(|_| SourceVad::new(smooth_ms)).collect(),
            resampler,
            n_sources,
            hop,
        })
    }

    /// Advance one clustering frame and return the smoothed speech probability
    /// per source. `block` is frame-major `[sample][source]`.
    pub fn advance(&mut self, block: &[f64], length: usize, n_sources: usize) -> Result<Vec<f64>> {
        ensure!(
            n_sources == self.n_sources,
            "VAD source count changed from {} to {n_sources}",
            self.n_sources
        );
        ensure!(length <= block.len() / n_sources, "VAD block is too short");
        let mut probabilities = vec![0.0f64; n_sources];

        if let Some(resampler) = self.resampler.as_mut() {
            let output_frames = resampler.output_frames_next().max(1);
            let mut output = vec![0.0f64; n_sources * output_frames];
            let input = InterleavedSlice::new(&block[..length * n_sources], n_sources, length)?;
            let mut out_adapter = InterleavedSlice::new_mut(&mut output, n_sources, output_frames)?;
            let indexing = Indexing {
                input_offset: 0,
                output_offset: 0,
                partial_len: (length < self.hop).then_some(length),
                active_channels_mask: None,
            };
            let (_, produced) =
                resampler.process_into_buffer(&input, &mut out_adapter, Some(&indexing))?;
            if produced > 0 {
                self.sources
                    .par_iter_mut()
                    .zip(probabilities.par_iter_mut())
                    .enumerate()
                    .try_for_each(|(source, (vad, probability))| -> Result<()> {
                        *probability = f64::from(vad.push_iter(
                            &self.model,
                            (0..produced).map(|frame| output[frame * n_sources + source] as f32),
                        )?);
                        Ok(())
                    })?;
            }
        } else {
            self.sources
                .par_iter_mut()
                .zip(probabilities.par_iter_mut())
                .enumerate()
                .try_for_each(|(source, (vad, probability))| -> Result<()> {
                    *probability = f64::from(vad.push_iter(
                        &self.model,
                        (0..length).map(|frame| block[frame * n_sources + source] as f32),
                    )?);
                    Ok(())
                })?;
        }
        Ok(probabilities)
    }
}

/// Python `content_importance`: logistic mapping of the VAD probability.
pub fn content_importance(
    probabilities: &[f64],
    threshold: f64,
    slope: f64,
    bias: f64,
) -> Vec<f64> {
    let floor = 1.0 / (1.0 + bias.exp());
    let ceiling = 1.0 / (1.0 + (-slope * (1.0 - threshold).max(0.0) + bias).exp());
    probabilities
        .iter()
        .map(|probability| {
            let raw = 1.0 / (1.0 + (-slope * (*probability - threshold).max(0.0) + bias).exp());
            ((raw - floor) / (ceiling - floor).max(1e-12)).clamp(0.0, 1.0)
        })
        .collect()
}

/// Python `content_gain`: linear excitation gain from normalized importance.
pub fn content_gain(importance: &[f64], gain_db: f64) -> Vec<f64> {
    importance
        .iter()
        .map(|value| 10.0_f64.powf(gain_db * value / 20.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_uses_model_time_and_supports_zero_delay() {
        for smooth_ms in [0.0, 100.0, 500.0] {
            let mut vad = SourceVad::new(smooth_ms);
            vad.update_probability(0.0);
            for frame in 1..=10 {
                vad.update_probability(1.0);
                let expected = if smooth_ms == 0.0 {
                    1.0
                } else {
                    1.0 - (-(frame as f64) * 32.0 / smooth_ms).exp()
                };
                assert!((f64::from(vad.smoothed) - expected).abs() < 1e-7);
            }
        }
    }

    #[test]
    fn smoothing_is_independent_of_input_chunk_size() {
        let model = SileroVad::load().unwrap();
        let samples: Vec<f32> = (0..FRAME_SAMPLES * 6)
            .map(|sample| (sample as f32 * 0.13).sin() * 0.2)
            .collect();
        let mut whole = SourceVad::new(100.0);
        whole.push_iter(&model, samples.iter().copied()).unwrap();
        for chunk_size in [128, 512, 768, 1536] {
            let mut chunked = SourceVad::new(100.0);
            for chunk in samples.chunks(chunk_size) {
                chunked.push_iter(&model, chunk.iter().copied()).unwrap();
            }
            assert_eq!(chunked.frames, whole.frames);
            assert_eq!(chunked.smoothed, whole.smoothed);
        }
    }

    #[test]
    fn embedded_model_runs_at_common_master_rates() {
        for (rate, hop) in [
            (16_000, 1536),
            (44_100, 1536),
            (48_000, 1536),
            (96_000, 1536),
            (48_000, 512),
        ] {
            let mut bank = VadBank::new(2, 100.0, hop, rate).unwrap();
            assert!((bank.sources[0].smooth_alpha - (1.0 - (-0.32f64).exp())).abs() < 1e-12);
            let block = vec![0.0f64; hop * 2];
            let probabilities = bank.advance(&block, hop, 2).unwrap();
            assert_eq!(probabilities.len(), 2);
            assert!(
                probabilities
                    .iter()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value)),
                "rate {rate}, hop {hop}"
            );
        }
    }

    #[test]
    fn importance_and_gain_are_bounded() {
        let importance = content_importance(&[0.0, 0.5, 1.0], 0.1, 8.0, 2.0);
        assert!(importance.iter().all(|value| (0.0..=1.0).contains(value)));
        let gain = content_gain(&importance, 6.0);
        assert!(gain.iter().all(|value| (*value - 1.0).abs() < 2.0));
    }
}
