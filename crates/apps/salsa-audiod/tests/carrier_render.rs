//! Carrier render comparison: recovered carrier audio must render identically
//! to direct GStreamer input through the same Salsa parser/decoder/renderer.
//!
//! Set `SALSA_TEST_SPDIF_FIXTURES` to `platform/macos/salsa-hal/fixtures`.
//! Binaural checks additionally need `SALSA_ROSELLA_MODEL`. Tests are skipped
//! when the environment is not configured, so the default matrix stays
//! hermetic.

use std::path::PathBuf;

use salsa_audiod::{CarrierSource, Codec, RenderMode, ServiceConfig, compare};

fn fixtures() -> Option<PathBuf> {
    std::env::var_os("SALSA_TEST_SPDIF_FIXTURES").map(PathBuf::from)
}

fn carrier_config(fixture: &str, codec: Codec, render: RenderMode) -> ServiceConfig {
    let root = fixtures().expect("SALSA_TEST_SPDIF_FIXTURES");
    let mut config = ServiceConfig::new(CarrierSource::File(root.join(fixture)));
    config.codec = Some(codec);
    config.render = render;
    config
}

fn model() -> Option<PathBuf> {
    std::env::var_os("SALSA_ROSELLA_MODEL").map(PathBuf::from)
}

#[test]
fn eac3_speaker_matches_direct_rendering() {
    let Some(root) = fixtures() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the E-AC-3 carrier render check");
        return;
    };
    let config = carrier_config("eac3_20s.spdif", Codec::Eac3, RenderMode::speaker("7.1.4"));
    let (report, comparison) =
        compare(config, root.join("eac3_20s.ec3"), 0.0).expect("E-AC-3 comparison");
    assert_eq!(report.transport.carrier_bursts, 625);
    assert_eq!(report.transport.eac3_frames, 625);
    assert_eq!(report.source_frames, 960_000);
    assert!(comparison.passed, "{comparison:?}");
    assert_eq!(comparison.max_abs_diff, 0.0);
}

#[test]
fn truehd_speaker_matches_direct_rendering() {
    let Some(root) = fixtures() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the TrueHD carrier render check");
        return;
    };
    let config = carrier_config(
        "truehd_20s.spdif",
        Codec::TrueHd,
        RenderMode::speaker("7.1.4"),
    );
    let (report, comparison) =
        compare(config, root.join("truehd_20s.mlp"), 0.0).expect("TrueHD comparison");
    assert_eq!(report.transport.carrier_bursts, 999);
    assert_eq!(report.transport.truehd_mat_frames, 999);
    assert_eq!(report.transport.truehd_access_units, 23_976);
    assert!(report.source_frames >= 959_000);
    assert!(comparison.passed, "{comparison:?}");
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "binaural rendering is extremely slow in debug; run this test with --release"
)]
fn eac3_binaural_matches_direct_rendering() {
    let Some(root) = fixtures() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the E-AC-3 binaural check");
        return;
    };
    let Some(model) = model() else {
        eprintln!("SALSA_ROSELLA_MODEL is unset; skipping the E-AC-3 binaural check");
        return;
    };
    let config = carrier_config(
        "eac3_20s.spdif",
        Codec::Eac3,
        RenderMode::Binaural {
            model,
            filterbank: 2,
            profile: "mid".to_string(),
        },
    );
    let (report, comparison) =
        compare(config, root.join("eac3_20s.ec3"), 0.0).expect("E-AC-3 binaural comparison");
    assert_eq!(report.ledger.renderer_signal_samples, 448);
    assert!(comparison.passed, "{comparison:?}");
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "binaural rendering is extremely slow in debug; run this test with --release"
)]
fn truehd_binaural_matches_direct_rendering() {
    let Some(root) = fixtures() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the TrueHD binaural check");
        return;
    };
    let Some(model) = model() else {
        eprintln!("SALSA_ROSELLA_MODEL is unset; skipping the TrueHD binaural check");
        return;
    };
    let config = carrier_config(
        "truehd_20s.spdif",
        Codec::TrueHd,
        RenderMode::Binaural {
            model,
            filterbank: 2,
            profile: "mid".to_string(),
        },
    );
    let (report, comparison) =
        compare(config, root.join("truehd_20s.mlp"), 0.0).expect("TrueHD binaural comparison");
    assert_eq!(report.ledger.renderer_signal_samples, 448);
    assert!(comparison.passed, "{comparison:?}");
}

#[test]
fn binaural_requires_a_model() {
    let Some(root) = fixtures() else {
        eprintln!("SALSA_TEST_SPDIF_FIXTURES is unset; skipping the model validation check");
        return;
    };
    let mut config = ServiceConfig::new(CarrierSource::File(root.join("eac3_20s.spdif")));
    config.render = RenderMode::Binaural {
        model: root.join("does-not-exist.personalized_headphone"),
        filterbank: 1,
        profile: "mid".to_string(),
    };
    let error = salsa_audiod::render(config).expect_err("missing model must fail");
    assert!(error.to_string().contains("does not exist"), "{error}");
}
