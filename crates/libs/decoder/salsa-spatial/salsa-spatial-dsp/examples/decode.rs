//! Decode a salsa-spatial IAMF stream and report or write the object PCM.
//!
//! Usage:
//! `cargo run -p salsa-spatial-dsp --example decode -- <input.iamf> [output.wav]`

use std::fs;
use std::io::{BufWriter, Write};

use objectaudio_ir::{SpatialDecoder, SpatialFrame};
use salsa_spatial::{IamfReader, SalsaSpatialStreamConfig};
use salsa_spatial_dsp::SalsaSpatialDecoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args.next().ok_or("missing input path")?;
    let output = args.next();

    let bytes = fs::read(&input)?;
    let mut reader = IamfReader::new();
    let mut frames = reader.push_bytes(&bytes)?;
    frames.extend(reader.flush()?);
    let config: SalsaSpatialStreamConfig = reader
        .config()
        .cloned()
        .ok_or("stream has no salsa-spatial configuration")?;

    let mut decoder = SalsaSpatialDecoder::new(config.clone())?;
    let mut decoded: Vec<SpatialFrame> = Vec::new();
    let mut samples = 0usize;
    let mut channels = 0usize;
    for frame in &frames {
        let spatial = decoder.decode(frame)?;
        channels = spatial.pcm.channel_count();
        samples += spatial.frames;
        decoded.push(spatial);
    }
    if let Some(tail) = decoder.finish()? {
        channels = tail.pcm.channel_count();
        samples += tail.frames;
        decoded.push(tail);
    }

    println!(
        "tiles={} objects={} channels={} samples={} seconds={:.3}",
        frames.len(),
        config.object_count,
        channels,
        samples,
        samples as f64 / f64::from(salsa_spatial::SAMPLE_RATE)
    );

    if let Some(path) = output {
        write_wav(&path, &decoded, config.object_count)?;
        println!("wrote {path}");
    }
    Ok(())
}

fn write_wav(
    path: &str,
    frames: &[SpatialFrame],
    object_count: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let channels = object_count + 1;
    let samples: usize = frames.iter().map(|frame| frame.frames).sum();
    let mut file = BufWriter::new(fs::File::create(path)?);
    let data_bytes = (samples * channels * 4) as u32;
    let riff_bytes = data_bytes + 36;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_bytes.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&3u16.to_le_bytes())?; // IEEE float
    file.write_all(&(channels as u16).to_le_bytes())?;
    file.write_all(&salsa_spatial::SAMPLE_RATE.to_le_bytes())?;
    let byte_rate = salsa_spatial::SAMPLE_RATE * channels as u32 * 4;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&((channels * 4) as u16).to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;

    for frame in frames {
        let interleaved = frame.pcm.interleave();
        for sample in interleaved {
            file.write_all(&sample.to_le_bytes())?;
        }
    }
    file.flush()?;
    Ok(())
}
