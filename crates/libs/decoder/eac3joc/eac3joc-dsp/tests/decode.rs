//! Real-stream decode checks. Set `SALSA_TEST_EC3` to run them locally;
//! `SALSA_REFERENCE_F32` optionally compares the first frames against a raw
//! interleaved float32 reference dump.

use eac3joc::Eac3JocParser;
use eac3joc_dsp::Eac3JocDecoder;
use objectaudio_ir::SpatialDecoder;

fn stream_bytes() -> Option<Vec<u8>> {
    let path = std::env::var_os("SALSA_TEST_EC3")?;
    Some(std::fs::read(path).expect("read SALSA_TEST_EC3"))
}

#[test]
fn decodes_object_frames() {
    let Some(bytes) = stream_bytes() else {
        eprintln!("SALSA_TEST_EC3 is unset; skipping the real-stream decode test");
        return;
    };
    let mut parser = Eac3JocParser::new();
    let mut frames = parser.push_bytes(&bytes).expect("parse");
    frames.extend(parser.flush().expect("flush"));
    assert!(frames.len() > 10);
    let mut decoder = Eac3JocDecoder::try_new().expect("decoder");
    let mut first = Vec::new();
    for frame in frames.iter().take(10) {
        let spatial = SpatialDecoder::decode(&mut decoder, frame).expect("decode");
        let channel_count = spatial.pcm.channel_count();
        assert!((1..=16).contains(&channel_count));
        assert_eq!(spatial.objects.len() + 1, channel_count);
        assert_eq!(spatial.frames, 1536);
        assert_eq!(spatial.sample_rate, 48_000);
        for channel in 0..channel_count {
            let plane = spatial.pcm.channel(channel).expect("channel");
            assert!(plane.iter().all(|sample| sample.is_finite()));
            first.extend_from_slice(plane);
        }
    }
    assert!(first.iter().any(|sample| *sample != 0.0));
}

#[test]
#[allow(clippy::chunks_exact_to_as_chunks)]
fn matches_reference_when_provided() {
    let Some(bytes) = stream_bytes() else {
        return;
    };
    let Ok(reference_path) = std::env::var("SALSA_REFERENCE_F32") else {
        return;
    };
    let reference = std::fs::read(reference_path).expect("read reference");
    let mut parser = Eac3JocParser::new();
    let mut frames = parser.push_bytes(&bytes).expect("parse");
    frames.extend(parser.flush().expect("flush"));
    let mut decoder = Eac3JocDecoder::try_new().expect("decoder");
    let mut ours = Vec::new();
    for frame in frames.iter().take(10) {
        let spatial = SpatialDecoder::decode(&mut decoder, frame).expect("decode");
        ours.extend(spatial.pcm.interleave());
    }
    let expected: Vec<f32> = reference
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .take(ours.len())
        .collect();
    assert_eq!(
        expected.len(),
        ours.len(),
        "reference is shorter than 10 frames"
    );
    let max = ours
        .iter()
        .zip(&expected)
        .map(|(ours, expected)| (ours - expected).abs())
        .fold(0.0f32, f32::max);
    assert!(max < 1e-6, "max abs error {max}");
}
