//! DAMF presentation -> salsa object-audio blocks.
//!
//! This follows the `PosTimeline` semantics in `dolby-atmos-encoder`:
//!
//! * `samplePos` is inherited by events that omit it;
//! * non-LFE beds become fixed-position objects;
//! * an event holds the previous position until its sample time, then ramps to
//!   the new target over `rampLength` samples;
//! * `gain`, `active` and `size` are held between keyframes;
//! * the trajectory is sampled at block boundaries, and each changed object
//!   gets one ramp to its end-of-block state. The encoder's OAMD path also
//!   samples positions at frame end and writes a full-frame ramp, so this keeps
//!   the renderer's full-snapshot event model consistent across objects.
//!
//! `rampLength` drives the position trajectory. The salsa event ramp is the
//! source block length because the renderer events carry full object snapshots
//! and all objects in a block must share one ramp.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use objectaudio_ir::{ObjectEvent, ObjectState, SpatialPayload};

use crate::caf::CafReader;
use crate::error::{DamfError, Result};
use crate::manifest::Manifest;
use crate::metadata::{Event as DamfEvent, Metadata};

#[derive(Debug, Clone, Copy, PartialEq)]
struct State {
    position: [f32; 3],
    gain: f32,
    enabled: bool,
    size: Option<f32>,
}

impl State {
    fn object(self, id: u32) -> ObjectState {
        let mut object = ObjectState::new(id, self.position);
        object.gain = self.gain;
        object.enabled = self.enabled;
        object.size = self.size;
        object
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            position: [0.0, 0.0, 0.0],
            gain: 1.0,
            enabled: true,
            size: Some(0.0),
        }
    }
}

#[derive(Debug, Clone)]
struct Keyframe {
    sample: u64,
    state: State,
    /// Forward ramp length in samples, starting at `sample`.
    ramp: u32,
}

#[derive(Debug, Clone)]
struct Track {
    keys: Vec<Keyframe>,
    /// Position at each key's sample time; the previous key's ramp may still run.
    starts: Vec<[f32; 3]>,
}

impl Track {
    fn fixed(position: [f32; 3]) -> Self {
        Self {
            keys: vec![Keyframe {
                sample: 0,
                state: State {
                    position,
                    ..State::default()
                },
                ramp: 0,
            }],
            starts: vec![position],
        }
    }

    fn dynamic(events: &[&DamfEvent]) -> Self {
        let mut state = State::default();
        let mut ramp = 0u32;
        let mut by_sample: BTreeMap<u64, (State, u32)> = BTreeMap::new();
        for event in events {
            if let Some(position) = event.position {
                state.position = position;
            }
            if let Some(gain) = event.gain {
                state.gain = gain.linear();
            }
            if let Some(active) = event.active {
                state.enabled = active;
            }
            if let Some(size) = event.size {
                state.size = Some(size);
            }
            if let Some(value) = event.ramp_length {
                ramp = value;
            }
            // Last event at the same sample wins.
            by_sample.insert(event.sample_pos, (state, ramp));
        }

        let mut keys: Vec<Keyframe> = by_sample
            .into_iter()
            .map(|(sample, (state, ramp))| Keyframe {
                sample,
                state,
                ramp,
            })
            .collect();
        if keys.is_empty() {
            keys.push(Keyframe {
                sample: 0,
                state: State::default(),
                ramp: 0,
            });
        }
        let starts = key_starts(&keys);
        Self { keys, starts }
    }

    /// `dolby-atmos-encoder` `PosTimeline::at`: hold until the key sample, then
    /// ramp forward over `rampLength`.
    fn position_at(&self, sample: u64) -> [f32; 3] {
        if self.keys.is_empty() {
            return [0.0; 3];
        }
        let index = self.keys.partition_point(|key| key.sample <= sample);
        if index == 0 {
            return self.keys[0].state.position;
        }
        let key_index = index - 1;
        let key = &self.keys[key_index];
        let start = self.starts[key_index];
        if key.ramp == 0 {
            return key.state.position;
        }
        let fraction =
            ((sample.saturating_sub(key.sample)) as f64 / f64::from(key.ramp)).clamp(0.0, 1.0);
        [
            lerp(start[0], key.state.position[0], fraction),
            lerp(start[1], key.state.position[1], fraction),
            lerp(start[2], key.state.position[2], fraction),
        ]
    }

