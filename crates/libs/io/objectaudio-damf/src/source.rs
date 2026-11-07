//! DAMF PCM and resolved scene segments in salsa channel order.

use crate::caf::CafReader;
use crate::error::{DamfError, Result};
use crate::manifest::Manifest;
use crate::metadata::Metadata;
use crate::scene::{Consumer, DamfScene, ObjectTrack};
use objectaudio_ir::{CalibrationEvent, ObjectState, ProgramCalibration, SpatialPayload};
use std::collections::BTreeSet;
use std::path::Path;

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
    tracks: Vec<ObjectTrack>,
    next_sample: u64,
    lfe_track: Option<ObjectTrack>,
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

        let scene = DamfScene::resolve(&manifest, &metadata)?;
        warnings.extend(scene.warnings.iter().cloned());
        warnings.extend(scene.report(Consumer::Render).approximations);
        let lfe_channel = scene.lfe.as_ref().map(|object| object.channel);
        let lfe_track = scene.lfe.map(|object| object.track);
        let objects = scene
            .objects
            .iter()
            .enumerate()
            .map(|(index, object)| DamfObjectInfo {
                id: index as u32 + 1,
                damf_id: object.obj_id,
                source_channel: object.channel,
                name: object.name.clone(),
                fixed: object.is_fixed(),
                position: object.track.position_at(0),
            })
            .collect::<Vec<_>>();
        let tracks = scene
            .objects
            .into_iter()
            .map(|object| object.track)
            .collect();

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
            lfe_track,
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

        let mut pcm = vec![0.0f32; frames * self.info.output_channels];
        for frame in 0..frames {
            let source_base = frame * self.info.source_channels;
            let output_base = frame * self.info.output_channels;
            if let Some(lfe) = self.info.lfe_channel {
                pcm[output_base] = raw[source_base + lfe]
                    * self
                        .lfe_track
                        .as_ref()
                        .map_or(1.0, |track| track.effective_gain_at(start + frame as u64));
            }
            for (index, object) in self.info.objects.iter().enumerate() {
                pcm[output_base + 1 + index] = raw[source_base + object.source_channel];
            }
        }

        let objects: Vec<ObjectState> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| ObjectState {
                id: index as u32 + 1,
                ..track.state_at(start)
            })
            .collect();
        let mut events = self
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(index, track)| track.events(start, frames, index as u32 + 1))
            .collect::<Vec<_>>();
        events.sort_by_key(|event| event.offset);
        // TODO: Read the manifest's warpMode into programme calibration.
        // The source currently keeps per-object trimBypass but leaves the
        // programme controls at their defaults.
        let calibration_at = |sample| ProgramCalibration {
            bypass: self
                .tracks
                .iter()
                .map(|track| track.metadata_at(sample).trim_bypass)
                .collect(),
            ..ProgramCalibration::default()
        };
        let mut payload = SpatialPayload::new(self.info.sample_rate, objects, events)
            .with_calibration(calibration_at(start));
        let changes: BTreeSet<_> = self
            .tracks
            .iter()
            .flat_map(|track| track.bypass_changes(start, start + frames as u64))
            .map(|(sample, _)| sample)
            .collect();
        payload.calibration_events = changes
            .into_iter()
            .map(|sample| CalibrationEvent {
                offset: (sample - start) as i64,
                calibration: calibration_at(sample),
            })
            .collect();
        self.next_sample = start + frames as u64;

        Ok(Some(DamfBlock {
            sample_rate: self.info.sample_rate,
            start_sample: start,
            frames,
            pcm,
            payload,
        }))
    }
}

