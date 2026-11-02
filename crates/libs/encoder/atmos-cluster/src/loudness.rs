// SPDX-License-Identifier: MPL-2.0
//! One-third-octave excitation and partial loudness (TVL 2018 subset).

use std::sync::Arc;

use rayon::prelude::*;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::tvl2018;

pub const ERB_BANDS: usize = 150;

pub struct Loudness {
    pub fs: u32,
    pub n_fft: usize,
    db_max: f64,
    n_bins: usize,
    window: Vec<f32>,
    band_bins: Vec<Vec<usize>>,
    band_centers: Vec<f64>,
    d_hann_correction: f64,
    fft: Arc<dyn Fft<f32>>,
    scratch_len: usize,

    pub alpha: Vec<f64>,
    pub a: Vec<f64>,
    pub g: Vec<f64>,
    pub pu: Vec<f64>,
    p1000: f64,
    pub c: f64,

    geometry: Vec<f64>,
    abs_geometry: Vec<f64>,
    mask: Vec<bool>,
}

impl Loudness {
    pub fn new(fs: u32, window_ms: f64, db_max: f64) -> anyhow::Result<Self> {
        anyhow::ensure!(fs > 0, "sample rate must be positive");
        let n_fft = (f64::from(fs) * window_ms / 1000.0).round() as usize;
        anyhow::ensure!(n_fft >= 256, "window too short");
        let n_bins = n_fft / 2 + 1;

        let mut window = vec![0.0f32; n_fft];
        for (index, value) in window.iter_mut().enumerate() {
            *value = (0.5
                - 0.5 * (2.0 * std::f64::consts::PI * index as f64 / (n_fft - 1) as f64).cos())
                as f32;
        }

        let freqs: Vec<f64> = (0..n_bins)
            .map(|bin| bin as f64 * f64::from(fs) / n_fft as f64)
            .collect();
        let mut band_centers = Vec::new();
        let mut band_bins = Vec::new();
        for index in 0..28 {
            let center = 25.0 * 2.0_f64.powf(index as f64 / 3.0);
            if center >= f64::from(fs) / 2.0 {
                continue;
            }
            let low = center / 2.0_f64.powf(1.0 / 6.0);
            let high = center * 2.0_f64.powf(1.0 / 6.0);
            let mut bins: Vec<usize> = freqs
                .iter()
                .enumerate()
                .filter(|(_, frequency)| **frequency >= low && **frequency < high)
                .map(|(bin, _)| bin)
                .collect();
            if bins.is_empty() {
                let nearest = freqs
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        (*a - center)
                            .abs()
                            .partial_cmp(&(*b - center).abs())
                            .unwrap()
                    })
                    .map(|(bin, _)| bin)
                    .unwrap_or(0);
                bins.push(nearest);
            }
            band_centers.push(center);
            band_bins.push(bins);
        }

        let fc: Vec<f64> = (0..=149)
            .map(|index| tvl2018::erb_number_to_frequency(1.75 + index as f64 * 0.25))
            .collect();
        let alpha = fc.iter().map(|f| tvl2018::get_alpha(*f)).collect();
        let a = fc.iter().map(|f| tvl2018::get_a(*f)).collect();
        let g = fc.iter().map(|f| tvl2018::get_g_tvl(*f)).collect();
        let pu = fc.iter().map(|f| tvl2018::get_p(*f)).collect();
        let p1000 = tvl2018::get_p(1000.0);

        let mut geometry = vec![0.0; band_centers.len() * ERB_BANDS];
        let mut abs_geometry = vec![0.0; band_centers.len() * ERB_BANDS];
        let mut mask = vec![false; band_centers.len() * ERB_BANDS];
        for (band, center) in band_centers.iter().enumerate() {
            for (index, f) in fc.iter().enumerate() {
                let relative = (*center - *f) / *f;
                let entry = band * ERB_BANDS + index;
                geometry[entry] = relative;
                abs_geometry[entry] = relative.abs();
                mask[entry] = relative <= 4.0;
            }
        }

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n_fft);
        let scratch_len = fft.get_inplace_scratch_len();
        Ok(Self {
            fs,
            n_fft,
            db_max,
            n_bins,
            window,
            band_bins,
            band_centers,
            d_hann_correction: 10.0_f64.powf(3.32 / 10.0),
            fft,
            scratch_len,
            alpha,
            a,
            g,
            pu,
            p1000,
            c: 0.0631,
            geometry,
            abs_geometry,
            mask,
        })
    }

    /// Per-source third-octave levels, source-major `[source][band]`.
    ///
    /// `window` is planar `[source][sample]`.
    pub fn band_levels(&self, window: &[f32], n_sources: usize) -> Vec<f64> {
        let n_bands = self.band_centers.len();
        let mut levels = vec![0.0f64; n_sources * n_bands];
        levels.par_chunks_mut(n_bands).enumerate().for_each_init(
            || {
                (
                    vec![Complex32::new(0.0, 0.0); self.n_fft],
                    vec![Complex32::new(0.0, 0.0); self.scratch_len],
                    vec![0.0f64; self.n_bins],
                )
            },
            |(buffer, scratch, intensity), (source, out)| {
                let samples = &window[source * self.n_fft..(source + 1) * self.n_fft];
                for sample in 0..self.n_fft {
                    buffer[sample] = Complex32::new(samples[sample] * self.window[sample], 0.0);
                }
                self.fft.process_with_scratch(buffer, scratch);
                for (bin, value) in buffer.iter().take(self.n_bins).enumerate() {
                    let mut magnitude = value.norm() / self.n_fft as f32;
                    if bin > 0 {
                        magnitude *= 2.0;
                    }
                    intensity[bin] =
                        f64::from(magnitude) * f64::from(magnitude) * self.d_hann_correction;
                }
                for (band, bins) in self.band_bins.iter().enumerate() {
                    let mut power = 0.0;
                    for bin in bins {
                        power += intensity[*bin];
                    }
                    out[band] = 10.0 * (power + 1e-30).log10() + self.db_max;
                }
            },
        );
        levels
    }

    /// `[source][ERB]` excitation pattern.
    pub fn excitation(&self, levels: &[f64]) -> Vec<f64> {
        let n_bands = self.band_centers.len();
        let n_sources = levels.len() / n_bands;
        let mut output = vec![0.0f64; n_sources * ERB_BANDS];
        output.par_chunks_mut(ERB_BANDS).enumerate().for_each_init(
            || vec![0.0f64; n_bands],
            |power, (source, out)| {
                for band in 0..n_bands {
                    power[band] =
                        10.0f32.powf(levels[source * n_bands + band] as f32 / 10.0) as f64;
                }
                for index in 0..ERB_BANDS {
                    let pu = self.pu[index];
                    let p1000 = self.p1000;
                    let mut sum = 0.0;
                    for band in 0..n_bands {
                        let entry = band * ERB_BANDS + index;
                        if !self.mask[entry] {
                            continue;
                        }
                        let level = levels[source * n_bands + band];
                        let p = if self.geometry[entry] >= 0.0 {
                            pu
                        } else {
                            pu - 0.35 * (pu / p1000) * (level - 51.0)
                        };
                        let ag = self.abs_geometry[entry];
                        sum += (1.0 + p * ag) * (-p * ag).exp() * power[band];
                    }
                    out[index] = sum.max(1e-12);
                }
            },
        );
        output
    }

    /// Partial loudness of every object with spatial release from masking.
    pub fn partial_loudness_objects(
        &self,
        excitation: &[f64],
        f_pair: &[f64],
        sources: &[usize],
        active: &[bool],
    ) -> Vec<f64> {
        let n_sources = excitation.len() / ERB_BANDS;
        let mut total = vec![0.0f64; ERB_BANDS];
        for &source in sources {
            for band in 0..ERB_BANDS {
                total[band] += excitation[source * ERB_BANDS + band];
            }
        }
        let mut loudness = vec![0.0f64; n_sources];
        loudness
            .par_iter_mut()
            .enumerate()
            .for_each(|(source, value)| {
                if !active[source] {
                    return;
                }
                let mut sum = 0.0;
                for band in 0..ERB_BANDS {
                    let own = excitation[source * ERB_BANDS + band];
                    let mut weighted = 0.0;
                    for &other in sources {
                        weighted += f_pair[source * n_sources + other]
                            * excitation[other * ERB_BANDS + band];
                    }
                    let masker = (total[band] - weighted - own).max(0.0);
                    let g = self.g[band] as f32;
                    let a = self.a[band] as f32;
                    let alpha = self.alpha[band] as f32;
                    let own = own as f32;
                    let masker = masker as f32;
                    sum += ((g * (own + masker) + a).powf(alpha) - (g * masker + a).powf(alpha))
                        as f64;
                }
                *value = (self.c * sum).max(0.0);
            });
        loudness
    }

    /// FIG. 12 loudness-accounted-for score for the given candidate centroids.
    pub fn cluster_scores(
        &self,
        excitation: &[f64],
        f_cluster: &[f64],
        candidates: &[usize],
        sources: &[usize],
    ) -> Vec<f64> {
        let n_sources = excitation.len() / ERB_BANDS;
        if candidates.is_empty() {
            return vec![f64::NEG_INFINITY; n_sources];
        }
        let mut total = vec![0.0f64; ERB_BANDS];
        for &source in sources {
            for band in 0..ERB_BANDS {
                total[band] += excitation[source * ERB_BANDS + band];
            }
        }
        let candidate_count = candidates.len();
        let mut far = vec![0.0f64; ERB_BANDS * candidate_count];
        for (band, row) in far.chunks_mut(candidate_count).enumerate() {
            for &source in sources {
                let value = excitation[source * ERB_BANDS + band];
                if value == 0.0 {
                    continue;
                }
                let weights = &f_cluster[source * n_sources..(source + 1) * n_sources];
                for (slot, candidate) in candidates.iter().enumerate() {
                    row[slot] += value * weights[*candidate];
                }
            }
        }

        let total_term: Vec<f32> = (0..ERB_BANDS)
            .map(|band| {
                let g = self.g[band] as f32;
                let a = self.a[band] as f32;
                let alpha = self.alpha[band] as f32;
                (g * total[band] as f32 + a).powf(alpha)
            })
            .collect();
        let mut candidate_scores = vec![0.0f64; candidate_count];
        for (slot, score) in candidate_scores.iter_mut().enumerate() {
            let mut sum = 0.0f32;
            for band in 0..ERB_BANDS {
                let g = self.g[band] as f32;
                let a = self.a[band] as f32;
                let alpha = self.alpha[band] as f32;
                let far_value = far[band * candidate_count + slot] as f32;
                sum += total_term[band] - (g * far_value + a).powf(alpha);
            }
            *score = (self.c as f32 * sum).max(0.0) as f64;
        }
        let mut scores = vec![f64::NEG_INFINITY; n_sources];
        for (slot, candidate) in candidates.iter().enumerate() {
            scores[*candidate] = candidate_scores[slot];
        }
        scores
    }
}
