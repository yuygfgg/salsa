// SPDX-License-Identifier: MPL-2.0
//! Perceptual object clustering for Dolby Atmos Master Format masters.
//!
//! The pipeline reads a DAMF presentation, analyses per-object partial
//! loudness and spatial unmasking, selects at most `max_objects` centroids and
//! renders the source scene into the clustered object set. The output is a
//! standard DAMF master (`LFE + N` dynamic objects) that can be fed directly to
//! the salsa E-AC-3 JOC encoder.
//!
//! This is a Rust port of `atmos_cluster`; it deliberately reuses
//! `objectaudio-damf` for the DAMF reader/writer instead of reimplementing the
//! container and metadata formats.

#![forbid(unsafe_code)]

mod audio_assign;
mod clustering;
mod loudness;
mod matrix;
mod panning;
mod pipeline;
mod reconstruction;
mod renderer;
mod settings;
mod spatial;
mod timeline;
mod tvl2018;
mod vad;

pub use pipeline::{ClusterResult, run_cluster};
pub use renderer::RenderStats;
pub use settings::{AssignMode, AudioSourceWeighting, CentroidScore, ClusterSettings, LimitMode};
pub use timeline::{FrameTracks, SourceSet, Timeline, build_sources, sample_tracks};