/// ADM normalized position for a standard DAMF bed channel name.
pub fn bed_position(name: &str) -> Option<[f32; 3]> {
    objectaudio_ir::SpeakerPosition::from_bed_name(name).map(|channel| channel.adm_position())
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
    fn block_reads_preserve_steps_at_event_times() {
        let dir = unique_dir("semantics");
        let manifest = write_manifest(&dir, "LFE", &[10]);
        // LFE + one object, 2 channels, 10 frames.
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
        assert!(first.payload.events.is_empty());

        let second = source.read_block(4).unwrap().unwrap();
        assert_eq!(second.start_sample, 4);
        // The keyframe at sample 4 steps to x=1 and -6 dB.
        assert_eq!(second.payload.objects[0].position, [1.0, 0.0, 0.0]);
        assert!((second.payload.objects[0].gain - 0.501187).abs() < 1e-5);
        assert!(second.payload.events.is_empty());

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
    fn block_carries_the_per_object_trim_bypass() {
        let dir = unique_dir("trim-bypass");
        let manifest = write_manifest(&dir, "LFE", &[10]);
        write_caf(&dir.join("master.atmos.audio"), 2, 4, &[0.0; 8]);
        let mut metadata = std::fs::File::create(dir.join("master.atmos.metadata")).unwrap();
        metadata
            .write_all(
                br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
    trimBypass: true
"#,
            )
            .unwrap();

        let mut source = DamfSource::open(&manifest).unwrap();
        let block = source.read_block(4).unwrap().unwrap();
        let calibration = block.payload.calibration.expect("calibration");
        assert_eq!(calibration.bypass, vec![true]);
        assert_eq!(calibration.warp_mode, 0);
        assert_eq!(calibration.global_trim_mode, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn block_and_seek_keep_multiple_bypass_changes() {
        let dir = unique_dir("bypass-changes");
        let manifest = write_manifest(&dir, "Ls", &[10]);
        write_caf(&dir.join("master.atmos.audio"), 2, 160, &vec![0.0; 320]);
        std::fs::write(
            dir.join("master.atmos.metadata"),
            br#"sampleRate: 48000
events:
  - ID: 3
    samplePos: 0
    trimBypass: false
  - ID: 10
    samplePos: 0
    pos: [-1, 0, 0]
    trimBypass: false
  - ID: 10
    samplePos: 32
    pos: [1, 0, 0]
    rampLength: 96
  - ID: 3
    samplePos: 64
    trimBypass: true
  - ID: 10
    samplePos: 64
    trimBypass: true
  - ID: 10
    samplePos: 96
    trimBypass: false
  - ID: 3
    samplePos: 128
    trimBypass: false
"#,
        )
        .unwrap();
        let mut source = DamfSource::open(&manifest).unwrap();
        let block = source.read_block(128).unwrap().unwrap();
        assert_eq!(block.frames, 128);
        assert_eq!(
            block
                .payload
                .calibration_events
                .iter()
                .map(|event| event.offset)
                .collect::<Vec<_>>(),
            [64, 96]
        );
        let reference = objectaudio_ir::SceneTimeline::from_payload(&block.payload, 0).unwrap();
        for sample in 0..128 {
            assert_eq!(
                reference.calibration_at(sample).unwrap().bypass,
                [sample >= 64, (64..96).contains(&sample)]
            );
            let expected_x = -1.0 + 2.0 * ((sample - 32) as f32 / 96.0).clamp(0.0, 1.0);
            assert!((reference.state_at(1, sample).position[0] - expected_x).abs() < 1e-6);
        }
        for start in [0, 32, 64, 80, 96] {
            source.seek(start).unwrap();
            let block = source.read_block((128 - start) as usize).unwrap().unwrap();
            let actual =
                objectaudio_ir::SceneTimeline::from_payload(&block.payload, start as i64).unwrap();
            for sample in start as i64..128 {
                assert_eq!(
                    actual.calibration_at(sample),
                    reference.calibration_at(sample)
                );
            }
        }
        let last = source.read_block(32).unwrap().unwrap();
        assert_eq!(last.payload.calibration.unwrap().bypass, [false, false]);
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
        // block keeps the original target and the remaining ramp duration.
        for (index, expected) in [-0.5f32, 0.0, 0.5, 1.0].into_iter().enumerate() {
            let block = source.read_block(2).unwrap().unwrap();
            assert_eq!(block.start_sample, 2 + index as u64 * 2);
            assert_eq!(block.payload.events.len(), 1);
            assert_eq!(
                block.payload.events[0].transition.samples(),
                8 - index as u32 * 2
            );
            let scene = objectaudio_ir::SceneTimeline::from_payload(
                &block.payload,
                block.start_sample as i64,
            )
            .unwrap();
            assert!(
                (scene.state_at(0, block.start_sample as i64 + 2).position[0] - expected).abs()
                    < 1e-5
            );
        }
        assert!(source.read_block(2).unwrap().is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonical_beds_preserve_pcm_identity_after_lfe_reordering() {
        let dir = unique_dir("canonical-beds");
        let names = [
            "L", "R", "C", "LFE", "Lss", "Rss", "Lrs", "Rrs", "Lfh", "Rfh", "Lts", "Rts", "Lrh",
            "Rrh", "Lw", "Rw",
        ];
        let path = dir.join("master.atmos");
        let beds: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(id, name)| crate::BedEntry {
                id: id as i64,
                name: (*name).into(),
            })
            .collect();
        crate::write_manifest(
            &path,
            &crate::ManifestSpec {
                audio_name: "master.atmos.audio",
                metadata_name: "master.atmos.metadata",
                offset: 0.0,
                fps: 24.0,
                sc_bed_configuration: &[],
                creation_tool: "channel-test",
                creation_tool_version: "1",
                beds: &beds,
                objects: &[],
            },
        )
        .unwrap();
        Metadata {
            sample_rate: 48_000,
            events: vec![],
        }
        .save(dir.join("master.atmos.metadata"))
        .unwrap();
        let input: Vec<_> = (1..=16).map(|channel| channel as f32 / 32.0).collect();
        write_caf(&dir.join("master.atmos.audio"), 16, 1, &input);
        let mut source = DamfSource::open(&path).unwrap();
        let block = source.read_block(1).unwrap().unwrap();
        assert_eq!(block.pcm.len(), 16);
        assert_eq!(block.payload.objects.len(), 15);
        assert_eq!(block.pcm[0], input[3]);
        for (object, source_channel) in block
            .payload
            .objects
            .iter()
            .zip((0..16).filter(|index| *index != 3))
        {
            assert_eq!(
                object.position,
                bed_position(names[source_channel]).unwrap()
            );
            assert_eq!(block.pcm[object.id as usize], input[source_channel]);
        }
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
