//! Opt-in real-stream smoke tests for the playback shell and visual tap.
//!
//! Run with the Salsa plugins on `GST_PLUGIN_PATH`, a real stream, and the
//! headless sink override:
//!
//! ```sh
//! SALSA_TEST_EC3=stream.ec3 SALSA_SINK_OVERRIDE=fakesink cargo test
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use salsa_gui::pipeline::{OutputConfig, OutputMode};
use salsa_gui::player::Player;
use salsa_gui::visual::VisualTap;

fn run(variable: &str) {
    let Ok(path) = std::env::var(variable) else {
        eprintln!("{variable} is unset; skipping");
        return;
    };
    if std::env::var("SALSA_SINK_OVERRIDE").as_deref() != Ok("fakesink") {
        eprintln!("set SALSA_SINK_OVERRIDE=fakesink to run the headless playback test");
        return;
    }
    gst::init().expect("gst init");
    let config = OutputConfig {
        mode: OutputMode::Speaker,
        layout: "7.1.4".to_string(),
        ..Default::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let player = Player::open(
        &PathBuf::from(&path),
        &config,
        &[],
        gst::ClockTime::ZERO,
        true,
        &cancel,
        true,
    )
    .expect("open stream");

    let caps = player
        .decoder()
        .static_pad("src")
        .and_then(|pad| pad.current_caps())
        .expect("decoder caps");
    let channels = caps
        .structure(0)
        .and_then(|structure| structure.get::<i32>("channels").ok())
        .expect("channel count");
    assert!(channels > 1, "stream must carry objects");

    let tap = VisualTap::new();
    tap.attach_decoder(player.decoder()).expect("attach tap");
    let mut snapshot = None;
    for _ in 0..120 {
        std::thread::sleep(Duration::from_millis(25));
        snapshot = tap.snapshot_at(player.position());
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.objects.is_empty())
        {
            break;
        }
    }
    let snapshot = snapshot.expect("object snapshot");
    assert!(!snapshot.objects.is_empty());
    assert_eq!(snapshot.objects.len() as i32, channels - 1);
    player.close();
}

#[test]
fn ec3_playback_reaches_the_visual_tap() {
    run("SALSA_TEST_EC3");
}

#[test]
fn truehd_playback_reaches_the_visual_tap() {
    run("SALSA_TEST_TRUEHD");
}

#[test]
fn binaural_playback_reaches_the_visual_tap() {
    let Ok(model) = std::env::var("SALSA_TEST_MODEL") else {
        eprintln!("SALSA_TEST_MODEL is unset; skipping");
        return;
    };
    let Ok(path) = std::env::var("SALSA_TEST_EC3") else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping");
        return;
    };
    if std::env::var("SALSA_SINK_OVERRIDE").as_deref() != Ok("fakesink") {
        eprintln!("set SALSA_SINK_OVERRIDE=fakesink to run the headless playback test");
        return;
    }
    gst::init().expect("gst init");
    let mut config = OutputConfig {
        mode: OutputMode::Binaural,
        layout: "7.1.4".to_string(),
        ..Default::default()
    };
    config.binaural.model = Some(PathBuf::from(model));
    let cancel = Arc::new(AtomicBool::new(false));
    let player = Player::open(
        &PathBuf::from(&path),
        &config,
        &[],
        gst::ClockTime::ZERO,
        true,
        &cancel,
        true,
    )
    .expect("open binaural stream");
    let tap = VisualTap::new();
    tap.attach_decoder(player.decoder()).expect("attach tap");
    let mut snapshot = None;
    for _ in 0..120 {
        std::thread::sleep(Duration::from_millis(25));
        snapshot = tap.snapshot_at(player.position());
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.objects.is_empty())
        {
            break;
        }
    }
    let snapshot = snapshot.expect("object snapshot");
    assert!(!snapshot.objects.is_empty());
    player.close();
}

