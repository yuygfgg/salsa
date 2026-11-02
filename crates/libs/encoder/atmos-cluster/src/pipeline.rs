// SPDX-License-Identifier: MPL-2.0
//! End-to-end DAMF clustering orchestration.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use objectaudio_damf::{
    BedEntry, CafReader, CafWriter, DamfEvent, Gain, Manifest, ManifestSpec, Metadata, ObjectEntry,
    write_manifest,
};

use crate::loudness::Loudness;
use crate::renderer::{ClusterRenderer, FrameFeatures, RenderStats};
use crate::settings::{ClusterSettings, LimitMode};
use crate::timeline::{Timeline, build_sources, sample_tracks};
use crate::vad::{VadBank, content_gain, content_importance};

#[derive(Debug, Clone)]
pub struct ClusterResult {
    pub source: PathBuf,
    pub manifest: PathBuf,
    pub metadata: PathBuf,
    pub audio: PathBuf,
    pub output_dir: PathBuf,
    pub stem: String,
    pub clusters: usize,
    pub sample_rate: u32,
    pub total_frames: u64,
    pub stats: RenderStats,
}

struct SourceReader {
    caf: CafReader,
    channels: usize,
    sources: Vec<Timeline>,
    lfe_channel: Option<usize>,
    n_fft: usize,
    hop: usize,
    total_frames: u64,
    // Gain-adjusted samples covering both the analysis window and output block.
    buf: Vec<f32>,
    buf_start: Option<i64>,
    buf_frames: usize,
    window: Vec<f32>,
    block: Vec<f64>,
    lfe: Vec<f64>,
}

impl SourceReader {
    fn new(
        caf: CafReader,
        sources: Vec<Timeline>,
        lfe_channel: Option<usize>,
        n_fft: usize,
        hop: usize,
        total_frames: u64,
    ) -> Self {
        let channels = caf.info().channels;
        let source_count = sources.len();
        let buf_frames = n_fft.max(hop);
        Self {
            caf,
            channels,
            sources,
            lfe_channel,
            n_fft,
            hop,
            total_frames,
            buf: vec![0.0; buf_frames * channels],
            buf_start: None,
            buf_frames,
            window: vec![0.0; n_fft * source_count],
            block: vec![0.0; hop * source_count],
            lfe: vec![0.0; hop],
        }
    }

    fn fill_buffer(&mut self, start: i64) -> Result<()> {
        let retained = match self.buf_start.map(|previous| start - previous) {
            Some(advance) if (0..=self.buf_frames as i64).contains(&advance) => {
                self.buf.copy_within(advance as usize * self.channels.., 0);
                self.buf_frames - advance as usize
            }
            _ => 0,
        };
        self.buf[retained * self.channels..].fill(0.0);
        let read_start = (start + retained as i64).max(0) as u64;
        let read_end = (start + self.buf_frames as i64).clamp(0, self.total_frames as i64) as u64;
        if read_end > read_start {
            self.caf.seek_to_frame(read_start)?;
            let mut raw = self.caf.read_frames((read_end - read_start) as usize)?;
            ensure!(
                raw.len() == (read_end - read_start) as usize * self.channels,
                "CAF essence ended before the requested audio block"
            );
            for (frame, row) in raw.chunks_exact_mut(self.channels).enumerate() {
                let sample = read_start + frame as u64;
                for source in &self.sources {
                    row[source.channel] *= source.track.step_at(sample).gain;
                }
            }
            let offset = (read_start as i64 - start) as usize * self.channels;
            self.buf[offset..offset + raw.len()].copy_from_slice(&raw);
        }
        self.buf_start = Some(start);
        Ok(())
    }

    /// Fill the analysis window and the current hop block. Returns the block length.
    fn read_frame(&mut self, frame: usize) -> Result<usize> {
        let start = frame as u64 * self.hop as u64;
        ensure!(start < self.total_frames, "frame past the end");
        let length = (self.total_frames - start).min(self.hop as u64) as usize;
        let center = start as i64 + length as i64 / 2;
        let buffer_start = center - self.buf_frames as i64 / 2;
        self.fill_buffer(buffer_start)?;

        let n_sources = self.sources.len();
        self.block.fill(0.0);
        self.lfe.fill(0.0);

        let window_offset = self.buf_frames / 2 - self.n_fft / 2;
        for frame in 0..self.n_fft {
            let src = (window_offset + frame) * self.channels;
            for (index, source) in self.sources.iter().enumerate() {
                self.window[index * self.n_fft + frame] = self.buf[src + source.channel];
            }
        }
        let block_offset = (start as i64 - buffer_start) as usize;
        for offset in 0..length {
            let src = (block_offset + offset) * self.channels;
            for (index, source) in self.sources.iter().enumerate() {
                self.block[offset * n_sources + index] = f64::from(self.buf[src + source.channel]);
            }
            if let Some(channel) = self.lfe_channel {
                self.lfe[offset] = f64::from(self.buf[src + channel]);
            }
        }
        Ok(length)
    }
}

