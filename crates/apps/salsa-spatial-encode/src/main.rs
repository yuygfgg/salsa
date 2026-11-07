//! CLI for the salsa-spatial encoder.

mod progress;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use salsa_spatial_encoder::{EncodeOptions, encode_with_progress};

use crate::progress::Progress;

#[derive(Parser)]
#[command(
    name = "salsa-spatial-encode",
    version,
    about = "Encode a Dolby Atmos Master (DAMF) to a salsa-spatial IAMF stream"
)]
struct Cli {
    /// Path to the `.atmos` manifest.
    master: PathBuf,
    /// Output IAMF stream.
    #[arg(short, long)]
    out: PathBuf,
    /// Metadata rate-distortion weight per encoded bit.
    #[arg(long, default_value_t = 1e-6)]
    beta: f64,
    /// Encode at most this many source frames.
    #[arg(long)]
    max_frames: Option<usize>,
    /// First source sample to encode.
    #[arg(long, default_value_t = 0)]
    reference_start: u64,
    /// Override the automatic headroom gain, in dB (e.g. -6).
    #[arg(long, allow_hyphen_values = true)]
    headroom_db: Option<f64>,
    /// Front L/R Opus bitrate in bits/s.
    #[arg(long, default_value_t = 160_000)]
    front_bitrate: u32,
    /// Surround Ls/Rs Opus bitrate in bits/s.
    #[arg(long, default_value_t = 128_000)]
    surround_bitrate: u32,
    /// Height H_L/H_R Opus bitrate in bits/s.
    #[arg(long, default_value_t = 160_000)]
    height_bitrate: u32,
    /// Center Opus bitrate in bits/s.
    #[arg(long, default_value_t = 112_000)]
    center_bitrate: u32,
    /// LFE Opus bitrate in bits/s.
    #[arg(long, default_value_t = 24_000)]
    lfe_bitrate: u32,
    /// Optional path for the JSON report.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Disable progress output on stderr.
    #[arg(long)]
    no_progress: bool,
    /// Enable info-level logging (or set RUST_LOG).
    #[arg(long)]
    log: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.log {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    let options = EncodeOptions {
        master: cli.master,
        output: cli.out,
        core_bitrates: [
            cli.front_bitrate,
            cli.surround_bitrate,
            cli.height_bitrate,
            cli.center_bitrate,
            cli.lfe_bitrate,
        ],
        beta: cli.beta,
        max_frames: cli.max_frames,
        reference_start: cli.reference_start,
        headroom_db: cli.headroom_db,
    };

    let mut progress = Progress::new(!cli.no_progress);
    let result = encode_with_progress(&options, |update| progress.update(update));
    progress.finish();
    let report = result?;
    let json = serde_json::to_string_pretty(&report).context("serializing the encode report")?;
    if let Some(path) = cli.report {
        std::fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
    }
    println!("{json}");
    Ok(())
}
