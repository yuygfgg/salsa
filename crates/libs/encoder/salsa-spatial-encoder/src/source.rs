//! DAMF source preparation: authored gain and activity applied once, at audio
//! rate, plus per-sample positions for trajectory fitting and the headroom
//! prepass.

use std::path::Path;

use anyhow::{Context, Result, bail};
use objectaudio_damf::{DamfInfo, DamfSource};
use objectaudio_ir::{SceneTimeline, Transition};
use rayon::prelude::*;

/// One authored scene event retained for the decoded IR.
#[derive(Debug, Clone, Copy)]
pub struct SourceEvent {
    pub object: usize,
    pub offset: i64,
    pub transition: Transition,
    pub enabled: bool,
}

/// One tile of source PCM with authored gain already applied to the objects.
#[derive(Debug, Clone)]
pub struct TileSource {
    pub start_sample: u64,
    pub frames: usize,
    pub object_count: usize,
    /// Planar object PCM, authored gain applied exactly once.
    pub object_pcm: Vec<Vec<f32>>,
    /// The separate LFE channel (DamfSource already applied its authored gain).
    pub lfe_pcm: Vec<f32>,
    /// `[sample][object]` ADM positions.
    pub positions: Vec<[f32; 3]>,
    /// `[sample][object]` authored activity (enabled with nonzero gain).
    pub active: Vec<bool>,
    pub events: Vec<SourceEvent>,
}

impl TileSource {
    pub fn position(&self, sample: usize, object: usize) -> [f32; 3] {
        self.positions[sample * self.object_count + object]
    }

    pub fn is_active(&self, sample: usize, object: usize) -> bool {
        self.active[sample * self.object_count + object]
    }

    /// Peak absolute value of the authored 5.1.2 downmix plus LFE.
    pub fn downmix_peak(&self) -> f64 {
        (0..self.frames)
            .into_par_iter()
            .map(|sample| self.sample_peak(sample))
            .reduce(|| 0.0, f64::max)
    }

    fn sample_peak(&self, sample: usize) -> f64 {
        let mut core = [0.0f64; 7];
        for object in 0..self.object_count {
            let value = f64::from(self.object_pcm[object][sample]);
            if value == 0.0 {
                continue;
            }
            let position = self.position(sample, object).map(f64::from);
            let column = salsa_spatial_dsp::core_gain_column(position);
            for channel in 0..7 {
                core[channel] += column[channel] * value;
            }
        }
        core.into_iter()
            .map(f64::abs)
            .fold(f64::from(self.lfe_pcm[sample]).abs(), f64::max)
    }
}

/// Sequential tile reader over a `DamfSource`.
pub struct SourceReader {
    source: DamfSource,
    info: DamfInfo,
}

impl SourceReader {
    pub fn open(manifest: impl AsRef<Path>) -> Result<Self> {
        let source = DamfSource::open(manifest)?;
        let info = source.info().clone();
        if info.sample_rate != salsa_spatial::SAMPLE_RATE {
            bail!(
                "salsa-spatial requires 48 kHz, source is {} Hz",
                info.sample_rate
            );
        }
        Ok(Self { source, info })
    }

    pub fn info(&self) -> &DamfInfo {
        &self.info
    }

    pub fn seek(&mut self, sample: u64) -> Result<()> {
        self.source.seek(sample)?;
        Ok(())
    }

    pub fn read_tile(&mut self, frames: usize) -> Result<Option<TileSource>> {
        let block = self.source.read_block(frames)?;
        let Some(block) = block else {
            return Ok(None);
        };
        if block.frames == 0 {
            return Ok(None);
        }
        let object_count = self.info.objects.len();
        let output_channels = self.info.output_channels;
        if block.pcm.len() != block.frames * output_channels {
            bail!("DAMF block PCM length does not match its channel count");
        }

        let start = block.start_sample;
        let timeline = SceneTimeline::from_payload(&block.payload, start as i64)
            .context("building source scene timeline")?;

        let mut object_pcm = vec![vec![0.0f32; block.frames]; object_count];
        let mut lfe_pcm = vec![0.0f32; block.frames];
        let mut positions = vec![[0.0f32; 3]; block.frames * object_count];
        let mut active = vec![false; block.frames * object_count];

        for sample in 0..block.frames {
            let base = sample * output_channels;
            lfe_pcm[sample] = block.pcm[base];
            let sample_position = sample * object_count;
            for object in 0..object_count {
                let state = timeline.state_at(object, (start + sample as u64) as i64);
                object_pcm[object][sample] =
                    apply_authored_gain(block.pcm[base + 1 + object], &state);
                positions[sample_position + object] = state.position;
                active[sample_position + object] = state.enabled && state.gain > 0.0;
            }
        }

        let mut events = Vec::new();
        for event in &block.payload.events {
            if event.offset < 0 || event.offset >= block.frames as i64 {
                continue;
            }
            let object = event.state.id as usize;
            if object == 0 || object > object_count {
                continue;
            }
            events.push(SourceEvent {
                object: object - 1,
                offset: event.offset,
                transition: event.transition,
                enabled: event.state.enabled,
            });
        }
        events.sort_by_key(|event| event.offset);

        Ok(Some(TileSource {
            start_sample: start,
            frames: block.frames,
            object_count,
            object_pcm,
            lfe_pcm,
            positions,
            active,
            events,
        }))
    }
}

/// Apply the authored object gain and activity exactly once.
pub fn apply_authored_gain(raw: f32, state: &objectaudio_ir::ObjectState) -> f32 {
    if state.enabled { raw * state.gain } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objectaudio_ir::ObjectState;

    #[test]
    fn authored_gain_is_applied_once_and_activity_mutes() {
        let mut state = ObjectState::new(1, [0.0, 1.0, 0.0]);
        state.gain = 0.5;
        assert_eq!(apply_authored_gain(2.0, &state), 1.0);
        state.enabled = false;
        assert_eq!(apply_authored_gain(2.0, &state), 0.0);
    }

    #[test]
    fn parallel_peak_preserves_object_cancellation_and_includes_lfe() {
        let frames = 1024;
        let mut source = TileSource {
            start_sample: 0,
            frames,
            object_count: 2,
            object_pcm: vec![vec![0.5; frames], vec![-0.5; frames]],
            lfe_pcm: vec![0.0; frames],
            positions: vec![[1.0, 1.0, 0.0]; frames * 2],
            active: vec![true; frames * 2],
            events: Vec::new(),
        };
        source.lfe_pcm[frames - 1] = -0.75;
        assert_eq!(source.downmix_peak(), 0.75);
        source.object_pcm[1].fill(0.5);
        assert_eq!(source.downmix_peak(), 1.0);
    }
}
