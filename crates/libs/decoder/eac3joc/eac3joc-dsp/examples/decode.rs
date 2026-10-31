use std::fs;

use eac3joc::{Eac3JocFrame, Eac3JocParser};
use eac3joc_dsp::Eac3JocDecoder;
use objectaudio_ir::SpatialDecoder;

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: decode <in.ec3> [frames] [out.f32]");
    let frames: usize = args
        .next()
        .map(|value| value.parse().expect("frame count"))
        .unwrap_or(10);
    let output = args.next();
    let bytes = fs::read(&input).expect("read input");
    let mut parser = Eac3JocParser::new();
    let mut frames_out = parser.push_bytes(&bytes).expect("parse");
    frames_out.extend(parser.flush().expect("flush"));
    println!("audio frames: {}", frames_out.len());
    let mut decoder = Eac3JocDecoder::try_new().expect("decoder");
    let mut pcm = Vec::new();
    for frame in frames_out.iter().take(frames) {
        let spatial = decode_frame(&mut decoder, frame);
        pcm.extend_from_slice(&spatial.pcm.interleave());
    }
    if let Some(path) = output {
        fs::write(path, to_le_bytes(&pcm)).expect("write output");
    }
}

fn decode_frame(
    decoder: &mut Eac3JocDecoder,
    frame: &Eac3JocFrame,
) -> objectaudio_ir::SpatialFrame {
    match SpatialDecoder::decode(decoder, frame) {
        Ok(frame) => frame,
        Err(error) => panic!("decode failed at {}: {error}", frame.index),
    }
}

fn to_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}
