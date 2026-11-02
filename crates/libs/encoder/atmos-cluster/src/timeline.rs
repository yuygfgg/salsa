// SPDX-License-Identifier: MPL-2.0
//! Sample the shared DAMF scene onto the clustering analysis grid.

use objectaudio_damf::{Consumer, DamfScene, Manifest, Metadata, ResolvedObject};
pub type Timeline = ResolvedObject;

#[derive(Debug)]
pub struct SourceSet {
    pub sources: Vec<Timeline>,
    pub lfe: Option<ResolvedObject>,
    pub lfe_channel: Option<usize>,
}
impl SourceSet {
    pub fn n_sources(&self) -> usize {
        self.sources.len()
    }
}

pub fn build_sources(manifest: &Manifest, metadata: &Metadata) -> anyhow::Result<SourceSet> {
    let scene = DamfScene::resolve(manifest, metadata)?;
    for warning in scene
        .warnings
        .iter()
        .chain(&scene.report(Consumer::Cluster).approximations)
    {
        log::warn!("{warning}");
    }
    Ok(SourceSet {
        lfe_channel: scene.lfe.as_ref().map(|source| source.channel),
        sources: scene.objects,
        lfe: scene.lfe,
    })
}

#[derive(Debug)]
pub struct FrameTracks {
    pub n_frames: usize,
    pub n_sources: usize,
    pub positions: Vec<[f64; 3]>,
    pub active: Vec<bool>,
    pub sizes: Vec<f64>,
    pub importance: Vec<f64>,
    pub dialog: Vec<f64>,
    pub music: Vec<f64>,
    pub zones: Vec<String>,
    pub priorities: Vec<f64>,
}

pub fn sample_tracks(
    sources: &[Timeline],
    n_frames: usize,
    hop: usize,
    total_frames: u64,
    dialog_boost_db: f64,
) -> FrameTracks {
    let n_sources = sources.len();
    let mut positions = vec![[0.0f64; 3]; n_frames * n_sources];
    let mut active = vec![false; n_frames * n_sources];
    let mut sizes = vec![0.0f64; n_frames * n_sources];
    let mut importance = vec![0.0f64; n_frames * n_sources];
    let mut dialog = vec![-1.0f64; n_frames * n_sources];
    let mut music = vec![-1.0f64; n_frames * n_sources];
    let mut zones = vec![String::new(); n_frames * n_sources];

    for (source_index, source) in sources.iter().enumerate() {
        for frame in 0..n_frames {
            let query = ((frame as u64) * hop as u64).min(total_frames.saturating_sub(1));
            let index = frame * n_sources + source_index;

            let state = source.track.state_at(query);
            let content = source.track.metadata_at(query);
            positions[index] = state.position.map(f64::from);
            active[index] = state.enabled;
            sizes[index] = f64::from(state.size.unwrap_or(0.0));
            importance[index] = f64::from(content.importance);
            dialog[index] = f64::from(content.dialog);
            music[index] = f64::from(content.music);
            zones[index] = content.zones.clone();
        }
    }

    let boost = 10.0_f64.powf(dialog_boost_db / 20.0);
    let mut priorities = vec![0.0f64; n_frames * n_sources];
    for index in 0..priorities.len() {
        let mut prior = importance[index].max(0.0);
        if dialog_boost_db != 0.0 && dialog[index] >= 0.5 {
            prior *= boost;
        }
        priorities[index] = prior;
    }

    FrameTracks {
        n_frames,
        n_sources,
        positions,
        active,
        sizes,
        importance,
        dialog,
        music,
        zones,
        priorities,
    }
}
