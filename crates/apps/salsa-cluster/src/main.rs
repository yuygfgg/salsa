// SPDX-License-Identifier: MPL-2.0
//! CLI for perceptual DAMF object clustering.

use std::path::PathBuf;

use anyhow::Result;
use atmos_cluster::{
    AssignMode, AudioSourceWeighting, CentroidScore, ClusterSettings, LimitMode, run_cluster,
};
use clap::{Parser, ValueEnum};

#[derive(Parser)]
#[command(
    name = "salsa-cluster",
    version,
    about = "Cluster a DAMF master to at most N dynamic objects"
)]
struct Cli {
    /// Input `.atmos` manifest.
    atmos: PathBuf,
    /// Output `.atmos` path or directory.
    #[arg(long)]
    out: PathBuf,
    /// Output stem when --out is a directory.
    #[arg(long)]
    stem: Option<String>,

    /// Target dynamic clusters (default 15).
    #[arg(long, default_value_t = 15)]
    max_objects: usize,
    /// Samples between centroid updates.
    #[arg(long, default_value_t = 1536)]
    analysis_hop: usize,
    #[arg(long, default_value_t = 64.0)]
    window_ms: f64,
    #[arg(long, default_value_t = 100.0)]
    db_max: f64,
    #[arg(long, default_value_t = 0.5)]
    importance_smooth_s: f64,
    #[arg(long, default_value_t = 1.0)]
    adaptive_smooth_s: f64,
    #[arg(long, default_value_t = 20.0)]
    gain_smooth_ms: f64,
    #[arg(long, default_value_t = 6.0)]
    transient_db: f64,
    #[arg(long, default_value_t = 0.995)]
    unmask_fmax: f64,
    #[arg(long, default_value_t = 0.5)]
    unmask_d0: f64,
    #[arg(long, default_value_t = 0.5)]
    width_weight: f64,
    #[arg(long, default_value_t = 0.0)]
    dialog_boost_db: f64,

    /// Centroid ranking: FIG. 12 loudness or marginal rendered reconstruction.
    #[arg(long, value_enum, default_value = "loudness")]
    centroid_score: CentroidScoreArg,
    /// Object-to-cluster assignment.
    #[arg(long, value_enum, default_value = "audio")]
    assign: AssignArg,
    /// Treat non-LFE bed channels as fixed-position inputs.
    #[arg(long)]
    beds_as_objects: bool,
    /// Enable the Silero VAD content prior (tract-onnx).
    #[arg(long)]
    vad: bool,
    #[arg(long, default_value_t = 6.0)]
    vad_gain_db: f64,
    #[arg(long, default_value_t = 0.1)]
    vad_threshold: f64,
    #[arg(long, default_value_t = 100.0)]
    vad_smooth_ms: f64,
    #[arg(long, default_value_t = 0.02)]
    audio_spatial_weight: f64,
    #[arg(long, default_value_t = 12)]
    audio_iterations: usize,
    #[arg(long, value_enum, default_value = "none")]
    audio_source_weighting: AudioWeightingArg,
    #[arg(long, default_value_t = 2)]
    metadata_interval_frames: usize,
    #[arg(long, default_value_t = 500.0)]
    centroid_hold_ms: f64,
    #[arg(long, default_value_t = 0.5)]
    centroid_stability: f64,
    #[arg(long, default_value_t = 0.1)]
    centroid_min_distance: f64,
    #[arg(long, value_enum, default_value = "peak")]
    limit: LimitArg,

    /// Analyse only the first N seconds (0 = whole master).
    #[arg(long, default_value_t = 0.0)]
    max_seconds: f64,
    #[arg(long)]
    workdir: Option<PathBuf>,
    #[arg(long)]
    keep_workdir: bool,
    #[arg(long, default_value_t = 500)]
    progress: usize,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CentroidScoreArg {
    Loudness,
    Reconstruction,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AssignArg {
    Nearest,
    Pan2,
    Pan3,
    Auto,
    Audio,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AudioWeightingArg {
    None,
    Power,
    Importance,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LimitArg {
    Peak,
    None,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Cli::parse();
    let settings = ClusterSettings {
        max_objects: args.max_objects,
        hop: args.analysis_hop,
        window_ms: args.window_ms,
        db_max: args.db_max,
        importance_smooth_s: args.importance_smooth_s,
        adaptive_smooth_s: args.adaptive_smooth_s,
        gain_smooth_ms: args.gain_smooth_ms,
        transient_db: args.transient_db,
        unmask_fmax: args.unmask_fmax,
        unmask_d0: args.unmask_d0,
        width_weight: args.width_weight,
        dialog_boost_db: args.dialog_boost_db,
        use_vad: args.vad,
        vad_gain_db: args.vad_gain_db,
        vad_threshold: args.vad_threshold,
        vad_smooth_ms: args.vad_smooth_ms,
        centroid_score: match args.centroid_score {
            CentroidScoreArg::Loudness => CentroidScore::Loudness,
            CentroidScoreArg::Reconstruction => CentroidScore::Reconstruction,
        },
        assign: match args.assign {
            AssignArg::Nearest => AssignMode::Nearest,
            AssignArg::Pan2 => AssignMode::Pan2,
            AssignArg::Pan3 => AssignMode::Pan3,
            AssignArg::Auto => AssignMode::Auto,
            AssignArg::Audio => AssignMode::Audio,
        },
        beds_as_objects: args.beds_as_objects,
        audio_spatial_weight: args.audio_spatial_weight,
        audio_iterations: args.audio_iterations,
        audio_source_weighting: match args.audio_source_weighting {
            AudioWeightingArg::None => AudioSourceWeighting::None,
            AudioWeightingArg::Power => AudioSourceWeighting::Power,
            AudioWeightingArg::Importance => AudioSourceWeighting::Importance,
        },
        metadata_interval_frames: args.metadata_interval_frames,
        centroid_hold_ms: args.centroid_hold_ms,
        centroid_stability: args.centroid_stability,
        centroid_min_distance: args.centroid_min_distance,
        limit: match args.limit {
            LimitArg::Peak => LimitMode::Peak,
            LimitArg::None => LimitMode::None,
        },
        max_seconds: args.max_seconds.max(0.0),
        workdir: args.workdir,
        keep_workdir: args.keep_workdir,
    };
    run_cluster(
        &args.atmos,
        &args.out,
        &settings,
        args.stem.as_deref(),
        args.progress,
    )?;
    Ok(())
}
