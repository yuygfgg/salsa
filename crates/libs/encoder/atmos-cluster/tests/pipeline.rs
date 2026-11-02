// SPDX-License-Identifier: MPL-2.0

use std::path::PathBuf;

use atmos_cluster::{ClusterSettings, run_cluster};
use objectaudio_damf::{
    BedEntry, CafReader, CafWriter, DamfEvent, Gain, Manifest, ManifestSpec, Metadata, ObjectEntry,
    write_manifest,
};

fn unique_dir() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "salsa-atmos-cluster-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn event(id: i64, x: f32) -> DamfEvent {
    DamfEvent {
        id,
        sample_pos: 0,
        active: Some(true),
        position: Some([x, 0.0, 0.0]),
        snap: Some(false),
        elevation: Some(true),
        zones: Some("all".to_string()),
        size: Some(0.0),
        decorr: Some(0),
        importance: Some(1.0),
        gain: Some(Gain::Db(0.0)),
        ramp_length: Some(1536),
        trim_bypass: Some(false),
        dialog: Some(-1.0),
        music: Some(-1.0),
        screen_factor: Some(0.0),
        depth_factor: Some(0.25),
        head_track_mode: Some("undefined".to_string()),
        binaural_render_mode: Some("undefined".to_string()),
    }
}

#[test]
fn end_to_end_writes_a_clustered_damf() {
    let dir = unique_dir();
    let manifest = dir.join("source.atmos");
    let audio = dir.join("source.atmos.audio");
    let metadata = dir.join("source.atmos.metadata");
    let beds = [BedEntry {
        id: 3,
        name: "LFE".to_string(),
    }];
    let objects = [
        ObjectEntry {
            id: 10,
            description: None,
        },
        ObjectEntry {
            id: 11,
            description: None,
        },
    ];
    write_manifest(
        &manifest,
        &ManifestSpec {
            audio_name: "source.atmos.audio",
            metadata_name: "source.atmos.metadata",
            offset: 0.0,
            fps: 24.0,
            sc_bed_configuration: &[3],
            creation_tool: "salsa-cluster-test",
            creation_tool_version: "0.1.0",
            beds: &beds,
            objects: &objects,
        },
    )
    .unwrap();

    let frames = 12_000usize;
    let channels = 3usize;
    let mut writer = CafWriter::create(&audio, 48_000, channels, frames as u64, true).unwrap();
    let mut samples = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        let left = (2.0 * std::f32::consts::PI * 440.0 * frame as f32 / 48_000.0).sin() * 0.1;
        let right = (2.0 * std::f32::consts::PI * 660.0 * frame as f32 / 48_000.0).sin() * 0.05;
        samples.extend_from_slice(&[0.0, left, right]);
    }
    writer.write_frames(&samples).unwrap();
    writer.finish().unwrap();

    Metadata {
        sample_rate: 48_000,
        events: vec![event(10, -1.0), event(11, 1.0)],
    }
    .save(&metadata)
    .unwrap();

    let settings = ClusterSettings {
        max_objects: 1,
        hop: 512,
        use_vad: false,
        ..ClusterSettings::default()
    };
    let result = run_cluster(&manifest, &dir, &settings, Some("clustered"), 0).unwrap();
    assert_eq!(result.clusters, 1);
    assert_eq!(result.total_frames, frames as u64);

    let output_manifest = Manifest::load(&result.manifest).unwrap();
    assert_eq!(output_manifest.objects.len(), 1);
    assert_eq!(output_manifest.beds.len(), 1);
    assert_eq!(output_manifest.beds[0].name, "LFE");

    let mut reader = CafReader::open(&result.audio).unwrap();
    assert_eq!(reader.info().channels, 2);
    assert_eq!(reader.total_frames(), Some(frames as u64));
    reader.seek_to_frame(0).unwrap();
    let decoded = reader.read_frames(frames).unwrap();
    assert_eq!(decoded.len(), frames * 2);
    assert!(decoded.iter().all(|value| value.is_finite()));
    assert!(decoded.iter().any(|value| value.abs() > 1e-4));

    let output_metadata = Metadata::load(&result.metadata).unwrap();
    assert!(output_metadata.events.iter().any(|event| event.id == 10));
    assert_eq!(output_metadata.sample_rate, 48_000);

    std::fs::remove_dir_all(dir).unwrap();
}
