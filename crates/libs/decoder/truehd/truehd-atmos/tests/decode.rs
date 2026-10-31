//! Opt-in real-stream parse/decode check.
//!
//! Set `SALSA_TEST_TRUEHD` to a `.mlp`/`.thd` file. The test is skipped when the
//! variable is unset so the default `cargo test` stays hermetic.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use objectaudio_ir::SpatialParser;
use truehd_atmos::{
    OamdPayload, TrueHdDecoder, TrueHdFrame, TrueHdParser, oamd_payloads,
    truehd::process::extract::Extractor, truehd::process::parse::Parser,
    truehd::structs::access_unit::AccessUnit,
};

fn sample_path() -> Option<std::path::PathBuf> {
    std::env::var_os("SALSA_TEST_TRUEHD").map(std::path::PathBuf::from)
}

#[test]
fn parses_and_decodes_the_header_and_first_commands() {
    let Some(path) = sample_path() else {
        eprintln!("SALSA_TEST_TRUEHD is unset; skipping the real-stream test");
        return;
    };

    let file = File::open(&path).expect("open test stream");
    let mut data = Vec::new();
    file.take(8 * 1024 * 1024)
        .read_to_end(&mut data)
        .expect("read test stream");

    let mut parser = TrueHdParser::new();
    let frames = parser.push_bytes(&data).expect("parse");
    assert!(!frames.is_empty(), "no access units parsed");

    let mut decoder = TrueHdDecoder::new();
    let mut snapshots = 0usize;
    let mut samples = 0usize;
    let mut objects = 0usize;
    for frame in &frames {
        let spatial = objectaudio_ir::SpatialDecoder::decode(&mut decoder, frame).expect("decode");
        assert!(spatial.sample_rate > 0);
        assert!(spatial.frames > 0);
        assert!(spatial.pcm.channel_count() <= 16);
        assert_eq!(spatial.objects.len() + 1, spatial.pcm.channel_count());
        assert!(
            spatial
                .pcm
                .channels()
                .iter()
                .all(|plane| plane.iter().all(|sample| sample.is_finite()))
        );
        samples += spatial.frames;
        objects = spatial.objects.len();
        for event in &spatial.events {
            assert!(event.offset as usize <= spatial.frames);
        }
        if decoder.take_snapshot().is_some() {
            snapshots += 1;
        }
    }

    assert!(snapshots > 0, "no OAMD payload was seen");
    assert_eq!(objects, 15, "the reference master has 15 dynamic objects");
    assert_eq!(samples, frames.len() * 40, "each access unit is 40 samples");
}

#[test]
fn direct_extractor_and_parser_match_the_wrapper() {
    let Some(path) = sample_path() else {
        eprintln!("SALSA_TEST_TRUEHD is unset; skipping the real-stream test");
        return;
    };

    let mut file = File::open(&path).expect("open test stream");
    file.seek(SeekFrom::Start(0)).expect("seek");
    let mut data = Vec::new();
    file.take(256 * 1024)
        .read_to_end(&mut data)
        .expect("read test stream");

    let mut extractor = Extractor::default();
    let mut parser = Parser::default();
    extractor.push_bytes(&data);
    let direct: Vec<AccessUnit> = std::iter::from_fn(|| {
        let item = extractor.next()?;
        let frame = item.ok()?;
        parser.parse(&frame).ok()
    })
    .collect();

    let mut wrapper = TrueHdParser::new();
    let wrapped = wrapper.push_bytes(&data).expect("parse");
    assert_eq!(direct.len(), wrapped.len());
    for (index, (direct, wrapped)) in direct.iter().zip(wrapped.iter()).enumerate() {
        let TrueHdFrame {
            access_unit, oamd, ..
        } = wrapped;
        assert_eq!(
            oamd_payloads(direct)
                .iter()
                .map(|p| &p.bytes)
                .collect::<Vec<_>>(),
            oamd.iter().map(|p| &p.bytes).collect::<Vec<_>>(),
            "access unit {index}"
        );
        assert_eq!(
            direct.access_unit_length, access_unit.access_unit_length,
            "access unit {index}"
        );
    }
}

#[test]
fn oamd_payloads_have_the_expected_id_and_offsets() {
    let Some(path) = sample_path() else {
        eprintln!("SALSA_TEST_TRUEHD is unset; skipping the real-stream test");
        return;
    };
    let file = File::open(path).expect("open test stream");
    let mut data = Vec::new();
    file.take(256 * 1024)
        .read_to_end(&mut data)
        .expect("read test stream");

    let mut parser = TrueHdParser::new();
    let frames = parser.push_bytes(&data).expect("parse");
    let payloads: Vec<&OamdPayload> = frames.iter().flat_map(|frame| &frame.oamd).collect();
    assert!(!payloads.is_empty(), "no OAMD payloads");
    for payload in payloads {
        let parsed = objectaudio_oamd::parse(&payload.bytes).expect("OAMD parse");
        assert_eq!(parsed.object_count, 16);
        assert_eq!(parsed.program.num_dynamic_objects, 15);
        assert!(parsed.bed.contains(objectaudio_oamd::BedChannel::Lfe));
    }
}
