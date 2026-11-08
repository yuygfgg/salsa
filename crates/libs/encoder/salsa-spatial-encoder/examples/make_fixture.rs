//! Write a short Salsa Ogg Opus multistream fixture for transport checks.
//!
//! Usage: make_fixture <output.ogg> [seconds]

use std::f64::consts::PI;
use std::fs;

use anyhow::{Context, Result};
use salsa_spatial::{
    EnhancementTile, OggSalsaWriter, SAMPLE_RATE, SalsaSpatialFrame, SalsaSpatialStreamConfig,
    TILE_SAMPLES,
};
use salsa_spatial_encoder::{MultistreamCodec, TransportSignals};

fn tone(frequency: f64, start: u64, frames: usize, amplitude: f32) -> Vec<f32> {
    (0..frames)
        .map(|index| {
            let sample = (start + index as u64) as f64;
            let phase = 2.0 * PI * frequency * sample / f64::from(SAMPLE_RATE);
            (f64::from(amplitude) * phase.sin()) as f32
        })
        .collect()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args
        .next()
        .context("usage: make_fixture <output.ogg> [seconds]")?;
    let seconds: f64 = args.next().unwrap_or_else(|| "1".to_owned()).parse()?;
    let total = (seconds * f64::from(SAMPLE_RATE)).round() as u64;

    let config = SalsaSpatialStreamConfig {
        object_count: 0,
        ..SalsaSpatialStreamConfig::default()
    };
    let mut codec = MultistreamCodec::new(config.clone())?;
    let pre_skip = codec.lookahead_samples() as u16;
    let (mut writer, mut bytes) = OggSalsaWriter::new(config, 0x5341_4c53, pre_skip)?;

    let mut start = 0u64;
    while start < total {
        let frames = (total - start).min(TILE_SAMPLES as u64) as usize;
        let signals = TransportSignals {
            base: [
                tone(200.0, start, frames, 0.15),
                tone(300.0, start, frames, 0.15),
                tone(400.0, start, frames, 0.15),
                tone(500.0, start, frames, 0.15),
                tone(600.0, start, frames, 0.15),
            ],
            aux: vec![
                tone(700.0, start, frames, 0.15),
                tone(800.0, start, frames, 0.15),
            ],
            lfe: tone(60.0, start, frames, 0.15),
            frames,
        };
        let packets = codec.encode_tile(&signals)?;
        let frame = SalsaSpatialFrame {
            sample_rate: SAMPLE_RATE,
            start_sample: start,
            frames,
            packets,
            enhancement: EnhancementTile {
                start_sample: start,
                frames,
                bytes: Vec::new(),
                snapshot: start == 0,
            },
            objects: Vec::new(),
            events: Vec::new(),
            snapshot: start == 0,
            parameters: None,
        };
        bytes.extend(writer.write_frame(&frame)?);
        start += frames as u64;
    }
    if let Some(tail) = codec.flush()? {
        bytes.extend(writer.write_tail(&tail)?);
    }
    bytes.extend(writer.finish());
    fs::write(&output, &bytes).with_context(|| format!("writing {output}"))?;
    let duration = writer.total_samples() as f64 / f64::from(SAMPLE_RATE);
    println!("wrote {output}: {} bytes, {:.3} s", bytes.len(), duration);
    Ok(())
}
