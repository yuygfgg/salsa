// SPDX-License-Identifier: MPL-2.0
//! Stateful streaming renderer for the clustering decisions.

use anyhow::{Result, ensure};
use rayon::prelude::*;

use crate::audio_assign::audio_reconstruction_gains;
use crate::clustering::{ClusterFrameMetadata, assign_gains, cluster_metadata, select_centroids};
use crate::loudness::Loudness;
use crate::reconstruction::select_centroids_reconstruction;
use crate::settings::{AssignMode, CentroidScore, ClusterSettings, SILENCE_POWER};
use crate::spatial::{clustering_unmasking, unmasking_matrix};

pub struct FrameFeatures<'a> {
    pub positions: &'a [[f64; 3]],
    pub active: &'a [bool],
    pub sizes: &'a [f64],
    pub content_prior: &'a [f64],
    pub dialog: &'a [f64],
    pub music: &'a [f64],
    pub content_gain: Option<&'a [f64]>,
    pub content_importance: Option<&'a [f64]>,
    pub dialog_probability: Option<&'a [f64]>,
}

pub struct ClusterFrame {
    pub audio: Vec<f32>,
    pub metadata: ClusterFrameMetadata,
    pub selected: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct RenderStats {
    pub err_num: f64,
    pub err_weight: f64,
    pub err_max: f64,
    pub effective_err_num: f64,
    pub effective_err_weight: f64,
    pub effective_err_max: f64,
    pub occupancy: Vec<f64>,
    pub occ_frames: usize,
}

impl RenderStats {
    pub fn error_mean(&self) -> f64 {
        if self.err_weight > 0.0 {
            self.err_num / self.err_weight
        } else {
            0.0
        }
    }

    pub fn effective_error_mean(&self) -> f64 {
        if self.effective_err_weight > 0.0 {
            self.effective_err_num / self.effective_err_weight
        } else {
            0.0
        }
    }

    pub fn occupancy_mean(&self) -> Vec<f64> {
        if self.occ_frames == 0 {
            return self.occupancy.clone();
        }
        self.occupancy
            .iter()
            .map(|value| value / self.occ_frames as f64)
            .collect()
    }
}

pub struct ClusterRenderer<'a> {
    settings: &'a ClusterSettings,
    zones: Vec<String>,
    fixed_mask: Vec<bool>,
    fixed_indices: Vec<usize>,
    n_sources: usize,
    target: usize,

    n_smooth: Vec<f64>,
    base_smooth_tau: f64,
    gain_alpha: f64,

    prev_gains: Option<Vec<f64>>,
    prev_centroids: Option<Vec<[f64; 3]>>,
    prev_energy: Option<Vec<f64>>,
    prev_selected: Vec<usize>,
    hold_frames: usize,
    selected_hold: Vec<usize>,
    pub cluster_zones: Vec<String>,
    started: bool,
    stats: RenderStats,
    pub timing_partial: f64,
    pub timing_select: f64,
    pub timing_gains: f64,
    pub timing_output: f64,
    pub timing_stats: f64,
}

impl<'a> ClusterRenderer<'a> {
    pub fn new(
        fs: u32,
        settings: &'a ClusterSettings,
        zones: Vec<String>,
        fixed_mask: Vec<bool>,
    ) -> Self {
        let n_sources = fixed_mask.len();
        let fixed_indices = fixed_mask
            .iter()
            .enumerate()
            .filter(|(_, fixed)| **fixed)
            .map(|(index, _)| index)
            .collect();
        let target = settings.max_objects.min(n_sources);
        let base_smooth_tau = settings.importance_smooth_s.max(1e-6);
        let gain_alpha = (-(settings.hop as f64)
            / (f64::from(fs) * settings.gain_smooth_ms.max(1e-6) / 1000.0))
            .exp();
        let hold_frames = ((settings.centroid_hold_ms / 1000.0 * f64::from(fs)
            / settings.hop as f64)
            .round() as usize)
            .max(1);
        Self {
            settings,
            zones,
            fixed_mask,
            fixed_indices,
            n_sources,
            target,
            n_smooth: vec![0.0; n_sources],
            base_smooth_tau,
            gain_alpha,
            prev_gains: None,
            prev_centroids: None,
            prev_energy: None,
            prev_selected: Vec::new(),
            hold_frames,
            selected_hold: vec![0; n_sources],
            cluster_zones: Vec::new(),
            started: false,
            stats: RenderStats {
                occupancy: vec![0.0; target],
                ..RenderStats::default()
            },
            timing_partial: 0.0,
            timing_select: 0.0,
            timing_gains: 0.0,
            timing_output: 0.0,
            timing_stats: 0.0,
        }
    }