struct FrameInput {
    window: Vec<f32>,
    block: Vec<f64>,
    lfe: Vec<f64>,
    length: usize,
}

impl SourceReader {
    fn take_frame(&mut self, frame: usize) -> Result<FrameInput> {
        let length = self.read_frame(frame)?;
        let window =
            std::mem::replace(&mut self.window, vec![0.0; self.n_fft * self.sources.len()]);
        let block = std::mem::replace(&mut self.block, vec![0.0; self.hop * self.sources.len()]);
        let lfe = std::mem::replace(&mut self.lfe, vec![0.0; self.hop]);
        Ok(FrameInput {
            window,
            block,
            lfe,
            length,
        })
    }
}

#[derive(Clone)]
struct ClusterState {
    position: [f64; 3],
    active: bool,
    size: f64,
    importance: f64,
    dialog: f64,
    music: f64,
    zones: String,
}

struct MetadataCollector {
    hop: usize,
    interval: usize,
    position_threshold: f64,
    importance_threshold: f64,
    max_interval: usize,
    last: Vec<Option<ClusterState>>,
    last_time: Vec<i64>,
    zones: Vec<String>,
    events: Vec<DamfEvent>,
}

impl MetadataCollector {
    fn new(hop: usize, interval_frames: usize, n_clusters: usize) -> Self {
        let interval = interval_frames.max(1);
        let position_threshold = if interval <= 1 { 1e-5 } else { 0.02 };
        let importance_threshold = if interval <= 1 { 1e-4 } else { 0.05 };
        let max_interval = if interval > 1 { interval * 2 } else { 0 };
        // The rendered layout always starts with LFE, including silent padding.
        let events = vec![DamfEvent {
            id: 3,
            sample_pos: 0,
            active: Some(true),
            position: None,
            snap: None,
            elevation: None,
            zones: None,
            size: None,
            decorr: None,
            importance: Some(1.0),
            gain: Some(Gain::Db(0.0)),
            ramp_length: Some(1536),
            trim_bypass: Some(false),
            dialog: None,
            music: None,
            screen_factor: None,
            depth_factor: None,
            head_track_mode: Some("undefined".to_string()),
            binaural_render_mode: Some("off".to_string()),
        }];
        Self {
            hop,
            interval,
            position_threshold,
            importance_threshold,
            max_interval,
            last: vec![None; n_clusters],
            last_time: vec![-1; n_clusters],
            zones: vec!["all".to_string(); n_clusters],
            events,
        }
    }

