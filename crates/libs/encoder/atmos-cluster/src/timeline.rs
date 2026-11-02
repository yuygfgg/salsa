// SPDX-License-Identifier: MPL-2.0
//! Per-source metadata timelines sampled onto the clustering frame grid.

use std::collections::{BTreeMap, HashMap};

use objectaudio_damf::{DamfEvent, Manifest, Metadata, ObjectTrack, bed_position};

#[derive(Debug, Clone)]
struct ContentKeyframe {
    sample: u64,
    importance: f64,
    dialog: f64,
    music: f64,
    zones: String,
}

impl Default for ContentKeyframe {
    fn default() -> Self {
        Self {
            sample: 0,
            importance: 1.0,
            dialog: -1.0,
            music: -1.0,
            zones: "all".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Timeline {
    pub obj_id: i64,
    pub channel: usize,
    pub fixed_pos: Option<[f64; 3]>,
    pub track: ObjectTrack,
    pub zones: String,
    content: Vec<ContentKeyframe>,
}

impl Timeline {
    pub fn is_fixed(&self) -> bool {
        self.fixed_pos.is_some()
    }

    fn content_at(&self, sample: u64) -> &ContentKeyframe {
        let index = self.content.partition_point(|key| key.sample <= sample);
        &self.content[index.saturating_sub(1)]
    }
}

#[derive(Debug)]
pub struct SourceSet {
    pub sources: Vec<Timeline>,
    pub lfe_channel: Option<usize>,
    pub bed_count: usize,
    pub channels: Vec<usize>,
}

impl SourceSet {
    pub fn n_sources(&self) -> usize {
        self.sources.len()
    }

    pub fn fixed_indices(&self) -> Vec<usize> {
        self.sources
            .iter()
            .enumerate()
            .filter(|(_, source)| source.is_fixed())
            .map(|(index, _)| index)
            .collect()
    }
}

fn fixed_timeline(obj_id: i64, channel: usize, position: [f32; 3]) -> Timeline {
    Timeline {
        obj_id,
        channel,
        fixed_pos: Some(position.map(f64::from)),
        track: ObjectTrack::fixed(position),
        zones: "all".to_string(),
        content: vec![ContentKeyframe::default()],
    }
}

fn dynamic_timeline(obj_id: i64, channel: usize, events: &[&DamfEvent]) -> Timeline {
    let mut key = ContentKeyframe::default();
    let mut keys = BTreeMap::new();
    for event in events {
        key.sample = event.sample_pos;
        if let Some(value) = event.importance {
            key.importance = f64::from(value);
        }
        if let Some(value) = event.dialog {
            key.dialog = f64::from(value);
        }
        if let Some(value) = event.music {
            key.music = f64::from(value);
        }
        if let Some(value) = &event.zones {
            key.zones = value.clone();
        }
        keys.insert(key.sample, key.clone());
    }
    if keys.is_empty() {
        keys.insert(0, ContentKeyframe::default());
    }
    let content: Vec<_> = keys.into_values().collect();

    Timeline {
        obj_id,
        channel,
        fixed_pos: None,
        track: ObjectTrack::dynamic(events),
        zones: content[0].zones.clone(),
        content,
    }
}

pub fn build_sources(manifest: &Manifest, metadata: &Metadata) -> SourceSet {
    let mut by_id: HashMap<i64, Vec<&DamfEvent>> = HashMap::new();
    for event in &metadata.events {
        by_id.entry(event.id).or_default().push(event);
    }

    let mut sources = Vec::new();
    let mut channels = Vec::new();
    let mut lfe_channel = None;
    let bed_count = manifest.beds.len();

    for (channel, bed) in manifest.beds.iter().enumerate() {
        if bed.name.eq_ignore_ascii_case("LFE") {
            if lfe_channel.is_none() {
                lfe_channel = Some(channel);
            }
            continue;
        }
        let position = bed_position(&bed.name).unwrap_or_else(|| {
            log::warn!(
                "unknown bed channel {:?}; using a centre fixed position",
                bed.name
            );
            [0.0, 0.0, 0.0]
        });
        sources.push(fixed_timeline(bed.id, channel, position));
        channels.push(channel);
    }

    for (index, object) in manifest.objects.iter().enumerate() {
        let channel = bed_count + index;
        let events = by_id.get(&object.id).map(Vec::as_slice).unwrap_or(&[]);
        sources.push(dynamic_timeline(object.id, channel, events));
        channels.push(channel);
    }

    SourceSet {
        sources,
        lfe_channel,
        bed_count,
        channels,
    }
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

    for (source_index, source) in sources.iter().enumerate() {
        for frame in 0..n_frames {
            let query = ((frame as u64) * hop as u64).min(total_frames.saturating_sub(1));
            let index = frame * n_sources + source_index;

            let state = source.track.state_at(query);
            let content = source.content_at(query);
            positions[index] = state.position.map(f64::from);
            active[index] = state.enabled;
            sizes[index] = f64::from(state.size.unwrap_or(0.0));
            importance[index] = content.importance;
            dialog[index] = content.dialog;
            music[index] = content.music;
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
        zones: sources.iter().map(|source| source.zones.clone()).collect(),
        priorities,
    }
}
