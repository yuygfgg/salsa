//! Progress and deterministic output across encoder worker counts.

use objectaudio_damf::{BedEntry, CafWriter, ManifestSpec, Metadata, write_manifest};
use objectaudio_ir::SpatialParser;
use salsa_spatial::{SAMPLE_RATE, SalsaSpatialParser, TILE_SAMPLES};
use salsa_spatial_encoder::{EncodeOptions, EncodeProgress, EncodeStage, encode_with_progress};

#[test]
fn progress_tracks_selected_samples_and_threads_preserve_the_stream() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let master = path.join("master.atmos");
    let total_frames = 2 * TILE_SAMPLES;
    let beds: Vec<_> = ["LFE", "Ltf", "Rtr"]
        .into_iter()
        .enumerate()
        .map(|(id, name)| BedEntry {
            id: id as i64,
            name: name.into(),
        })
        .collect();
    write_manifest(
        &master,
        &ManifestSpec {
            audio_name: "master.atmos.audio",
            metadata_name: "master.atmos.metadata",
            offset: 0.0,
            fps: 24.0,
            sc_bed_configuration: &[],
            creation_tool: "salsa-spatial-test",
            creation_tool_version: "1",
            beds: &beds,
            objects: &[],
        },
    )
    .unwrap();
    Metadata {
        sample_rate: SAMPLE_RATE,
        events: Vec::new(),
    }
    .save(path.join("master.atmos.metadata"))
    .unwrap();
    let mut audio = CafWriter::create(
        path.join("master.atmos.audio"),
        SAMPLE_RATE,
        3,
        total_frames as u64,
        true,
    )
    .unwrap();
    let samples: Vec<f32> = (0..total_frames)
        .flat_map(|sample| {
            let t = sample as f32;
            [
                0.01 * (t * 0.007).sin(),
                0.1 * (t * 0.059).sin(),
                0.1 * (t * 0.093).cos(),
            ]
        })
        .collect();
    audio.write_frames(&samples).unwrap();
    audio.finish().unwrap();

    let duration = TILE_SAMPLES + 137;
    let mut options = EncodeOptions {
        master,
        output: path.join("single.ogg"),
        core_bitrates: [160_000, 128_000, 160_000, 112_000, 24_000],
        beta: 1e-6,
        max_frames: Some(duration),
        reference_start: 91,
        headroom_db: None,
        ceiling_horizontal_gain: 0.0,
    };
    let mut updates = Vec::new();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let report = pool
        .install(|| encode_with_progress(&options, |value| updates.push(value)))
        .unwrap();
    let expected = |stage| {
        [0, TILE_SAMPLES as u64, duration as u64].map(|completed_samples| EncodeProgress {
            stage,
            completed_samples,
            total_samples: duration as u64,
        })
    };
    let mut expected_updates = expected(EncodeStage::Headroom).to_vec();
    expected_updates.extend(expected(EncodeStage::Encoding));
    assert_eq!(updates, expected_updates);
    assert_eq!(report.duration_samples, duration as u64);
    let serial = std::fs::read(&options.output).unwrap();
    assert_eq!(report.total_bytes, serial.len() as u64);

    options.output = path.join("parallel.ogg");
    updates.clear();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    pool.install(|| encode_with_progress(&options, |value| updates.push(value)))
        .unwrap();
    assert_eq!(updates, expected_updates);
    assert_eq!(serial, std::fs::read(&options.output).unwrap());

    options.output = path.join("fixed-headroom.ogg");
    options.headroom_db = Some(0.0);
    updates.clear();
    pool.install(|| encode_with_progress(&options, |value| updates.push(value)))
        .unwrap();
    assert_eq!(updates, expected(EncodeStage::Encoding));
    assert_eq!(serial, std::fs::read(&options.output).unwrap());

    let mut parser = SalsaSpatialParser::new();
    let mut frames = parser.push_bytes(&serial).unwrap();
    frames.extend(parser.flush().unwrap());
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].start_sample, options.reference_start);
    assert_eq!(frames[1].frames, 137);
    assert_eq!(
        frames.iter().map(|frame| frame.frames).sum::<usize>(),
        duration
    );

    use objectaudio_ir::SpatialDecoder;
    let config = parser.config().cloned().expect("stream configuration");
    assert_eq!(config.format_version, 1);
    assert_eq!(
        config.core_panner,
        salsa_spatial::CorePannerConfig::default()
    );
    let mut decoder = salsa_spatial_dsp::SalsaSpatialDecoder::new(config).unwrap();
    let mut decoded = 0usize;
    for frame in &frames {
        decoded += decoder.decode(frame).unwrap().frames;
    }
    if let Some(tail) = decoder.finish().unwrap() {
        decoded += tail.frames;
    }
    assert!(
        decoded + 120 >= duration as usize,
        "decoded {decoded} samples of {duration}"
    );

    options.ceiling_horizontal_gain = 0.25;
    options.headroom_db = None;
    options.output = path.join("compressed.ogg");
    let report = pool
        .install(|| encode_with_progress(&options, |_| {}))
        .unwrap();
    assert_eq!(report.ceiling_horizontal_gain, 0.25);
    let mut parser = SalsaSpatialParser::new();
    let mut frames = parser
        .push_bytes(&std::fs::read(&options.output).unwrap())
        .unwrap();
    frames.extend(parser.flush().unwrap());
    let config = parser.config().unwrap().clone();
    assert_eq!(config.format_version, 2);
    assert_eq!(config.signal_models[0].algorithm_version, 2);
    assert_eq!(config.core_panner.ceiling_horizontal_gain(), 0.25);
    assert!(
        config
            .initial_objects
            .iter()
            .all(|object| object.position[2] == 1.0)
    );
    assert!(config.enhanced_supported());
    let mut decoder = salsa_spatial_dsp::SalsaSpatialDecoder::new(config).unwrap();
    let mut decoded = 0;
    for frame in &frames {
        let output = decoder.decode(frame).unwrap();
        decoded += output.frames;
    }
    if let Some(tail) = decoder.finish().unwrap() {
        decoded += tail.frames;
    }
    assert!(decoded + 120 >= duration);
}