    fn push(&mut self, frame: usize, metadata: &crate::clustering::ClusterFrameMetadata) {
        if frame == 0 && !metadata.zones.is_empty() {
            self.zones = metadata.zones.clone();
        }
        let sample_pos = (frame as u64) * self.hop as u64;
        for cluster in 0..metadata.positions.len() {
            let state = ClusterState {
                position: metadata.positions[cluster],
                active: metadata.active[cluster],
                size: metadata.sizes[cluster],
                importance: metadata.importance[cluster],
                dialog: metadata.dialog[cluster],
                music: metadata.music[cluster],
                zones: self.zones[cluster].clone(),
            };
            let previous = self.last[cluster].as_ref();
            let elapsed = sample_pos as i64 - self.last_time[cluster];
            let changed = match previous {
                None => true,
                Some(previous) => {
                    let active_changed = state.active != previous.active;
                    let distance = {
                        let dx = state.position[0] - previous.position[0];
                        let dy = state.position[1] - previous.position[1];
                        let dz = state.position[2] - previous.position[2];
                        (dx * dx + dy * dy + dz * dz).sqrt()
                    };
                    let significant = active_changed
                        || (state.size - previous.size).abs() > 1e-5
                        || (state.importance - previous.importance).abs()
                            > self.importance_threshold
                        || (state.dialog - previous.dialog).abs() > 1e-4
                        || (state.music - previous.music).abs() > 1e-4
                        || state.zones != previous.zones
                        || distance > self.position_threshold;
                    let due = elapsed >= (self.interval * self.hop) as i64;
                    let forced =
                        self.max_interval > 0 && elapsed >= (self.max_interval * self.hop) as i64;
                    (significant && due) || forced || (active_changed && elapsed >= self.hop as i64)
                }
            };
            if !changed {
                continue;
            }
            let x = state.position[0].clamp(-1.0, 1.0) as f32;
            let y = state.position[1].clamp(-1.0, 1.0) as f32;
            let z = state.position[2].clamp(0.0, 1.0) as f32;
            self.events.push(DamfEvent {
                id: 10 + cluster as i64,
                sample_pos,
                active: Some(state.active),
                position: Some([x, y, z]),
                snap: Some(false),
                elevation: Some(true),
                zones: Some(state.zones.clone()),
                size: Some(state.size as f32),
                decorr: Some(0),
                importance: Some(state.importance as f32),
                gain: Some(Gain::Db(0.0)),
                ramp_length: Some(1536),
                trim_bypass: Some(false),
                dialog: Some(state.dialog as f32),
                music: Some(state.music as f32),
                screen_factor: Some(0.0),
                depth_factor: Some(0.25),
                head_track_mode: Some("undefined".to_string()),
                binaural_render_mode: Some("undefined".to_string()),
            });
            self.last[cluster] = Some(state);
            self.last_time[cluster] = sample_pos as i64;
        }
    }

    fn finish(self, sample_rate: u32) -> Metadata {
        Metadata {
            sample_rate,
            events: self.events,
        }
    }
}

fn unique_temp_dir(settings: &ClusterSettings) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let base = settings.workdir.clone().unwrap_or_else(std::env::temp_dir);
    base.join(format!("cluster_atmos_{}_{}", std::process::id(), nonce))
}

fn scan_peak(path: &Path, channels: usize) -> Result<f64> {
    let mut reader = BufReader::new(File::open(path)?);
    let chunk_samples = 65_536usize * channels;
    let mut bytes = vec![0u8; chunk_samples * 4];
    let mut peak = 0.0f64;
    loop {
        let read = reader.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        for chunk in bytes[..read].as_chunks::<4>().0 {
            let value = f32::from_le_bytes(*chunk);
            peak = peak.max(f64::from(value).abs());
        }
    }
    Ok(peak)
}

fn convert_to_caf(
    raw: &Path,
    caf: &Path,
    sample_rate: u32,
    channels: usize,
    total_frames: u64,
    scale: f64,
    little_endian: bool,
) -> Result<()> {
    let mut reader = BufReader::new(File::open(raw)?);
    let mut writer = CafWriter::create(caf, sample_rate, channels, total_frames, little_endian)?;
    let chunk_frames = 65_536usize;
    let mut bytes = vec![0u8; chunk_frames * channels * 4];
    loop {
        let read = reader.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        ensure!(
            read % 4 == 0,
            "raw f32 stream ended on a partial sample ({} bytes)",
            read
        );
        let mut samples = Vec::with_capacity(read / 4);
        for chunk in bytes[..read].as_chunks::<4>().0 {
            let value = f32::from_le_bytes(*chunk);
            samples.push((f64::from(value) * scale) as f32);
        }
        writer.write_frames(&samples)?;
    }
    Ok(writer.finish()?)
}