    pub fn stats(&self) -> &RenderStats {
        &self.stats
    }

    pub fn set_zones(&mut self, zones: &[String]) {
        self.zones.clone_from_slice(zones);
    }

    pub fn render(
        &mut self,
        loudness: &Loudness,
        excitation: &mut [f64],
        features: &FrameFeatures<'_>,
        block: &[f64],
        lfe: Option<&[f64]>,
    ) -> Result<ClusterFrame> {
        let n_sources = self.n_sources;
        let bands = excitation.len() / n_sources;
        let length = block.len().checked_div(n_sources).unwrap_or(0);

        let mut block_power = vec![0.0f64; n_sources];
        if length > 0 {
            for frame in 0..length {
                let row = &block[frame * n_sources..(frame + 1) * n_sources];
                for (source, value) in row.iter().enumerate() {
                    block_power[source] += value * value;
                }
            }
            for value in &mut block_power {
                *value /= length as f64;
            }
        }

        let audible: Vec<bool> = (0..n_sources)
            .map(|source| features.active[source] && block_power[source] > SILENCE_POWER)
            .collect();
        for value in &mut self.selected_hold {
            *value = value.saturating_sub(1);
        }
        if !audible.iter().any(|value| *value) {
            excitation.fill(0.0);
        } else {
            for source in 0..n_sources {
                if !audible[source] {
                    excitation[source * bands..(source + 1) * bands].fill(0.0);
                }
            }
        }
        if let Some(gain) = features.content_gain {
            for source in 0..n_sources {
                let factor = gain[source];
                for value in &mut excitation[source * bands..(source + 1) * bands] {
                    *value *= factor;
                }
            }
        }

        let stamp = std::time::Instant::now();
        let active_sources: Vec<usize> = (0..n_sources).filter(|source| audible[*source]).collect();
        let f_pair = unmasking_matrix(
            features.positions,
            self.settings.unmask_fmax,
            self.settings.unmask_d0,
        );
        let mut n_partial =
            loudness.partial_loudness_objects(excitation, &f_pair, &active_sources, &audible);
        for source in 0..n_sources {
            if !audible[source] {
                n_partial[source] = 0.0;
            }
        }
        let energy: Vec<f64> = (0..n_sources)
            .map(|source| {
                excitation[source * bands..(source + 1) * bands]
                    .iter()
                    .sum()
            })
            .collect();
        if !self.started {
            self.n_smooth = n_partial.clone();
        } else {
            let base_alpha = 1.0
                - (-(self.settings.hop as f64)
                    / (f64::from(loudness.fs) * self.base_smooth_tau.max(1e-6)))
                .exp();
            let mut alpha = vec![base_alpha; n_sources];
            if let Some(importance) = features.content_importance {
                for source in 0..n_sources {
                    let tau = self.base_smooth_tau
                        + self.settings.adaptive_smooth_s * importance[source].clamp(0.0, 1.0);
                    alpha[source] = 1.0
                        - (-(self.settings.hop as f64) / (f64::from(loudness.fs) * tau.max(1e-6)))
                            .exp();
                }
            }
            if self.settings.transient_db > 0.0
                && let Some(previous) = &self.prev_energy
            {
                for source in 0..n_sources {
                    let rise_db =
                        10.0 * ((energy[source] + 1e-20) / (previous[source] + 1e-20)).log10();
                    if rise_db > self.settings.transient_db && energy[source] > 1e-12 {
                        alpha[source] = 1.0;
                    }
                }
            }
            for source in 0..n_sources {
                self.n_smooth[source] = (1.0 - alpha[source]) * self.n_smooth[source]
                    + alpha[source] * n_partial[source];
            }
        }
        for source in 0..n_sources {
            if !audible[source] {
                self.n_smooth[source] = 0.0;
            }
        }
        self.prev_energy = Some(energy);
        self.timing_partial += stamp.elapsed().as_secs_f64();

        let stamp = std::time::Instant::now();
        let f_cluster = clustering_unmasking(
            features.positions,
            features.sizes,
            self.settings.width_weight,
            self.settings.unmask_fmax,
            self.settings.unmask_d0,
        );
        let selected = if self.settings.centroid_score == CentroidScore::Reconstruction {
            let candidate_active: Vec<bool> = (0..n_sources)
                .map(|source| audible[source] || self.selected_hold[source] > 0)
                .collect();
            let prev_hold: Vec<usize> = self
                .prev_selected
                .iter()
                .copied()
                .filter(|index| *index < n_sources && candidate_active[*index])
                .collect();
            select_centroids_reconstruction(
                features.positions,
                features.sizes,
                &audible,
                block,
                length,
                &self.n_smooth,
                features.content_prior,
                self.target,
                &self.fixed_indices,
                self.settings.width_weight,
                self.settings.audio_spatial_weight,
                self.settings.audio_iterations,
                self.settings.audio_source_weighting,
                &prev_hold,
                self.settings.centroid_stability,
                self.settings.centroid_min_distance,
            )
        } else {
            select_centroids(
                loudness,
                excitation,
                &f_cluster,
                &audible,
                features.content_prior,
                &self.n_smooth,
                self.target,
                &self.fixed_indices,
                &active_sources,
            )
        };
        self.timing_select += stamp.elapsed().as_secs_f64();
        ensure!(
            selected.len() == self.target,
            "selected {} clusters, expected {}",
            selected.len(),
            self.target
        );

        let mut centroids: Vec<[f64; 3]> = selected
            .iter()
            .map(|index| features.positions[*index])
            .collect();
        let mut selected = selected;
        if let Some(previous) = &self.prev_centroids
            && previous.len() == centroids.len()
        {
            let n = centroids.len();
            let mut pairs = Vec::with_capacity(n * n);
            for row in 0..n {
                for column in 0..n {
                    let dx = centroids[row][0] - previous[column][0];
                    let dy = centroids[row][1] - previous[column][1];
                    let dz = centroids[row][2] - previous[column][2];
                    pairs.push(((dx * dx + dy * dy + dz * dz).sqrt(), row, column));
                }
            }
            pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut row_used = vec![false; n];
            let mut column_used = vec![false; n];
            let mut ordered_selected = vec![None; n];
            let mut ordered_centroids = vec![[0.0; 3]; n];
            for (_, row, column) in pairs {
                if row_used[row] || column_used[column] {
                    continue;
                }
                row_used[row] = true;
                column_used[column] = true;
                ordered_selected[column] = Some(selected[row]);
                ordered_centroids[column] = centroids[row];
            }
            if ordered_selected.iter().all(Option::is_some) {
                selected = ordered_selected.into_iter().flatten().collect();
                centroids = ordered_centroids;
            }
        }
        self.prev_centroids = Some(centroids.clone());

        let stamp = std::time::Instant::now();
        let n_clusters = selected.len();
        let gains_target = if self.settings.assign == AssignMode::Audio {
            let importance: Vec<f64> = (0..n_sources)
                .map(|source| {
                    self.n_smooth[source].max(0.0) * features.content_prior[source].max(1e-9)
                })
                .collect();
            let anchors = if self.settings.centroid_score == CentroidScore::Reconstruction {
                Some(self.fixed_indices.as_slice())
            } else {
                None
            };
            audio_reconstruction_gains(
                block,
                length,
                features.positions,
                features.sizes,
                &audible,
                &selected,
                &centroids,
                self.settings.width_weight,
                self.settings.audio_spatial_weight,
                self.settings.audio_iterations,
                anchors,
                self.settings.audio_source_weighting,
                Some(&importance),
            )
        } else {
            assign_gains(
                features.positions,
                features.sizes,
                features.active,
                &selected,
                self.settings.assign,
                self.settings.width_weight,
            )
        };

        self.timing_gains += stamp.elapsed().as_secs_f64();
        let stamp = std::time::Instant::now();
        let (mut gains_smooth, gains_start) = if let Some(previous) = &self.prev_gains
            && previous.len() == gains_target.len()
        {
            let smooth: Vec<f64> = previous
                .iter()
                .zip(&gains_target)
                .map(|(old, target)| self.gain_alpha * old + (1.0 - self.gain_alpha) * target)
                .collect();
            (smooth, previous.clone())
        } else {
            (gains_target.clone(), gains_target.clone())
        };

        let mut out_clusters = vec![0.0f64; length * n_clusters];
        out_clusters
            .par_chunks_mut(n_clusters)
            .enumerate()
            .for_each(|(frame, out)| {
                let fraction = frame as f64 / length.max(1) as f64;
                let row = &block[frame * n_sources..(frame + 1) * n_sources];
                for (source, value) in row.iter().enumerate() {
                    if *value == 0.0 {
                        continue;
                    }
                    let gain_row = source * n_clusters;
                    for (cluster, item) in out.iter_mut().enumerate() {
                        let start = gains_start[gain_row + cluster];
                        let smooth = gains_smooth[gain_row + cluster];
                        *item += value * (start + (smooth - start) * fraction);
                    }
                }
            });

        let mut audio = vec![0.0f32; length * (1 + n_clusters)];
        for frame in 0..length {
            let base = frame * (1 + n_clusters);
            audio[base] = lfe.map_or(0.0, |samples| samples[frame] as f32);
            for cluster in 0..n_clusters {
                audio[base + 1 + cluster] = out_clusters[frame * n_clusters + cluster] as f32;
            }
        }

        self.timing_output += stamp.elapsed().as_secs_f64();
        let stamp = std::time::Instant::now();
        let dialog = features.dialog_probability.unwrap_or(features.dialog);
        let mut metadata = cluster_metadata(
            &gains_smooth,
            features.positions,
            features.sizes,
            &self.n_smooth,
            dialog,
            features.music,
            &self.zones,
            &centroids,
            &block_power,
            &audible,
        );
        for (cluster, source) in selected.iter().enumerate() {
            if self.fixed_mask[*source] {
                metadata.positions[cluster] = features.positions[*source];
                metadata.sizes[cluster] = 0.0;
            }
        }
        for index in &selected {
            if audible[*index] {
                self.selected_hold[*index] = self.hold_frames;
            }
        }
        self.prev_selected = selected.clone();
        if self.cluster_zones.is_empty() {
            self.cluster_zones = metadata.zones.clone();
        }

        let mut distance = vec![vec![0.0f64; n_clusters]; n_sources];
        for source in 0..n_sources {
            for cluster in 0..n_clusters {
                let dx = features.positions[source][0] - centroids[cluster][0];
                let dy = features.positions[source][1] - centroids[cluster][1];
                let dz = features.positions[source][2] - centroids[cluster][2];
                distance[source][cluster] = (dx * dx + dy * dy + dz * dz).sqrt();
            }
        }
        for source in 0..n_sources {
            let dmin = distance[source]
                .iter()
                .copied()
                .fold(f64::INFINITY, f64::min);
            let weight = self.n_smooth[source].max(0.0);
            self.stats.err_num += weight * dmin;
            self.stats.err_weight += weight;
            if weight > 0.0 {
                self.stats.err_max = self.stats.err_max.max(dmin);
            }
        }

        let mut power = vec![0.0f64; n_sources * n_clusters];
        for index in 0..power.len() {
            power[index] = gains_smooth[index] * gains_smooth[index];
        }
        for source in 0..n_sources {
            let power_sum: f64 = power[source * n_clusters..(source + 1) * n_clusters]
                .iter()
                .sum();
            if power_sum > 1e-15 {
                let mut rendered = [0.0f64; 3];
                for cluster in 0..n_clusters {
                    let weight = power[source * n_clusters + cluster];
                    rendered[0] += weight * centroids[cluster][0];
                    rendered[1] += weight * centroids[cluster][1];
                    rendered[2] += weight * centroids[cluster][2];
                }
                rendered[0] /= power_sum;
                rendered[1] /= power_sum;
                rendered[2] /= power_sum;
                let dx = features.positions[source][0] - rendered[0];
                let dy = features.positions[source][1] - rendered[1];
                let dz = features.positions[source][2] - rendered[2];
                let effective_distance = (dx * dx + dy * dy + dz * dz).sqrt();
                let effective_weight = self.n_smooth[source].max(0.0);
                self.stats.effective_err_num += effective_weight * effective_distance;
                self.stats.effective_err_weight += effective_weight;
                if effective_weight > 0.0 {
                    self.stats.effective_err_max =
                        self.stats.effective_err_max.max(effective_distance);
                }
            }
        }
        for cluster in 0..n_clusters {
            let count = (0..n_sources)
                .filter(|source| gains_smooth[*source * n_clusters + cluster] > 0.01)
                .count();
            if cluster < self.stats.occupancy.len() {
                self.stats.occupancy[cluster] += count as f64;
            }
        }
        self.stats.occ_frames += 1;
        self.timing_stats += stamp.elapsed().as_secs_f64();
        self.started = true;
        self.prev_gains = Some(std::mem::take(&mut gains_smooth));

        Ok(ClusterFrame {
            audio,
            metadata,
            selected,
        })
    }
}
