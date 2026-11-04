// SPDX-License-Identifier: MPL-2.0
//! Algorithm parameters for one clustering run.

use std::path::PathBuf;

pub const DEFAULT_HOP: usize = 1536;
pub const DEFAULT_WINDOW_MS: f64 = 64.0;
pub const DEFAULT_DB_MAX: f64 = 100.0;
pub const SILENCE_POWER: f64 = 1e-10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignMode {
    Nearest,
    Pan2,
    Pan3,
    Auto,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CentroidScore {
    Loudness,
    Reconstruction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSourceWeighting {
    None,
    Power,
    Importance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitMode {
    Peak,
    None,
}

#[derive(Debug, Clone)]
pub struct ClusterSettings {
    pub max_objects: usize,
    pub hop: usize,
    pub window_ms: f64,
    pub db_max: f64,
    pub importance_smooth_s: f64,
    pub adaptive_smooth_s: f64,
    pub gain_smooth_ms: f64,
    pub transient_db: f64,
    pub unmask_fmax: f64,
    pub unmask_d0: f64,
    pub width_weight: f64,
    pub dialog_boost_db: f64,
    pub use_vad: bool,
    pub vad_gain_db: f64,
    pub vad_threshold: f64,
    pub vad_smooth_ms: f64,
    pub centroid_score: CentroidScore,
    pub assign: AssignMode,
    pub beds_as_objects: bool,
    pub audio_spatial_weight: f64,
    pub audio_iterations: usize,
    pub audio_source_weighting: AudioSourceWeighting,
    pub metadata_interval_frames: usize,
    pub centroid_hold_ms: f64,
    pub centroid_stability: f64,
    pub centroid_min_distance: f64,
    pub limit: LimitMode,
    pub max_seconds: f64,
    pub workdir: Option<PathBuf>,
    pub keep_workdir: bool,
}

impl Default for ClusterSettings {
    fn default() -> Self {
        Self {
            max_objects: 15,
            hop: DEFAULT_HOP,
            window_ms: DEFAULT_WINDOW_MS,
            db_max: DEFAULT_DB_MAX,
            importance_smooth_s: 0.5,
            adaptive_smooth_s: 1.0,
            gain_smooth_ms: 20.0,
            transient_db: 6.0,
            unmask_fmax: 0.995,
            unmask_d0: 0.5,
            width_weight: 0.5,
            dialog_boost_db: 0.0,
            use_vad: false,
            vad_gain_db: 6.0,
            vad_threshold: 0.1,
            vad_smooth_ms: 100.0,
            centroid_score: CentroidScore::Loudness,
            assign: AssignMode::Audio,
            beds_as_objects: false,
            audio_spatial_weight: 0.02,
            audio_iterations: 12,
            audio_source_weighting: AudioSourceWeighting::None,
            metadata_interval_frames: 2,
            centroid_hold_ms: 500.0,
            centroid_stability: 0.5,
            centroid_min_distance: 0.1,
            limit: LimitMode::Peak,
            max_seconds: 0.0,
            workdir: None,
            keep_workdir: false,
        }
    }
}

impl ClusterSettings {
    pub fn validate(&self) -> crate::Result<()> {
        crate::ensure!(self.max_objects > 0, "max_objects must be positive");
        crate::ensure!(self.hop > 0, "hop must be positive");
        crate::ensure!(self.window_ms > 0.0, "window_ms must be positive");
        crate::ensure!(
            self.importance_smooth_s > 0.0,
            "importance_smooth_s must be positive"
        );
        crate::ensure!(
            self.adaptive_smooth_s >= 0.0,
            "adaptive_smooth_s must not be negative"
        );
        crate::ensure!(self.gain_smooth_ms > 0.0, "gain_smooth_ms must be positive");
        crate::ensure!(
            self.transient_db >= 0.0,
            "transient_db must not be negative"
        );
        crate::ensure!(
            self.audio_spatial_weight >= 0.0,
            "audio_spatial_weight must not be negative"
        );
        crate::ensure!(
            self.metadata_interval_frames >= 1,
            "metadata_interval_frames must be positive"
        );
        crate::ensure!(
            self.centroid_hold_ms >= 0.0,
            "centroid_hold_ms must not be negative"
        );
        crate::ensure!(
            self.centroid_stability >= 0.0,
            "centroid_stability must not be negative"
        );
        crate::ensure!(
            self.centroid_min_distance >= 0.0,
            "centroid_min_distance must not be negative"
        );
        crate::ensure!(self.vad_gain_db >= 0.0, "vad_gain_db must not be negative");
        crate::ensure!(
            (0.0..=1.0).contains(&self.vad_threshold),
            "vad_threshold must be in [0, 1]"
        );
        crate::ensure!(
            self.vad_smooth_ms >= 0.0,
            "vad_smooth_ms must not be negative"
        );
        crate::ensure!(self.max_seconds >= 0.0, "max_seconds must not be negative");
        if self.centroid_score == CentroidScore::Reconstruction {
            crate::ensure!(
                self.assign == AssignMode::Audio,
                "centroid-score reconstruction requires --assign audio"
            );
        }
        Ok(())
    }
}
