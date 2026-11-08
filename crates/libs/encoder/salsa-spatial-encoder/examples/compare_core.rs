//! Verify that parameter experiments preserve every Opus core packet.
//! Usage: compare_core <reference.ogg> <candidate.ogg> [...]

use anyhow::{Context, Result, ensure};
use objectaudio_ir::SpatialParser;
use salsa_spatial::SalsaSpatialParser;
use sha2::{Digest, Sha256};

fn fingerprint(path: &str) -> Result<(String, usize)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let mut reader = SalsaSpatialParser::new();
    let mut frames = reader.push_bytes(&bytes)?;
    frames.extend(reader.flush()?);
    let mut hash = Sha256::new();
    let mut count = 0;
    for frame in frames {
        for packet in frame.packets {
            for stream in packet.substreams {
                hash.update((stream.len() as u64).to_le_bytes());
                hash.update(&stream);
            }
            count += 1;
        }
    }
    ensure!(count > 0, "{path} has no core packets");
    Ok((format!("{:x}", hash.finalize()), count))
}

fn main() -> Result<()> {
    let paths: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        paths.len() >= 2,
        "usage: compare_core <reference.ogg> <candidate.ogg> [...]"
    );
    let reference = fingerprint(&paths[0])?;
    println!("{} packets, SHA-256 {}", reference.1, reference.0);
    for path in &paths[1..] {
        ensure!(
            fingerprint(path)? == reference,
            "core packets differ: {path}"
        );
        println!("identical: {path}");
    }
    Ok(())
}
