//! Resolve DAMF patches once, before any audio or metadata consumer runs.

use std::collections::{BTreeMap, HashMap, HashSet};

use objectaudio_ir::{ObjectEvent, ObjectState, SpeakerPosition, StateTrack, Transition};

use crate::{DamfError, DamfEvent, Manifest, Metadata, Result};

/// Complete inherited DAMF properties. Format-only properties stay here until
/// a consumer explicitly projects them into its supported subset.
#[derive(Debug, Clone)]
pub struct ResolvedMetadata {
    pub position: [f32; 3],
    pub gain: f32,
    pub enabled: bool,
    pub ramp_length: u32,
    pub snap: bool,
    pub elevation: bool,
    pub zones: String,
    pub size: f32,
    pub decorr: u32,
    pub importance: f32,
    pub trim_bypass: bool,
    pub dialog: f32,
    pub music: f32,
    pub screen_factor: f32,
    pub depth_factor: f32,
    pub head_track_mode: String,
    pub binaural_render_mode: String,
}

impl Default for ResolvedMetadata {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            gain: 1.0,
            enabled: true,
            ramp_length: 0,
            snap: false,
            elevation: true,
            zones: "all".into(),
            size: 0.0,
            decorr: 0,
            importance: 1.0,
            trim_bypass: false,
            dialog: -1.0,
            music: -1.0,
            screen_factor: 0.0,
            depth_factor: 0.25,
            head_track_mode: "undefined".into(),
            binaural_render_mode: "undefined".into(),
        }
    }
}

impl ResolvedMetadata {
    fn apply(&mut self, event: &DamfEvent) {
        // No `..`: a new format property must be handled at this boundary.
        let DamfEvent {
            id: _,
            sample_pos: _,
            active,
            position,
            snap,
            elevation,
            zones,
            size,
            decorr,
            importance,
            gain,
            ramp_length,
            trim_bypass,
            dialog,
            music,
            screen_factor,
            depth_factor,
            head_track_mode,
            binaural_render_mode,
        } = event;
        if let Some(value) = active {
            self.enabled = *value;
        }
        if let Some(value) = position {
            self.position = *value;
        }
        if let Some(value) = gain {
            self.gain = value.linear();
        }
        macro_rules! inherit {
            ($($field:ident),+ $(,)?) => { $(if let Some(value) = $field { self.$field.clone_from(value); })+ };
        }
        inherit!(
            snap,
            elevation,
            zones,
            size,
            decorr,
            importance,
            ramp_length,
            trim_bypass,
            dialog,
            music,
            screen_factor,
            depth_factor,
            head_track_mode,
            binaural_render_mode
        );
    }