    /// Values that change at a keyframe and then hold until the next keyframe.
    fn step_at(&self, sample: u64) -> &State {
        let index = self
            .keys
            .partition_point(|key| key.sample <= sample)
            .saturating_sub(1)
            .min(self.keys.len() - 1);
        &self.keys[index].state
    }

    fn state_at(&self, sample: u64) -> State {
        let held = self.step_at(sample);
        State {
            position: self.position_at(sample),
            ..*held
        }
    }
}

fn key_starts(keys: &[Keyframe]) -> Vec<[f32; 3]> {
    let mut starts = Vec::with_capacity(keys.len());
    for (index, key) in keys.iter().enumerate() {
        if index == 0 {
            starts.push(key.state.position);
            continue;
        }
        let previous = &keys[index - 1];
        let fraction = if previous.ramp == 0 {
            1.0
        } else {
            (key.sample.saturating_sub(previous.sample) as f64 / f64::from(previous.ramp))
                .clamp(0.0, 1.0)
        };
        let start = starts[index - 1];
        starts.push([
            lerp(start[0], previous.state.position[0], fraction),
            lerp(start[1], previous.state.position[1], fraction),
            lerp(start[2], previous.state.position[2], fraction),
        ]);
    }
    starts
}

fn lerp(lower: f32, upper: f32, fraction: f64) -> f32 {
    (f64::from(lower) + (f64::from(upper) - f64::from(lower)) * fraction) as f32
}