#[test]
fn flush_seek_moves_forward_and_backward() {
    let Ok(path) = std::env::var("SALSA_TEST_EC3") else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping");
        return;
    };
    if std::env::var("SALSA_SINK_OVERRIDE").as_deref() != Ok("fakesink")
        || std::env::var("SALSA_SINK_SYNC").as_deref() != Ok("true")
    {
        eprintln!("set SALSA_SINK_OVERRIDE=fakesink SALSA_SINK_SYNC=true to run the seek test");
        return;
    }
    gst::init().expect("gst init");
    let config = OutputConfig {
        mode: OutputMode::Speaker,
        layout: "7.1.4".to_string(),
        ..Default::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let player = Player::open(
        &PathBuf::from(&path),
        &config,
        &[],
        gst::ClockTime::ZERO,
        false,
        &cancel,
        true,
    )
    .expect("open stream");
    if player
        .duration()
        .is_some_and(|duration| duration < gst::ClockTime::from_seconds(3))
    {
        eprintln!("stream is too short for the seek test; skipping");
        player.close();
        return;
    }

    player
        .seek(gst::ClockTime::from_seconds(2))
        .expect("forward seek");
    player.set_playing().expect("play");
    std::thread::sleep(Duration::from_millis(350));
    let forward = player.position();
    assert!(
        forward >= gst::ClockTime::from_mseconds(1500),
        "forward seek landed at {forward}"
    );

    player
        .seek(gst::ClockTime::from_mseconds(500))
        .expect("backward seek");
    std::thread::sleep(Duration::from_millis(350));
    let backward = player.position();
    assert!(
        backward < gst::ClockTime::from_mseconds(1500),
        "backward seek landed at {backward}"
    );
    player.close();
}

#[test]
fn device_enumeration_is_safe() {
    gst::init().expect("gst init");
    let devices = salsa_gui::pipeline::enumerate_devices();
    eprintln!("{} audio sink devices", devices.len());
    for device in &devices {
        eprintln!(
            "  {} ({} ch){}",
            device.display_name,
            device
                .max_channels
                .map(|channels| channels.to_string())
                .unwrap_or_else(|| "?".to_string()),
            if device.is_default { " [default]" } else { "" }
        );
    }
}

#[test]
fn non_atmos_stream_is_rejected() {
    let Ok(path) = std::env::var("SALSA_TEST_NONATMOS") else {
        eprintln!("SALSA_TEST_NONATMOS is unset; skipping");
        return;
    };
    if std::env::var("SALSA_SINK_OVERRIDE").as_deref() != Ok("fakesink") {
        eprintln!("set SALSA_SINK_OVERRIDE=fakesink to run the headless playback test");
        return;
    }
    gst::init().expect("gst init");
    let config = OutputConfig {
        mode: OutputMode::Speaker,
        layout: "7.1.4".to_string(),
        ..Default::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let result = Player::open(
        &PathBuf::from(&path),
        &config,
        &[],
        gst::ClockTime::ZERO,
        false,
        &cancel,
        true,
    );
    match result {
        Err(salsa_gui::player::PlayerError::NoObjectAudio) => {}
        Err(error) => panic!("expected NoObjectAudio, got {error}"),
        Ok(player) => {
            player.close();
            panic!("expected NoObjectAudio, but the stream opened");
        }
    }
}

#[test]
fn duration_scan_in_a_worker_is_safe() {
    let Ok(path) = std::env::var("SALSA_TEST_EC3") else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping");
        return;
    };
    gst::init().expect("gst init");
    let path = PathBuf::from(path);
    let codec = salsa_gui::pipeline::detect_codec(&path).expect("codec");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let cancel = Arc::new(AtomicBool::new(false));
        let duration = salsa_gui::pipeline::scan_duration(&path, codec, &cancel);
        let _ = sender.send(duration);
    });
    let duration = receiver
        .recv_timeout(Duration::from_secs(180))
        .expect("duration worker finished");
    eprintln!("scanned duration: {duration:?}");
}
