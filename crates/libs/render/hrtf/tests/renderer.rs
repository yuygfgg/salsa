//! Opt-in model/renderer smoke test.
//!
//! Set `SALSA_ROSELLA_MODEL` to a `.personalized_headphone` file. The default
//! test matrix stays independent of any Dolby sample data.

use objectaudio_hrtf::{
    FRAME_SAMPLES, FilterbankKind, RosellaRenderOptions, RosellaRenderer,
    load_personalized_headphone,
};

#[test]
fn renders_a_silent_frame_with_both_banks() {
    let Some(path) = std::env::var_os("SALSA_ROSELLA_MODEL") else {
        eprintln!("SALSA_ROSELLA_MODEL is unset; skipping the Rosella renderer test");
        return;
    };
    for filterbank in [FilterbankKind::Public, FilterbankKind::Captured] {
        let model = load_personalized_headphone(&path).expect("model");
        let options = RosellaRenderOptions {
            filterbank,
            chunk_frames: 1,
            tail_seconds: 0.0,
            skip_latency: true,
            ..RosellaRenderOptions::default()
        };
        let mut renderer = RosellaRenderer::new(model, options, 16).expect("renderer");
        let frame = vec![0.0f32; FRAME_SAMPLES * 16];
        renderer.submit_frame(&frame, None).expect("submit");
        let output = renderer.take_output();
        assert_eq!(output.len(), 2 * (FRAME_SAMPLES - filterbank.latency()));
        assert!(output.iter().all(|value| value.is_finite()));
        assert_eq!(renderer.latency_samples(), filterbank.latency());
        renderer.reset().expect("reset");
    }
}

#[test]
fn renders_dynamic_source_counts() {
    let Some(path) = std::env::var_os("SALSA_ROSELLA_MODEL") else {
        eprintln!("SALSA_ROSELLA_MODEL is unset; skipping the Rosella renderer test");
        return;
    };
    for source_channels in [1usize, 16, 128, 256] {
        let model = load_personalized_headphone(&path).expect("model");
        let options = RosellaRenderOptions {
            filterbank: FilterbankKind::Public,
            chunk_frames: 1,
            tail_seconds: 0.0,
            skip_latency: true,
            ..RosellaRenderOptions::default()
        };
        let mut renderer = RosellaRenderer::new(model, options, source_channels).expect("renderer");
        assert_eq!(renderer.source_channels(), source_channels);
        assert_eq!(renderer.object_count(), source_channels - 1);
        let frame = vec![0.0f32; FRAME_SAMPLES * source_channels];
        renderer.submit_frame(&frame, None).expect("submit");
        let output = renderer.take_output();
        assert!(output.iter().all(|value| value.is_finite()));
    }
}
