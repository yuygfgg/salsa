//! Re-encode only LFE and keep the existing full-range packets and metadata.
//! Usage: lfe_sweep <master.atmos> <baseline.ogg> <start_sample> <output_dir>

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use objectaudio_ir::SpatialParser;
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use salsa_spatial::{
    OPUS_FRAME_SAMPLES, OggSalsaWriter, SAMPLE_RATE, SalsaSpatialParser, TILE_SAMPLES,
};
use salsa_spatial_encoder::SourceReader;
use serde_json::json;

fn write_pcm(path: &Path, samples: &[f32]) -> Result<()> {
    let mut file = std::io::BufWriter::new(fs::File::create(path)?);
    for value in samples {
        file.write_all(&value.to_le_bytes())?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 4,
        "usage: lfe_sweep <master.atmos> <baseline.ogg> <start_sample> <output_dir>"
    );
    let bytes = fs::read(&args[1])?;
    let mut reader = SalsaSpatialParser::new();
    let mut frames = reader.push_bytes(&bytes)?;
    frames.extend(reader.flush()?);
    let config = reader
        .config()
        .context("missing stream configuration")?
        .clone();
    let packet_count: usize = frames.iter().map(|frame| frame.packets.len()).sum();
    let sample_count: usize = frames.iter().map(|frame| frame.frames).sum();
    ensure!(
        sample_count > 0 && packet_count > 0,
        "empty baseline stream"
    );
    let output = Path::new(&args[3]);
    fs::create_dir_all(output)?;

    let mut source = SourceReader::open(&args[0])?;
    source.seek(args[2].parse()?)?;
    let mut lfe = Vec::with_capacity(sample_count);
    while lfe.len() < sample_count {
        let tile = source
            .read_tile((sample_count - lfe.len()).min(TILE_SAMPLES))?
            .context("source ended before the last core packet")?;
        lfe.extend(tile.lfe_pcm);
    }
    write_pcm(&output.join("reference.f32"), &lfe)?;
    let gain = config.headroom_gain as f32;
    let mut scaled: Vec<_> = lfe.iter().map(|sample| sample * gain).collect();
    scaled.resize(packet_count * OPUS_FRAME_SAMPLES, 0.0);
    let mut results = Vec::new();

    for rate in [16_000, 24_000, 32_000, 48_000] {
        // Match the production LFE encoder. Baseline packet equality below
        // detects changes to either these settings or source preparation.
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::LowDelay)?;
        encoder.set_complexity(10)?;
        encoder.set_bitrate(Bitrate::Bits(rate))?;
        encoder.set_vbr(true)?;
        encoder.set_vbr_constraint(false)?;
        encoder.set_dtx(false)?;
        encoder.set_inband_fec(false)?;
        let lookahead = encoder.get_lookahead()? as usize;
        let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Mono)?;
        let mut decoded = Vec::with_capacity(sample_count);
        let mut scratch = vec![0.0; OPUS_FRAME_SAMPLES];
        let mut updated = frames.clone();
        let mut payload_bytes = 0;
        let mut identical_packets = 0;
        for (index, packet) in updated
            .iter_mut()
            .flat_map(|frame| &mut frame.packets)
            .enumerate()
        {
            let offset = index * OPUS_FRAME_SAMPLES;
            let encoded =
                encoder.encode_vec_float(&scaled[offset..offset + OPUS_FRAME_SAMPLES], 8192)?;
            identical_packets += usize::from(encoded == packet.substreams[4]);
            payload_bytes += encoded.len();
            let count = decoder.decode_float(&encoded, &mut scratch, false)?;
            decoded.extend(scratch[..count].iter().map(|sample| sample / gain));
            packet.substreams[4] = encoded;
        }
        if rate as u32 == config.core_bitrates[4] {
            ensure!(
                identical_packets == packet_count,
                "baseline LFE packets differ: {identical_packets}/{packet_count}"
            );
        }
        // Do not synthesize an untransmitted EOS packet. The common metric
        // span must end before this core decoder's unavailable tail.
        write_pcm(
            &output.join(format!("lfe-{rate}.f32")),
            &decoded[lookahead..decoded.len().min(lookahead + sample_count)],
        )?;
        let mut updated_config = config.clone();
        updated_config.core_bitrates[4] = rate as u32;
        let (mut writer, mut remuxed) =
            OggSalsaWriter::new(updated_config.clone(), 1, lookahead as u16)?;
        for frame in &updated {
            remuxed.extend(writer.write_frame(frame)?);
        }
        remuxed.extend(writer.finish());
        fs::write(output.join(format!("lfe-{rate}.ogg")), &remuxed)?;
        results.push(json!({
            "target_bps": rate,
            "payload_bytes": payload_bytes,
            "actual_lfe_bps": payload_bytes as f64 * 8.0 * f64::from(SAMPLE_RATE) / sample_count as f64,
            "total_bps": remuxed.len() as f64 * 8.0 * f64::from(SAMPLE_RATE) / sample_count as f64,
            "identical_baseline_packets": identical_packets,
            "packet_count": packet_count,
            "lookahead_samples": lookahead,
        }));
    }
    let report = json!({"baseline": args[1], "start_sample": args[2].parse::<u64>()?,
        "source_samples": sample_count, "headroom_gain": config.headroom_gain, "rates": results});
    fs::write(
        output.join("rates.json"),
        serde_json::to_string_pretty(&report)? + "\n",
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