/// One source object after the manifest's bed/object layout has been flattened.
#[derive(Debug, Clone, PartialEq)]
pub struct DamfObjectInfo {
    /// Salsa object id, contiguous from 1.
    pub id: u32,
    /// Original DAMF object/bed id.
    pub damf_id: i64,
    /// Channel index in the CAF file.
    pub source_channel: usize,
    pub name: String,
    pub fixed: bool,
    /// Fixed position for a bed, or the initial position for a dynamic object.
    pub position: [f32; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct DamfInfo {
    pub sample_rate: u32,
    pub source_channels: usize,
    /// `1 + objects.len()`: LFE first, then point objects.
    pub output_channels: usize,
    pub total_frames: Option<u64>,
    pub lfe_channel: Option<usize>,
    pub objects: Vec<DamfObjectInfo>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DamfBlock {
    pub sample_rate: u32,
    pub start_sample: u64,
    pub frames: usize,
    /// Interleaved `[sample][LFE + objects]`.
    pub pcm: Vec<f32>,
    pub payload: SpatialPayload,
}

/// A sequential DAMF reader that produces salsa object-audio blocks.
#[derive(Debug)]
pub struct DamfSource {
    caf: CafReader,
    info: DamfInfo,
    tracks: Vec<Track>,
    next_sample: u64,
}

impl DamfSource {
    pub fn open(manifest: impl AsRef<Path>) -> Result<Self> {
        let manifest = Manifest::load(manifest)?;
        let metadata = Metadata::load(&manifest.metadata)?;
        let caf = CafReader::open(&manifest.audio)?;
        let source_channels = caf.info().channels;
        let sample_rate = caf.info().sample_rate;

        let mut warnings = Vec::new();
        if metadata.sample_rate != sample_rate {
            warnings.push(format!(
                "metadata sampleRate {} differs from CAF rate {sample_rate}; using CAF",
                metadata.sample_rate
            ));
        }

        let bed_count = manifest.beds.len();
        let expected_channels = bed_count + manifest.objects.len();
        if expected_channels != source_channels {
            return Err(DamfError::ChannelMismatch {
                caf: source_channels,
                manifest: expected_channels,
            });
        }

        let mut by_id: HashMap<i64, Vec<&DamfEvent>> = HashMap::new();
        for event in &metadata.events {
            by_id.entry(event.id).or_default().push(event);
        }

        let mut objects = Vec::new();
        let mut tracks = Vec::new();
        let mut lfe_channel = None;

        for (channel, bed) in manifest.beds.iter().enumerate() {
            if bed.name.eq_ignore_ascii_case("LFE") {
                if lfe_channel.is_none() {
                    lfe_channel = Some(channel);
                } else {
                    warnings.push(format!(
                        "additional LFE bed id {} on channel {channel} is ignored",
                        bed.id
                    ));
                }
                continue;
            }
            let position = match bed_position(&bed.name) {
                Some(position) => position,
                None => {
                    warnings.push(format!(
                        "unknown bed channel {:?}; using a centre fixed position",
                        bed.name
                    ));
                    [0.0, 0.0, 0.0]
                }
            };
            objects.push(DamfObjectInfo {
                id: objects.len() as u32 + 1,
                damf_id: bed.id,
                source_channel: channel,
                name: bed.name.clone(),
                fixed: true,
                position,
            });
            tracks.push(Track::fixed(position));
        }

        for (index, object) in manifest.objects.iter().enumerate() {
            let source_channel = bed_count + index;
            let events = by_id.get(&object.id).map(Vec::as_slice).unwrap_or(&[]);
            let track = Track::dynamic(events);
            let position = track.state_at(0).position;
            objects.push(DamfObjectInfo {
                id: objects.len() as u32 + 1,
                damf_id: object.id,
                source_channel,
                name: object
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("object {}", object.id)),
                fixed: false,
                position,
            });
            tracks.push(track);
        }

        if lfe_channel.is_none() {
            warnings.push("no LFE bed found; a silent LFE channel will be emitted".to_string());
        }

        let info = DamfInfo {
            sample_rate,
            source_channels,
            output_channels: objects.len() + 1,
            total_frames: caf.total_frames(),
            lfe_channel,
            objects,
            warnings,
        };

        Ok(Self {
            caf,
            info,
            tracks,
            next_sample: 0,
        })
    }

    pub fn info(&self) -> &DamfInfo {
        &self.info
    }

    pub fn next_sample(&self) -> u64 {
        self.next_sample
    }

    /// Move to `frame` (clamped to the essence length). The object timeline is
    /// stateless, so no decoder reset is needed.
    pub fn seek(&mut self, frame: u64) -> Result<()> {
        let frame = self
            .info
            .total_frames
            .map_or(frame, |total| frame.min(total));
        self.caf.seek_to_frame(frame)?;
        self.next_sample = frame;
        Ok(())
    }

    /// Read the next block. `frames` is the requested block size; the final
    /// block may be shorter. Returns `None` at the end of the essence.
    pub fn read_block(&mut self, frames: usize) -> Result<Option<DamfBlock>> {
        if frames == 0 {
            return Ok(None);
        }
        let remaining = self
            .info
            .total_frames
            .map(|total| total.saturating_sub(self.next_sample));
        let frames = remaining.map_or(frames, |remaining| frames.min(remaining as usize));
        if frames == 0 {
            return Ok(None);
        }

        let raw = self.caf.read_frames(frames)?;
        if raw.is_empty() {
            return Ok(None);
        }
        let frames = raw.len() / self.info.source_channels;
        if frames == 0 {
            return Ok(None);
        }

        let start = self.next_sample;
        let end = start + frames as u64;

        let mut pcm = vec![0.0f32; frames * self.info.output_channels];
        for frame in 0..frames {
            let source_base = frame * self.info.source_channels;
            let output_base = frame * self.info.output_channels;
            if let Some(lfe) = self.info.lfe_channel {
                pcm[output_base] = raw[source_base + lfe];
            }
            for (index, object) in self.info.objects.iter().enumerate() {
                pcm[output_base + 1 + index] = raw[source_base + object.source_channel];
            }
        }

        let objects: Vec<ObjectState> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| track.state_at(start).object(index as u32 + 1))
            .collect();

        // The renderer treats every event as a full object snapshot, so all
        // objects in a buffer must share one ramp. Sample the encoder
        // trajectory at the buffer boundaries and ramp from start to end.
        let mut events = Vec::new();
        for (index, track) in self.tracks.iter().enumerate() {
            let object = index as u32 + 1;
            let begin = track.state_at(start);
            let finish = track.state_at(end);
            if begin != finish {
                events.push(ObjectEvent {
                    offset: 0,
                    ramp_samples: u32::try_from(frames).unwrap_or(u32::MAX),
                    object,
                    state: finish.object(object),
                });
            }
        }

        let payload = SpatialPayload::new(self.info.sample_rate, objects, events);
        self.next_sample = end;

        Ok(Some(DamfBlock {
            sample_rate: self.info.sample_rate,
            start_sample: start,
            frames,
            pcm,
            payload,
        }))
    }
}

fn bed_position(name: &str) -> Option<[f32; 3]> {
    let position = match name.trim().to_ascii_uppercase().as_str() {
        "L" => [-1.0, 1.0, 0.0],
        "R" => [1.0, 1.0, 0.0],
        "C" => [0.0, 1.0, 0.0],
        "LS" => [-1.0, -1.0, 0.0],
        "RS" => [1.0, -1.0, 0.0],
        "LB" => [-1.0, -1.0, 0.0],
        "RB" => [1.0, -1.0, 0.0],
        "LSS" => [-1.0, 0.0, 0.0],
        "RSS" => [1.0, 0.0, 0.0],
        "LRS" => [-1.0, -1.0, 0.0],
        "RRS" => [1.0, -1.0, 0.0],
        "LTS" => [-1.0, 0.0, 1.0],
        "RTS" => [1.0, 0.0, 1.0],
        "TFL" => [-1.0, 1.0, 1.0],
        "TFR" => [1.0, 1.0, 1.0],
        "TRL" => [-1.0, -1.0, 1.0],
        "TRR" => [1.0, -1.0, 1.0],
        "TSL" => [-1.0, -1.0, 1.0],
        "TSR" => [1.0, -1.0, 1.0],
        _ => return None,
    };
    Some(position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("salsa-damf-{}-{nonce}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_caf(path: &Path, channels: usize, frames: usize, samples: &[f32]) {
        let data_len = samples.len() * 3;
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"caff").unwrap();
        file.write_all(&1u16.to_be_bytes()).unwrap();
        file.write_all(&0u16.to_be_bytes()).unwrap();
        file.write_all(b"desc").unwrap();
        file.write_all(&32u64.to_be_bytes()).unwrap();
        file.write_all(&48_000f64.to_be_bytes()).unwrap();
        file.write_all(b"lpcm").unwrap();
        file.write_all(&0u32.to_be_bytes()).unwrap();
        file.write_all(&((channels * 3) as u32).to_be_bytes())
            .unwrap();
        file.write_all(&1u32.to_be_bytes()).unwrap();
        file.write_all(&(channels as u32).to_be_bytes()).unwrap();
        file.write_all(&24u32.to_be_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&((data_len + 4) as i64).to_be_bytes())
            .unwrap();
        file.write_all(&0u32.to_be_bytes()).unwrap();
        assert_eq!(samples.len(), channels * frames);
        for sample in samples {
            let value = (sample * 8_388_608.0).round() as i32;
            file.write_all(&value.to_be_bytes()[1..4]).unwrap();
        }
    }

    fn write_manifest(dir: &Path, bed: &str, objects: &[i64]) -> PathBuf {
        let manifest = dir.join("master.atmos");
        let mut file = std::fs::File::create(&manifest).unwrap();
        write!(
            file,
            "version: 0.5.1\npresentations:\n  - audio: master.atmos.audio\n    metadata: master.atmos.metadata\n    offset: 0.0\n    bedInstances:\n      - channels:\n          - channel: {bed}\n            ID: 3\n    objects:\n"
        )
        .unwrap();
        for id in objects {
            writeln!(file, "      - ID: {id}").unwrap();
        }
        manifest
    }

    #[test]
    fn block_sampling_uses_encoder_timeline() {
        let dir = unique_dir("semantics");
        let manifest = write_manifest(&dir, "LFE", &[10]);
        // LFE + one object, 3 channels, 10 frames.
        let mut samples = Vec::new();
        for frame in 0..10 {
            samples.extend_from_slice(&[0.0, frame as f32 / 10.0]);
        }
        write_caf(&dir.join("master.atmos.audio"), 2, 10, &samples);
        let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
        metadata
            .write_all(
                br#"sampleRate: 48000
events:
  - ID: 3
    samplePos: 0
    gain: 0
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
    gain: 0
  - ID: 10
    samplePos: 4
    pos: [1, 0, 0]
    gain: -6
  - ID: 10
    samplePos: 8
    pos: [0, 1, 0]
"#,
            )
            .unwrap();

        let mut source = DamfSource::open(&manifest).unwrap();
        assert_eq!(source.info().output_channels, 2);
        assert_eq!(source.info().objects[0].position, [-1.0, 0.0, 0.0]);

        let first = source.read_block(4).unwrap().unwrap();
        assert_eq!(first.start_sample, 0);
        assert_eq!(first.frames, 4);
        assert_eq!(first.payload.objects[0].position, [-1.0, 0.0, 0.0]);
        // Ramp from x=-1 at sample 0 to x=1 at sample 4; gain reaches -6 dB at 4.
        assert_eq!(first.payload.events.len(), 1);
        assert_eq!(first.payload.events[0].offset, 0);
        assert_eq!(first.payload.events[0].ramp_samples, 4);
        assert_eq!(first.payload.events[0].state.position, [1.0, 0.0, 0.0]);
        assert!((first.payload.events[0].state.gain - 0.501187).abs() < 1e-5);

        let second = source.read_block(4).unwrap().unwrap();
        assert_eq!(second.start_sample, 4);
        // The keyframe at sample 4 steps to x=1 and -6 dB.
        assert_eq!(second.payload.objects[0].position, [1.0, 0.0, 0.0]);
        assert!((second.payload.objects[0].gain - 0.501187).abs() < 1e-5);
        assert_eq!(second.payload.events.len(), 1);
        assert_eq!(second.payload.events[0].ramp_samples, 4);
        assert_eq!(second.payload.events[0].state.position, [0.0, 1.0, 0.0]);

        let third = source.read_block(4).unwrap().unwrap();
        assert_eq!(third.start_sample, 8);
        assert_eq!(third.frames, 2);
        // The last keyframe holds, so there is no state change in this block.
        assert!(third.payload.events.is_empty());
        assert_eq!(third.payload.objects[0].position, [0.0, 1.0, 0.0]);
        assert!(source.read_block(4).unwrap().is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn seek_restarts_at_the_requested_frame() {
        let dir = unique_dir("seek");
        let manifest = write_manifest(&dir, "LFE", &[10]);
        let mut samples = Vec::new();
        for frame in 0..16 {
            samples.extend_from_slice(&[0.0, frame as f32 / 100.0]);
        }
        write_caf(&dir.join("master.atmos.audio"), 2, 16, &samples);
        let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
        metadata
            .write_all(
                br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
  - ID: 10
    samplePos: 8
    pos: [1, 0, 0]
"#,
            )
            .unwrap();

        let mut source = DamfSource::open(&manifest).unwrap();
        let _ = source.read_block(4).unwrap().unwrap();
        source.seek(8).unwrap();
        let block = source.read_block(4).unwrap().unwrap();
        assert_eq!(block.start_sample, 8);
        assert_eq!(block.frames, 4);
        assert_eq!(block.payload.objects[0].position, [1.0, 0.0, 0.0]);
        assert!((block.pcm[1] - 0.08).abs() < 1e-6);
        assert_eq!(source.next_sample(), 12);

        // Backward seeks re-open the same CAF data window.
        source.seek(4).unwrap();
        let back = source.read_block(4).unwrap().unwrap();
        assert_eq!(back.start_sample, 4);
        assert!((back.pcm[1] - 0.04).abs() < 1e-6);

        source.seek(1000).unwrap();
        assert!(source.read_block(4).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ramp_length_controls_hold_then_ramp() {
        let dir = unique_dir("ramp-length");
        let manifest = write_manifest(&dir, "LFE", &[10]);
        write_caf(&dir.join("master.atmos.audio"), 2, 10, &[0.0; 20]);
        let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
        metadata
            .write_all(
                br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
  - ID: 10
    samplePos: 2
    pos: [1, 0, 0]
    rampLength: 8
"#,
            )
            .unwrap();

        let mut source = DamfSource::open(&manifest).unwrap();
        // [0, 2) holds the previous position; the ramp starts at sample 2.
        let first = source.read_block(2).unwrap().unwrap();
        assert_eq!(first.payload.objects[0].position, [-1.0, 0.0, 0.0]);
        assert!(first.payload.events.is_empty());

        // The 8-sample ramp runs from sample 2 to sample 10. Each 2-sample
        // block targets the encoder timeline's end-of-block position.
        for (index, expected) in [-0.5f32, 0.0, 0.5, 1.0].into_iter().enumerate() {
            let block = source.read_block(2).unwrap().unwrap();
            assert_eq!(block.start_sample, 2 + index as u64 * 2);
            assert_eq!(block.payload.events.len(), 1);
            assert_eq!(block.payload.events[0].ramp_samples, 2);
            assert!((block.payload.events[0].state.position[0] - expected).abs() < 1e-6);
        }
        assert!(source.read_block(2).unwrap().is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expands_non_lfe_beds_into_fixed_objects() {
        let dir = unique_dir("beds");
        let manifest = write_manifest(&dir, "L", &[10]);
        // Manifest bed is L, not LFE, so output is silent LFE + L + object.
        let mut samples = Vec::new();
        for frame in 0..2 {
            samples.extend_from_slice(&[frame as f32 / 10.0, frame as f32 / 20.0]);
        }
        write_caf(&dir.join("master.atmos.audio"), 2, 2, &samples);
        let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
        metadata
            .write_all(
                br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [0, 0, 0]
"#,
            )
            .unwrap();

        let mut source = DamfSource::open(&manifest).unwrap();
        assert_eq!(source.info().lfe_channel, None);
        assert_eq!(source.info().output_channels, 3);
        assert_eq!(source.info().objects[0].name, "L");
        assert_eq!(source.info().objects[0].position, [-1.0, 1.0, 0.0]);
        let block = source.read_block(2).unwrap().unwrap();
        assert_eq!(block.pcm.len(), 6);
        assert_eq!(block.pcm[0], 0.0);
        assert_eq!(block.pcm[1], 0.0);
        assert_eq!(block.pcm[2], 0.0);
        assert_eq!(block.pcm[3], 0.0);
        assert!((block.pcm[4] - 0.1).abs() < 1e-6);
        assert!((block.pcm[5] - 0.05).abs() < 1e-6);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_sample_positions_inherit_the_group() {
        let dir = unique_dir("inherit");
        let manifest = write_manifest(&dir, "LFE", &[10, 11]);
        write_caf(
            &dir.join("master.atmos.audio"),
            3,
            2,
            &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
        metadata
            .write_all(
                br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
  - ID: 11
    pos: [1, 0, 0]
  - ID: 10
    samplePos: 1
    pos: [-1, 1, 0]
"#,
            )
            .unwrap();
        let source = DamfSource::open(&manifest).unwrap();
        assert_eq!(source.info().objects.len(), 2);
        // Object 11's only event is at sample 0 through inheritance, so it is
        // not treated as a missing timeline.
        assert_eq!(source.info().objects[1].position, [1.0, 0.0, 0.0]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keyframes_are_last_wins_at_the_same_sample() {
        let dir = unique_dir("same-sample");
        let manifest = write_manifest(&dir, "LFE", &[10]);
        write_caf(&dir.join("master.atmos.audio"), 2, 1, &[0.0, 0.0]);
        let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
        metadata
            .write_all(
                br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
  - ID: 10
    samplePos: 0
    pos: [1, 0, 0]
"#,
            )
            .unwrap();
        let source = DamfSource::open(&manifest).unwrap();
        assert_eq!(source.info().objects[0].position, [1.0, 0.0, 0.0]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
