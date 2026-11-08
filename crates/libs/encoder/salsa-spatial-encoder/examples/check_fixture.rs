//! Decode a Salsa Ogg fixture with libopus multistream directly.
use anyhow::{Context, Result};
use opus::MSDecoder;
use salsa_spatial::OggOpusReader;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: check_fixture <file.ogg>")?;
    let bytes = std::fs::read(&path)?;
    let mut reader = OggOpusReader::new();
    let packets = reader.push_bytes(&bytes)?;
    let head = reader.head().context("no OpusHead")?;
    println!(
        "head: channels={} streams={} coupled={} mapping={:?} pre_skip={}",
        head.channels, head.stream_count, head.coupled_count, head.channel_mapping, head.pre_skip
    );
    let mut decoder = MSDecoder::new(
        48000,
        head.stream_count as u8,
        head.coupled_count as u8,
        &head.channel_mapping,
    )?;
    let mut ok = 0usize;
    let mut scratch = vec![0.0f32; 5760 * head.channels];
    for (index, packet) in packets.iter().enumerate() {
        match decoder.decode_float(&packet.data, &mut scratch, false) {
            Ok(frames) => {
                ok += 1;
                if index < 3 || index + 3 >= packets.len() {
                    println!(
                        "packet {index}: {frames} frames, first={:?}",
                        &scratch[..head.channels]
                    );
                }
            }
            Err(error) => {
                println!(
                    "packet {index}: decode error {error:?} len={}",
                    packet.data.len()
                );
                break;
            }
        }
    }
    println!("decoded {ok}/{} packets", packets.len());
    Ok(())
}
