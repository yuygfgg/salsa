//! Deterministic fallback and observation generation.

use salsa_spatial::{BaseFold, TrajectoryKnot};
use salsa_spatial_dsp::{CorePanner, interpolated_core_columns};

use crate::source::TileSource;

/// Complete fallback mix, auxiliary observations and separate LFE, ready for
/// the Opus multistream encoder.
#[derive(Debug, Clone, PartialEq)]
pub struct TransportSignals {
    /// Five full-range fallback rows: `FL, FR, FC, SL, SR`.
    pub base: [Vec<f32>; 5],
    /// Auxiliary observations in group order; initially `H_L, H_R`.
    pub aux: Vec<Vec<f32>>,
    /// Separate LFE signal.
    pub lfe: Vec<f32>,
    pub frames: usize,
}

impl TransportSignals {
    /// Peak of the complete transport, including the fixed height fold.
    pub fn peak(&self) -> f64 {
        self.base
            .iter()
            .chain(&self.aux)
            .flatten()
            .chain(&self.lfe)
            .map(|sample| f64::from(*sample).abs())
            .fold(0.0, f64::max)
    }

    /// Seven full-range observations in reconstruction order:
    /// `[b_FL, b_FR, b_FC, b_SL, b_SR, H_L, H_R]`.
    pub fn observation_order(&self) -> [Vec<f32>; 7] {
        std::array::from_fn(|channel| {
            if channel < 5 {
                self.base[channel].clone()
            } else {
                self.aux
                    .get(channel - 5)
                    .cloned()
                    .unwrap_or_else(|| vec![0.0; self.frames])
            }
        })
    }
}

/// Build the complete fallback mix and auxiliary observations.
///
/// The base rows are `g + A h`; the auxiliary rows are `h`. The single
/// programme headroom gain is applied to every transported signal exactly once.
pub fn build_transport(
    source: &TileSource,
    core_knots: &[Vec<TrajectoryKnot>],
    headroom_gain: f64,
    panner: CorePanner,
    fold: BaseFold,
    aux_count: usize,
) -> TransportSignals {
    let frames = source.frames;
    let object_count = source.object_count;
    let mut base: [Vec<f32>; 5] = std::array::from_fn(|_| vec![0.0f32; frames]);
    let mut aux: Vec<Vec<f32>> = (0..aux_count).map(|_| vec![0.0f32; frames]).collect();
    for object in 0..object_count {
        let columns =
            panner.observation_columns(&core_knots[object], source.start_sample, frames, fold);
        let pcm = &source.object_pcm[object];
        for sample in 0..frames {
            let value = pcm[sample];
            if value == 0.0 {
                continue;
            }
            let column = columns[sample];
            let height = [column[5], column[6]];
            for channel in 0..5 {
                base[channel][sample] += (column[channel] * f64::from(value)) as f32;
            }
            for channel in 0..aux_count {
                let gain = height.get(channel).copied().unwrap_or(0.0);
                aux[channel][sample] += (gain * f64::from(value)) as f32;
            }
        }
    }
    let gain = headroom_gain as f32;
    for channel in base.iter_mut() {
        for value in channel.iter_mut() {
            *value *= gain;
        }
    }
    for channel in &mut aux {
        for value in channel.iter_mut() {
            *value *= gain;
        }
    }
    let lfe = source.lfe_pcm.iter().map(|value| *value * gain).collect();
    TransportSignals {
        base,
        aux,
        lfe,
        frames,
    }
}

/// Build the seven full-range core channels and the separate LFE channel.
///
/// Every object contributes through the shared interpolated panner columns.
/// The common headroom gain is applied to all core channels once; it is never
/// applied to source analysis or object PCM.
#[allow(dead_code)]
pub fn build_core(
    source: &TileSource,
    core_knots: &[Vec<TrajectoryKnot>],
    headroom_gain: f64,
) -> ([Vec<f32>; 7], Vec<f32>) {
    let frames = source.frames;
    let object_count = source.object_count;
    let mut core: [Vec<f32>; 7] = std::array::from_fn(|_| vec![0.0f32; frames]);
    for object in 0..object_count {
        let columns = interpolated_core_columns(&core_knots[object], source.start_sample, frames);
        let pcm = &source.object_pcm[object];
        for sample in 0..frames {
            let value = pcm[sample];
            if value == 0.0 {
                continue;
            }
            let column = columns[sample];
            for channel in 0..7 {
                core[channel][sample] += (column[channel] * f64::from(value)) as f32;
            }
        }
    }
    let gain = headroom_gain as f32;
    for channel in core.iter_mut() {
        for value in channel.iter_mut() {
            *value *= gain;
        }
    }
    let lfe = source.lfe_pcm.iter().map(|value| *value * gain).collect();
    (core, lfe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use salsa_spatial::TrajectoryKnot;

    #[test]
    fn transport_peak_includes_compressed_geometry_and_the_fold() {
        let source = TileSource {
            start_sample: 0,
            frames: 1,
            object_count: 1,
            object_pcm: vec![vec![1.0]],
            lfe_pcm: vec![0.0],
            positions: vec![[-1.0, 1.0, 0.5]],
            active: vec![true],
            events: Vec::new(),
        };
        let knots = vec![vec![TrajectoryKnot {
            sample: 0,
            position: [-1.0, 1.0, 0.5],
        }]];
        let panner = CorePanner::from_config(salsa_spatial::CorePannerConfig::new(0.25).unwrap());
        let transport = build_transport(&source, &knots, 1.0, panner, BaseFold::v1(), 2);
        let angle = 0.5 * 0.25f64.acos();
        assert!((transport.peak() - (angle.cos() + 0.5 * angle.sin())).abs() < 1e-6);
        assert!(transport.peak() > 1.0);
    }

    #[test]
    fn one_object_at_the_right_speaker_lands_in_core_right() {
        let frames = 8;
        let source = TileSource {
            start_sample: 0,
            frames,
            object_count: 1,
            object_pcm: vec![vec![1.0f32; frames]],
            lfe_pcm: vec![0.0; frames],
            positions: vec![[1.0, 1.0, 0.0]; frames],
            active: vec![true; frames],
            events: Vec::new(),
        };
        let knots = vec![vec![TrajectoryKnot {
            sample: 0,
            position: [1.0, 1.0, 0.0],
        }]];
        let (core, lfe) = build_core(&source, &knots, 1.0);
        for sample in 0..frames {
            assert!((core[1][sample] - 1.0).abs() < 1e-5);
            assert!(core[0][sample].abs() < 1e-6);
            assert_eq!(lfe[sample], 0.0);
        }
    }

    #[test]
    fn headroom_gain_scales_every_core_channel() {
        let frames = 4;
        let source = TileSource {
            start_sample: 0,
            frames,
            object_count: 1,
            object_pcm: vec![vec![1.0f32; frames]],
            lfe_pcm: vec![1.0; frames],
            positions: vec![[0.0, 1.0, 0.0]; frames],
            active: vec![true; frames],
            events: Vec::new(),
        };
        let knots = vec![vec![TrajectoryKnot {
            sample: 0,
            position: [0.0, 1.0, 0.0],
        }]];
        let (core, lfe) = build_core(&source, &knots, 0.25);
        for channel in &core {
            for &sample in channel {
                assert!((sample - 0.25 * if channel[0] == 0.25 { 1.0 } else { 0.0 }).abs() < 1e-6);
            }
        }
        assert!((lfe[0] - 0.25).abs() < 1e-6);
    }
}
