//! Offline lifecycle checks.
//!
//! These exercise the playback/recovery behaviour that does not need a wired
//! output: session-generation resets, carrier discontinuity handling,
//! truncation, repeated opens and bounded output. Physical endpoint and A/V
//! timing checks require a separate hardware measurement.
//!
//! Set `SALSA_TEST_SPDIF_FIXTURES` to `platform/macos/salsa-hal/fixtures`.
//! Tests skip when the fixtures are absent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use salsa_audiod::{
    CarrierChunk, CarrierSource, Codec, OutputConfig, RenderMode, ServiceConfig, render,
};

fn fixtures_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SALSA_TEST_SPDIF_FIXTURES") {
        let path = PathBuf::from(path);
        return path.is_dir().then_some(path);
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3)?;
    let path = root.join("platform/macos/salsa-hal/fixtures");
    path.is_dir().then_some(path)
}

fn config(source: CarrierSource) -> ServiceConfig {
    let mut config = ServiceConfig::new(source);
    config.codec = Some(Codec::Eac3);
    config.render = RenderMode::speaker("7.1.4");
    config.output = OutputConfig::Memory;
    config.idle_timeout = Duration::from_secs(10);
    config
}

fn scripted_chunks(
    bytes: &[u8],
    generation: u64,
    start_sample: u64,
    discontinuity: bool,
) -> Vec<CarrierChunk> {
    bytes
        .chunks(64 * 1024)
        .enumerate()
        .map(|(index, chunk)| CarrierChunk {
            bytes: chunk.to_vec(),
            start_sample: Some(start_sample + (index * 64 * 1024 / 4) as u64),
            host_time: Some(0),
            bytes_per_frame: Some(4),
            generation,
            flags: if discontinuity && index == 0 {
                salsa_audiod::FLAG_DISCONTINUITY
            } else {
                0
            },
        })
        .collect()
}

#[test]
fn two_sessions_reset_and_stay_bounded() {
    let Some(root) = fixtures_dir() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the session reset check");
        return;
    };
    let carrier = std::fs::read(root.join("eac3_20s.spdif")).expect("read carrier fixture");

    let mut chunks = scripted_chunks(&carrier, 0, 0, false);
    chunks.extend(scripted_chunks(&carrier, 1, 1_920_000, false));

    let outcome = render(config(CarrierSource::Scripted(chunks))).expect("scripted render");
    assert_eq!(outcome.report.lifecycle.sessions, 2);
    assert_eq!(outcome.report.lifecycle.session_resets, 1);
    assert!(outcome.report.lifecycle.output_discontinuities >= 2);

    let pcm = outcome.pcm.expect("memory output");
    assert!(
        pcm.discontinuities.len() >= 2,
        "expected an initial start and a reset boundary"
    );
    let discont = *pcm
        .discontinuities
        .last()
        .expect("the second session must start a new output timeline");
    let single_session_frames = 960_000u64;
    assert!(
        pcm.frames() >= single_session_frames,
        "the new session must be rendered, got {} frames",
        pcm.frames()
    );
    assert!(
        pcm.frames() <= single_session_frames * 2 + 48_000,
        "bounded output expected, got {} frames",
        pcm.frames()
    );

    // Audio after the reset must be the new session, not the old queue.
    let fresh = render(config(CarrierSource::File(root.join("eac3_20s.spdif"))))
        .expect("fresh session render");
    let fresh = fresh.pcm.expect("fresh memory output");
    let channels = pcm.channels as usize;
    let start = discont as usize * channels;
    let compare = (pcm.samples.len() - start).min(fresh.samples.len());
    assert!(compare > 100_000, "not enough post-reset audio to compare");
    let max_diff = pcm.samples[start..start + compare]
        .iter()
        .zip(&fresh.samples[..compare])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_diff <= 1e-5,
        "post-reset audio differs from a fresh session: max diff {max_diff}"
    );
}

#[test]
fn short_session_reset_keeps_the_new_session() {
    let Some(root) = fixtures_dir() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the short reset check");
        return;
    };
    let carrier = std::fs::read(root.join("eac3_20s.spdif")).expect("read carrier fixture");
    // Two bursts are enough to start decoding but short enough that the reset
    // can arrive while the first session's DISCONT output is still queued.
    let short = &carrier[..2 * 24_576];

    let mut chunks = scripted_chunks(short, 0, 0, false);
    chunks.extend(scripted_chunks(&carrier, 1, 1_920_000, false));

    let outcome = render(config(CarrierSource::Scripted(chunks))).expect("scripted render");
    assert_eq!(outcome.report.lifecycle.sessions, 2);
    assert_eq!(outcome.report.lifecycle.session_resets, 1);

    let pcm = outcome.pcm.expect("memory output");
    let discont = *pcm
        .discontinuities
        .last()
        .expect("the second session must start a new output timeline");
    assert!(pcm.frames() > discont);

    let fresh = render(config(CarrierSource::File(root.join("eac3_20s.spdif"))))
        .expect("fresh session render");
    let fresh = fresh.pcm.expect("fresh memory output");
    let channels = pcm.channels as usize;
    let start = discont as usize * channels;
    let compare = (pcm.samples.len() - start).min(fresh.samples.len());
    assert!(compare > 100_000, "not enough post-reset audio to compare");
    let max_diff = pcm.samples[start..start + compare]
        .iter()
        .zip(&fresh.samples[..compare])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_diff <= 1e-5,
        "post-reset audio differs from a fresh session: max diff {max_diff}"
    );
}

#[test]
fn truncated_carrier_fails_explicitly() {
    if fixtures_dir().is_none() {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the truncated carrier check");
        return;
    }
    let path = std::env::temp_dir().join(format!(
        "salsa-audiod-truncated-{}.spdif",
        std::process::id()
    ));
    std::fs::write(&path, [0u8; 64]).unwrap();
    let error = render(config(CarrierSource::File(path.clone())))
        .expect_err("truncated carrier must fail explicitly");
    let _ = std::fs::remove_file(path);
    assert!(
        error
            .to_string()
            .contains("carrier ended before a supported"),
        "{error}"
    );
}

#[test]
fn output_open_failure_returns_without_waiting_for_pipeline_eos() {
    let Some(root) = fixtures_dir() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the output error check");
        return;
    };
    let mut config = config(CarrierSource::File(root.join("eac3_20s.spdif")));
    config.output = OutputConfig::Wav(std::env::temp_dir().join(format!(
        "salsa-audiod-missing-parent-{}/output.wav",
        std::process::id()
    )));
    config.idle_timeout = Duration::from_millis(500);
    let started = std::time::Instant::now();
    let error = render(config).expect_err("missing output parent must fail");
    assert!(error.to_string().contains("cannot create"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn repeated_render_is_deterministic() {
    let Some(root) = fixtures_dir() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the repeat-open check");
        return;
    };
    let path = root.join("eac3_20s.spdif");
    let first = render(config(CarrierSource::File(path.clone()))).expect("first render");
    let second = render(config(CarrierSource::File(path))).expect("second render");
    let first_pcm = first.pcm.expect("first memory output");
    let second_pcm = second.pcm.expect("second memory output");
    assert_eq!(first.report.lifecycle.sessions, 1);
    assert_eq!(second.report.lifecycle.sessions, 1);
    assert_eq!(first_pcm.rate, second_pcm.rate);
    assert_eq!(first_pcm.channels, second_pcm.channels);
    assert_eq!(first_pcm.samples, second_pcm.samples);
    assert_eq!(first_pcm.discontinuities, second_pcm.discontinuities);
}