#[allow(clippy::too_many_arguments)]
pub fn run_cluster(
    source: &Path,
    out: &Path,
    settings: &ClusterSettings,
    stem: Option<&str>,
    progress_every: usize,
) -> Result<ClusterResult> {
    settings.validate()?;
    let source = source.to_path_buf();
    let manifest = Manifest::load(&source)?;
    let metadata = Metadata::load(&manifest.metadata)?;
    let caf = CafReader::open(&manifest.audio)?;
    let sample_rate = caf.info().sample_rate;
    ensure!(
        caf.info().channels == manifest.beds.len() + manifest.objects.len(),
        "CAF and manifest channel counts differ"
    );
    let little_endian = !caf.info().big_endian;
    if metadata.sample_rate != sample_rate {
        log::warn!(
            "CAF rate {sample_rate} != metadata sampleRate {}; using CAF rate",
            metadata.sample_rate
        );
    }
    let total_frames = caf
        .total_frames()
        .ok_or_else(|| anyhow::anyhow!("CAF essence has no known frame count"))?;
    let total_frames = if settings.max_seconds > 0.0 {
        total_frames.min((settings.max_seconds * f64::from(sample_rate)).round() as u64)
    } else {
        total_frames
    };
    ensure!(
        total_frames > 0,
        "no audio frames in {}",
        manifest.audio.display()
    );

    let source_set = build_sources(&manifest, &metadata);
    ensure!(!source_set.sources.is_empty(), "no object sources");
    let n_sources = source_set.n_sources();
    let n_clusters = settings.max_objects.min(n_sources);
    let n_frames = total_frames.div_ceil(settings.hop as u64) as usize;

    log::info!(
        "master: {} ch, {n_sources} sources ({} fixed-position), {} samples ({:.2}s), target {n_clusters}",
        caf.info().channels,
        source_set
            .sources
            .iter()
            .filter(|source| source.is_fixed())
            .count(),
        total_frames,
        total_frames as f64 / f64::from(sample_rate),
    );
    let mut vad_bank = if settings.use_vad && n_clusters < n_sources {
        Some(VadBank::new(
            n_sources,
            settings.vad_smooth_ms,
            settings.hop,
            sample_rate,
        )?)
    } else {
        None
    };

    let tracks = sample_tracks(
        &source_set.sources,
        n_frames,
        settings.hop,
        total_frames,
        settings.dialog_boost_db,
    );
    let fixed_mask: Vec<bool> = source_set
        .sources
        .iter()
        .map(|source| source.is_fixed() && !settings.beds_as_objects)
        .collect();

    let loudness = Loudness::new(sample_rate, settings.window_ms, settings.db_max)?;
    let n_fft = loudness.n_fft;
    let mut renderer =
        ClusterRenderer::new(sample_rate, settings, tracks.zones.clone(), fixed_mask);
    let mut collector =
        MetadataCollector::new(settings.hop, settings.metadata_interval_frames, n_clusters);
    let mut reader = SourceReader::new(
        caf,
        source_set.sources,
        source_set.lfe_channel,
        n_fft,
        settings.hop,
        total_frames,
    );

    let temp_dir = unique_temp_dir(settings);
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("creating {}", temp_dir.display()))?;
    let raw_path = temp_dir.join("clusters.f32");
    let mut raw = BufWriter::new(
        File::create(&raw_path).with_context(|| format!("creating {}", raw_path.display()))?,
    );

    let started = std::time::Instant::now();
    let mut timing_read = 0.0;
    let mut timing_levels = 0.0;
    let mut timing_excitation = 0.0;
    let mut timing_render = 0.0;
    let mut timing_write = 0.0;
    let mut timing_vad = 0.0;

    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<FrameInput>>(2);
    let producer = std::thread::spawn(move || {
        for frame in 0..n_frames {
            let input = reader.take_frame(frame);
            let failed = input.is_err();
            if tx.send(input).is_err() || failed {
                break;
            }
        }
    });

    for frame in 0..n_frames {
        let stamp = std::time::Instant::now();
        let input = rx
            .recv()
            .map_err(|_| anyhow::anyhow!("clustering reader thread stopped early"))??;
        timing_read += stamp.elapsed().as_secs_f64();
        let stamp = std::time::Instant::now();
        let levels = loudness.band_levels(&input.window, n_sources);
        timing_levels += stamp.elapsed().as_secs_f64();
        let stamp = std::time::Instant::now();
        let mut excitation = loudness.excitation(&levels);
        timing_excitation += stamp.elapsed().as_secs_f64();
        let stamp = std::time::Instant::now();
        let (vad_gain, vad_importance, vad_dialog) = if let Some(vad) = vad_bank.as_mut() {
            let probabilities = vad.advance(&input.block, input.length, n_sources)?;
            let importance = content_importance(&probabilities, settings.vad_threshold, 8.0, 2.0);
            let gain = content_gain(&importance, settings.vad_gain_db);
            (Some(gain), Some(importance), Some(probabilities))
        } else {
            (None, None, None)
        };
        timing_vad += stamp.elapsed().as_secs_f64();
        let position_start = frame * n_sources;
        let features = FrameFeatures {
            positions: &tracks.positions[position_start..position_start + n_sources],
            active: &tracks.active[position_start..position_start + n_sources],
            sizes: &tracks.sizes[position_start..position_start + n_sources],
            content_prior: &tracks.priorities[position_start..position_start + n_sources],
            dialog: &tracks.dialog[position_start..position_start + n_sources],
            music: &tracks.music[position_start..position_start + n_sources],
            content_gain: vad_gain.as_deref(),
            content_importance: vad_importance.as_deref(),
            dialog_probability: vad_dialog.as_deref(),
        };
        let block = &input.block[..input.length * n_sources];
        let lfe = if source_set.lfe_channel.is_some() {
            Some(&input.lfe[..input.length])
        } else {
            None
        };
        let stamp = std::time::Instant::now();
        let clustered = renderer.render(&loudness, &mut excitation, &features, block, lfe)?;
        timing_render += stamp.elapsed().as_secs_f64();
        let stamp = std::time::Instant::now();
        let mut bytes = Vec::with_capacity(clustered.audio.len() * 4);
        for sample in &clustered.audio {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        raw.write_all(&bytes)?;
        collector.push(frame, &clustered.metadata);
        timing_write += stamp.elapsed().as_secs_f64();

        if progress_every > 0 && (frame % progress_every == 0 || frame + 1 == n_frames) {
            println!(
                "  frame {}/{}  k={}/{}  elapsed {:.1}s",
                frame + 1,
                n_frames,
                clustered.selected.len(),
                settings.max_objects,
                started.elapsed().as_secs_f64()
            );
        }
    }
    drop(rx);
    producer
        .join()
        .map_err(|_| anyhow::anyhow!("clustering reader thread panicked"))?;
    raw.flush()?;
    drop(raw);
    if std::env::var_os("SALSA_CLUSTER_TIMING").is_some() {
        println!(
            "timing: read {:.2}s  levels {:.2}s  excitation {:.2}s  render {:.2}s  write {:.2}s",
            timing_read, timing_levels, timing_excitation, timing_render, timing_write
        );
        println!("vad: {:.2}s", timing_vad);
        println!(
            "render: partial {:.2}s  select {:.2}s  gains {:.2}s  output {:.2}s  stats {:.2}s",
            renderer.timing_partial,
            renderer.timing_select,
            renderer.timing_gains,
            renderer.timing_output,
            renderer.timing_stats
        );
    }

    let peak = scan_peak(&raw_path, 1 + n_clusters)?;
    let scale = if settings.limit == LimitMode::Peak && peak > 0.99 {
        0.99 / peak
    } else {
        1.0
    };
    if scale != 1.0 {
        println!(
            "peak {peak:.3} -> scaled by {:+.2} dB",
            20.0 * scale.log10()
        );
    } else if peak > 1.0 {
        println!("warning: peak {peak:.3} > 1.0; use --limit peak or check source levels");
    }

    let out_base = out.to_path_buf();
    let (out_dir, out_stem, out_manifest) = if out_base
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("atmos"))
    {
        let name = out_base
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("clustered.atmos");
        let stem = name
            .strip_suffix(".atmos")
            .or_else(|| name.strip_suffix(".ATMOS"))
            .unwrap_or(name)
            .to_string();
        let dir = out_base
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        (dir, stem, out_base.clone())
    } else {
        let dir = out_base;
        let stem = stem
            .map(ToOwned::to_owned)
            .or_else(|| {
                source
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| "clustered".to_string());
        let manifest = dir.join(format!("{stem}.atmos"));
        (dir, stem, manifest)
    };
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let out_metadata = out_dir.join(format!("{out_stem}.atmos.metadata"));
    let out_audio = out_dir.join(format!("{out_stem}.atmos.audio"));

    let beds = [BedEntry {
        id: 3,
        name: "LFE".to_string(),
    }];
    let objects: Vec<ObjectEntry> = (0..n_clusters)
        .map(|index| ObjectEntry {
            id: 10 + index as i64,
            description: None,
        })
        .collect();
    let audio_name = format!("{out_stem}.atmos.audio");
    let metadata_name = format!("{out_stem}.atmos.metadata");
    write_manifest(
        &out_manifest,
        &ManifestSpec {
            audio_name: &audio_name,
            metadata_name: &metadata_name,
            offset: 0.0,
            fps: manifest.fps,
            sc_bed_configuration: &[3],
            creation_tool: "salsa-atmos-cluster",
            creation_tool_version: "0.1.0",
            beds: &beds,
            objects: &objects,
        },
    )?;
    collector.finish(sample_rate).save(&out_metadata)?;
    convert_to_caf(
        &raw_path,
        &out_audio,
        sample_rate,
        1 + n_clusters,
        total_frames,
        scale,
        little_endian,
    )?;

    let stats = renderer.stats().clone();
    println!(
        "wrote {} ({} dynamic clusters + {}, {:.2}s)",
        out_manifest.display(),
        n_clusters,
        if source_set.lfe_channel.is_some() {
            "LFE"
        } else {
            "silent LFE"
        },
        total_frames as f64 / f64::from(sample_rate)
    );
    if stats.err_weight > 0.0 {
        println!(
            "spatial: importance-weighted nearest-centroid error mean {:.4} room units, max {:.4}",
            stats.error_mean(),
            stats.err_max
        );
    }
    if stats.effective_err_weight > 0.0 {
        println!(
            "spatial: importance-weighted gain-rendered error mean {:.4} room units, max {:.4}",
            stats.effective_error_mean(),
            stats.effective_err_max
        );
    }
    if stats.occ_frames > 0 {
        let occupancy = stats.occupancy_mean();
        println!(
            "clusters: mean source occupancy {}  (total {:.2})",
            occupancy
                .iter()
                .map(|value| format!("{value:.2}"))
                .collect::<Vec<_>>()
                .join(" "),
            occupancy.iter().sum::<f64>()
        );
    }

    if settings.keep_workdir {
        println!("workdir kept: {}", temp_dir.display());
    } else {
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    Ok(ClusterResult {
        source,
        manifest: out_manifest,
        metadata: out_metadata,
        audio: out_audio,
        output_dir: out_dir,
        stem: out_stem,
        clusters: n_clusters,
        sample_rate,
        total_frames,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::CentroidScore;
    use objectaudio_damf::DamfSource;

    struct TestMaster {
        dir: PathBuf,
        manifest: Manifest,
    }

    impl TestMaster {
        fn new(rate: u32, channels: &[Vec<f32>], has_lfe: bool, events: Vec<DamfEvent>) -> Self {
            let dir = unique_temp_dir(&ClusterSettings::default());
            std::fs::create_dir_all(&dir).unwrap();
            let beds: Vec<_> = has_lfe
                .then(|| BedEntry {
                    id: 3,
                    name: "LFE".to_string(),
                })
                .into_iter()
                .collect();
            let objects: Vec<_> = (0..channels.len() - beds.len())
                .map(|index| ObjectEntry {
                    id: 10 + index as i64,
                    description: None,
                })
                .collect();
            let path = dir.join("source.atmos");
            write_manifest(
                &path,
                &ManifestSpec {
                    audio_name: "source.atmos.audio",
                    metadata_name: "source.atmos.metadata",
                    offset: 0.0,
                    fps: 24.0,
                    sc_bed_configuration: if has_lfe { &[3] } else { &[] },
                    creation_tool: "salsa-cluster-test",
                    creation_tool_version: "0.1.0",
                    beds: &beds,
                    objects: &objects,
                },
            )
            .unwrap();
            let manifest = Manifest::load(&path).unwrap();
            Metadata {
                sample_rate: rate,
                events,
            }
            .save(&manifest.metadata)
            .unwrap();
            let frames = channels[0].len();
            assert!(channels.iter().all(|channel| channel.len() == frames));
            let samples: Vec<_> = (0..frames)
                .flat_map(|frame| channels.iter().map(move |channel| channel[frame]))
                .collect();
            let mut writer =
                CafWriter::create(&manifest.audio, rate, channels.len(), frames as u64, true)
                    .unwrap();
            writer.write_frames(&samples).unwrap();
            writer.finish().unwrap();
            Self { dir, manifest }
        }

        fn sources(&self) -> crate::timeline::SourceSet {
            build_sources(
                &self.manifest,
                &Metadata::load(&self.manifest.metadata).unwrap(),
            )
        }

        fn reader(&self, n_fft: usize, hop: usize) -> SourceReader {
            let sources = self.sources();
            let caf = CafReader::open(&self.manifest.audio).unwrap();
            let frames = caf.total_frames().unwrap();
            SourceReader::new(
                caf,
                sources.sources,
                sources.lfe_channel,
                n_fft,
                hop,
                frames,
            )
        }

        fn cluster(&self, settings: &ClusterSettings) -> ClusterResult {
            run_cluster(
                &self.manifest.path,
                &self.dir,
                settings,
                Some("clustered"),
                0,
            )
            .unwrap()
        }
    }

    impl Drop for TestMaster {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn event(id: i64, sample_pos: u64, x: f32, gain: Gain, ramp: u32) -> DamfEvent {
        DamfEvent {
            id,
            sample_pos,
            active: Some(true),
            position: Some([x, 0.0, 0.0]),
            gain: Some(gain),
            ramp_length: Some(ramp),
            snap: None,
            elevation: None,
            zones: None,
            size: None,
            decorr: None,
            importance: None,
            trim_bypass: None,
            dialog: None,
            music: None,
            screen_factor: None,
            depth_factor: None,
            head_track_mode: None,
            binaural_render_mode: None,
        }
    }

    fn signal(frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|frame| ((frame % 64) as f32 - 32.0) / 256.0)
            .collect()
    }

    fn decode(result: &ClusterResult) -> Vec<f32> {
        let mut reader = CafReader::open(&result.audio).unwrap();
        reader.read_frames(result.total_frames as usize).unwrap()
    }

    #[test]
    fn reader_covers_short_long_and_odd_windows_without_reapplying_gain() {
        for (n_fft, hop) in [
            (2, 5),
            (3, 6),
            (5, 5),
            (6, 5),
            (7, 4),
            (8, 3),
            (1024, 1536),
            (768, 1536),
            (3072, 1536),
        ] {
            let frames = hop * 3 + 1;
            let samples = signal(frames);
            let gain = Gain::Db(-6.0);
            let events = vec![
                event(10, 0, -1.0, Gain::Db(0.0), 0),
                event(10, 2, -1.0, gain, 0),
                event(10, (2 * hop + 1) as u64, -1.0, Gain::MinusInfinity, 0),
            ];
            let master = TestMaster::new(48_000, &[samples.clone(), samples.clone()], true, events);
            let mut reader = master.reader(n_fft, hop);
            let expected = |sample: i64| {
                if sample < 0 || sample >= frames as i64 {
                    return 0.0;
                }
                let index = sample as usize;
                let gain = if index < 2 {
                    1.0
                } else if index < 2 * hop + 1 {
                    gain.linear()
                } else {
                    0.0
                };
                samples[index] * gain
            };
            // Repeated and backward reads also exercise the buffer reset path.
            for frame in [0, 1, 2, 3, 3, 0, 2, 1] {
                let input = reader.take_frame(frame).unwrap();
                let start = frame * hop;
                let length = (frames - start).min(hop);
                assert_eq!(input.length, length);
                for offset in 0..length {
                    assert_eq!(
                        input.block[offset],
                        f64::from(expected((start + offset) as i64))
                    );
                    assert_eq!(input.lfe[offset], f64::from(samples[start + offset]));
                }
                assert!(input.block[length..].iter().all(|sample| *sample == 0.0));
                let window_start = start as i64 + length as i64 / 2 - n_fft as i64 / 2;
                for offset in 0..n_fft {
                    assert_eq!(
                        input.window[offset],
                        expected(window_start + offset as i64),
                        "window {n_fft}, hop {hop}, frame {frame}, offset {offset}"
                    );
                }
            }
        }
    }

    #[test]
    fn object_gain_is_baked_into_pcm_and_missing_lfe_is_declared() {
        let samples = signal(4097);
        let positive = Gain::Db(6.0);
        let negative = Gain::Db(-6.0);
        let master = TestMaster::new(
            48_000,
            std::slice::from_ref(&samples),
            false,
            vec![
                event(10, 0, -1.0, positive, 0),
                event(10, 1701, -1.0, negative, 0),
                event(10, 3201, -1.0, Gain::MinusInfinity, 0),
            ],
        );
        let result = master.cluster(&ClusterSettings {
            limit: LimitMode::None,
            ..ClusterSettings::default()
        });
        let output_manifest = Manifest::load(&result.manifest).unwrap();
        assert_eq!(
            output_manifest.beds,
            vec![BedEntry {
                id: 3,
                name: "LFE".to_string()
            }]
        );
        assert_eq!(output_manifest.objects.len(), 1);
        let output_metadata = Metadata::load(&result.metadata).unwrap();
        assert!(output_metadata.events.iter().any(|event| event.id == 3));
        assert!(
            output_metadata
                .events
                .iter()
                .all(|event| event.gain == Some(Gain::Db(0.0)))
        );
        let mut output = DamfSource::open(&result.manifest).unwrap();
        assert_eq!(output.info().source_channels, 2);
        let block = output.read_block(samples.len()).unwrap().unwrap();
        for (index, row) in block.pcm.as_chunks::<2>().0.iter().enumerate() {
            let gain = if index < 1701 {
                positive.linear()
            } else if index < 3201 {
                negative.linear()
            } else {
                0.0
            };
            assert_eq!(row[0], 0.0);
            assert!(
                (row[1] - samples[index] * gain).abs() < 2e-7,
                "sample {index}"
            );
        }
    }

    #[test]
    fn muted_raw_audio_matches_silent_input_in_analysis_and_mixing() {
        let frames = 4097;
        let audible = signal(frames);
        let muted: Vec<_> = audible.iter().map(|value| value * 4.0).collect();
        let events = vec![
            event(10, 0, -1.0, Gain::MinusInfinity, 0),
            event(11, 0, 1.0, Gain::Db(0.0), 0),
        ];
        let original = TestMaster::new(48_000, &[muted, audible.clone()], false, events.clone());
        let reference = TestMaster::new(48_000, &[vec![0.0; frames], audible], false, events);
        for centroid_score in [CentroidScore::Loudness, CentroidScore::Reconstruction] {
            let settings = ClusterSettings {
                max_objects: 1,
                centroid_score,
                ..ClusterSettings::default()
            };
            let actual = original.cluster(&settings);
            let expected = reference.cluster(&settings);
            assert_eq!(decode(&actual), decode(&expected));
            assert_eq!(
                Metadata::load(actual.metadata).unwrap(),
                Metadata::load(expected.metadata).unwrap()
            );
        }
    }

    #[test]
    fn short_windows_preserve_every_output_sample() {
        for (rate, window_ms) in [(16_000, 64.0), (48_000, 16.0)] {
            for frames in [1, 1537, 4613] {
                let samples = signal(frames);
                let master = TestMaster::new(
                    rate,
                    &[samples.clone(), samples.clone()],
                    true,
                    vec![event(10, 0, 0.0, Gain::Db(0.0), 0)],
                );
                let result = master.cluster(&ClusterSettings {
                    window_ms,
                    ..ClusterSettings::default()
                });
                assert_eq!(result.total_frames, frames as u64);
                let decoded = decode(&result);
                assert_eq!(decoded.len(), frames * 2);
                for (row, expected) in decoded.as_chunks::<2>().0.iter().zip(&samples) {
                    assert_eq!(row, &[*expected, *expected]);
                }
            }
        }
    }

    #[test]
    fn sampled_positions_hold_then_follow_each_event_ramp() {
        let events = vec![
            event(10, 0, -1.0, Gain::Db(0.0), 0),
            event(10, 6144, 0.5, Gain::Db(-6.0), 1536),
            event(10, 6144, 1.0, Gain::Db(-6.0), 1536),
            event(10, 6912, -1.0, Gain::Db(-6.0), 3072),
            event(10, 9984, 1.0, Gain::Db(0.0), 0),
        ];
        let master = TestMaster::new(48_000, &[signal(11_000)], false, events);
        let sources = master.sources();
        let tracks = sample_tracks(&sources.sources, 29, 384, 11_000, 0.0);
        for (sample, x) in [
            (0, -1.0),
            (3072, -1.0),
            (6144, -1.0),
            (6528, -0.5),
            (6912, 0.0),
            (7680, -0.25),
            (9600, -0.875),
            (9984, 1.0),
            (10_752, 1.0),
        ] {
            assert_eq!(
                tracks.positions[sample / 384],
                [x, 0.0, 0.0],
                "sample {sample}"
            );
        }
        let mut source = DamfSource::open(&master.manifest.path).unwrap();
        for frame in 0..tracks.n_frames {
            source.seek(frame as u64 * 384).unwrap();
            let block = source.read_block(1).unwrap().unwrap();
            assert_eq!(
                tracks.positions[frame],
                block.payload.objects[0].position.map(f64::from)
            );
        }
    }
}