    fn spatial(&self) -> ObjectState {
        ObjectState {
            id: 1,
            position: self.position,
            gain: self.gain,
            enabled: self.enabled,
            size: Some(self.size),
            spread: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Consumer {
    Render,
    Cluster,
    Encode,
}

pub use objectaudio_ir::ConversionReport;

fn inspect(report: &mut ConversionReport, state: &ResolvedMetadata, consumer: Consumer) {
    let ResolvedMetadata {
        position: _,
        gain: _,
        enabled: _,
        ramp_length: _,
        snap,
        elevation,
        zones,
        size,
        decorr,
        importance,
        trim_bypass,
        dialog,
        music,
        screen_factor,
        depth_factor,
        head_track_mode,
        binaural_render_mode,
    } = state;
    let mut unsupported = |field, used| {
        if used {
            report.record(field, "not represented by this consumer");
        }
    };
    unsupported("snap", *snap);
    unsupported("elevation", !*elevation);
    unsupported("decorr", *decorr != 0);
    if matches!(consumer, Consumer::Cluster) {
        unsupported("trimBypass", *trim_bypass);
    }
    unsupported("screenFactor", *screen_factor != 0.0);
    unsupported("depthFactor", *depth_factor != 0.25);
    unsupported("headTrackMode", head_track_mode != "undefined");
    unsupported("binauralRenderMode", binaural_render_mode != "undefined");
    match consumer {
        Consumer::Cluster => {}
        Consumer::Render | Consumer::Encode => {
            unsupported("zones", zones != "all");
            unsupported("importance", *importance != 1.0);
            unsupported("dialog", *dialog >= 0.0);
            unsupported("music", *music >= 0.0);
            unsupported("size", *size != 0.0);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObjectTrack {
    spatial: StateTrack,
    metadata: Vec<(u64, ResolvedMetadata)>,
}

impl ObjectTrack {
    pub fn fixed(position: [f32; 3]) -> Self {
        Self::build(&[], Some(position))
    }
    pub fn dynamic(events: &[&DamfEvent]) -> Self {
        Self::build(events, None)
    }

    fn build(events: &[&DamfEvent], fixed: Option<[f32; 3]>) -> Self {
        let mut events = events.to_vec();
        events.sort_by_key(|event| event.sample_pos);
        let mut state = ResolvedMetadata::default();
        let mut keys = BTreeMap::new();
        let mut spatial_keys = BTreeMap::new();
        for event in events {
            state.apply(event);
            if let Some(position) = fixed {
                state.position = position;
            }
            keys.insert(event.sample_pos, state.clone());
            // A metadata-only patch must not restart an inherited position ramp.
            if event.position.is_some()
                || event.gain.is_some()
                || event.active.is_some()
                || event.size.is_some()
                || event.ramp_length.is_some()
            {
                spatial_keys.insert(event.sample_pos, (state.spatial(), state.ramp_length));
            }
        }
        if keys.is_empty() {
            if let Some(position) = fixed {
                state.position = position;
            }
            keys.insert(0, state);
        }
        let metadata: Vec<_> = keys.into_iter().collect();
        let mut spatial = StateTrack::new(metadata[0].1.spatial());
        for (sample, (state, ramp)) in spatial_keys {
            spatial.set_target(sample as i64, state, Transition::Position(ramp));
        }
        Self { spatial, metadata }
    }

    pub fn metadata_at(&self, sample: u64) -> &ResolvedMetadata {
        let index = self
            .metadata
            .partition_point(|(at, _)| *at <= sample)
            .saturating_sub(1);
        &self.metadata[index].1
    }

    /// Changes to the discrete bypass state strictly inside `(start, end)`.
    pub fn bypass_changes(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, bool)> + '_ {
        let first = self
            .metadata
            .partition_point(|(sample, _)| *sample <= start)
            .saturating_sub(1);
        self.metadata[first..]
            .windows(2)
            .take_while(move |pair| pair[1].0 < end)
            .filter(move |pair| pair[1].0 > start && pair[0].1.trim_bypass != pair[1].1.trim_bypass)
            .map(|pair| (pair[1].0, pair[1].1.trim_bypass))
    }

    pub fn state_at(&self, sample: u64) -> ObjectState {
        self.spatial.state_at(sample as i64)
    }
    pub fn position_at(&self, sample: u64) -> [f32; 3] {
        self.state_at(sample).position
    }
    pub fn effective_gain_at(&self, sample: u64) -> f32 {
        self.state_at(sample).effective_gain()
    }

    pub fn events(&self, start: u64, frames: usize, id: u32) -> Vec<ObjectEvent> {
        self.spatial
            .events(start as i64, frames)
            .into_iter()
            .map(|mut event| {
                event.state.id = id;
                event
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedObject {
    pub obj_id: i64,
    pub channel: usize,
    pub name: String,
    pub fixed_pos: Option<[f64; 3]>,
    pub track: ObjectTrack,
}

impl ResolvedObject {
    pub fn is_fixed(&self) -> bool {
        self.fixed_pos.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct DamfScene {
    pub objects: Vec<ResolvedObject>,
    pub lfe: Option<ResolvedObject>,
    pub source_channels: usize,
    pub warnings: Vec<String>,
}

impl DamfScene {
    pub fn resolve(manifest: &Manifest, metadata: &Metadata) -> Result<Self> {
        let mut ids = HashSet::new();
        for id in manifest
            .beds
            .iter()
            .map(|bed| bed.id)
            .chain(manifest.objects.iter().map(|object| object.id))
        {
            if !ids.insert(id) {
                return Err(DamfError::Invalid(format!("duplicate element id {id}")));
            }
        }
        let mut by_id: HashMap<i64, Vec<&DamfEvent>> = HashMap::new();
        for event in &metadata.events {
            if !ids.contains(&event.id) {
                return Err(DamfError::Invalid(format!(
                    "metadata references unknown element {}",
                    event.id
                )));
            }
            by_id.entry(event.id).or_default().push(event);
        }
        let mut scene = Self {
            objects: Vec::new(),
            lfe: None,
            source_channels: manifest.beds.len() + manifest.objects.len(),
            warnings: Vec::new(),
        };
        for (channel, bed) in manifest.beds.iter().enumerate() {
            let speaker = SpeakerPosition::from_bed_name(&bed.name).ok_or_else(|| {
                DamfError::Invalid(format!("unsupported bed channel {:?}", bed.name))
            })?;
            if speaker == SpeakerPosition::Lfe2 {
                return Err(DamfError::Invalid("LFE2 bed is not supported".into()));
            }
            let position = speaker.adm_position();
            let object = ResolvedObject {
                obj_id: bed.id,
                channel,
                name: bed.name.clone(),
                fixed_pos: Some(position.map(f64::from)),
                track: ObjectTrack::build(
                    by_id.get(&bed.id).map(Vec::as_slice).unwrap_or(&[]),
                    Some(position),
                ),
            };
            if speaker == SpeakerPosition::Lfe {
                if scene.lfe.is_some() {
                    return Err(DamfError::Invalid("multiple LFE beds".into()));
                }
                scene.lfe = Some(object);
            } else {
                scene.objects.push(object);
            }
        }
        for (index, object) in manifest.objects.iter().enumerate() {
            scene.objects.push(ResolvedObject {
                obj_id: object.id,
                channel: manifest.beds.len() + index,
                name: object
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("object {}", object.id)),
                fixed_pos: None,
                track: ObjectTrack::dynamic(
                    by_id.get(&object.id).map(Vec::as_slice).unwrap_or(&[]),
                ),
            });
        }
        Ok(scene)
    }

    pub fn report(&self, consumer: Consumer) -> ConversionReport {
        let mut report = ConversionReport::default();
        for object in self.objects.iter().chain(self.lfe.iter()) {
            for (_, state) in &object.track.metadata {
                inspect(&mut report, state, consumer);
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::BedEntry;

    fn resolve_beds(names: &[&str]) -> Result<DamfScene> {
        let manifest = Manifest {
            path: "test.atmos".into(),
            audio: "test.atmos.audio".into(),
            metadata: "test.atmos.metadata".into(),
            offset: 0.0,
            fps: 24.0,
            beds: names
                .iter()
                .enumerate()
                .map(|(id, name)| BedEntry {
                    id: id as i64,
                    name: (*name).into(),
                })
                .collect(),
            objects: vec![],
        };
        DamfScene::resolve(
            &manifest,
            &Metadata {
                sample_rate: 48_000,
                events: vec![],
            },
        )
    }

    #[test]
    fn beds_keep_source_channels_and_use_shared_positions() {
        let names = ["L", "LFE", "Ls", "Lrs", "Lfh", "Tsl", "Lrh", "Lw"];
        let scene = resolve_beds(&names).unwrap();
        assert_eq!(scene.source_channels, names.len());
        assert_eq!(scene.lfe.unwrap().channel, 1);
        assert_eq!(scene.objects.len(), names.len() - 1);
        for object in scene.objects {
            let expected = crate::bed_position(names[object.channel]).unwrap();
            assert_eq!(object.fixed_pos, Some(expected.map(f64::from)));
        }
    }

    #[test]
    fn unknown_and_second_lfe_beds_fail_before_conversion() {
        for name in ["unknown", "LFE2"] {
            let error = resolve_beds(&["LFE", name]).unwrap_err();
            assert!(error.to_string().contains(name), "{error}");
        }
        assert!(resolve_beds(&["LFE", " lfe "]).is_err());
        assert!(resolve_beds(&[" lfe "]).unwrap().lfe.is_some());
    }
}
