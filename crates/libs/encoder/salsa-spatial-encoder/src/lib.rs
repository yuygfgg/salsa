//! DAMF to salsa-spatial encoder.
//!
//! The encoder resolves a DAMF presentation, applies authored object gain and
//! activity exactly once at audio rate, quantizes and simplifies object
//! trajectories, forms the deterministic 5.1.2 core, encodes five Opus
//! substreams, locally decodes them for measured error, and runs a fixed-weight
//! `D + beta*R` parameter search. Output is an Ogg Opus multistream track written through
//! `salsa-spatial`'s shared writer.

mod core_codec;
mod downmix;
mod measurement;
mod objective;
mod optimize;
mod rd;
pub mod source;
mod trajectory;

pub use core_codec::MultistreamCodec;
pub use downmix::TransportSignals;
pub use source::{SourceReader, TileSource};

use salsa_spatial::{BaseFold, CorePannerConfig};
use salsa_spatial_dsp::CorePanner;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use objectaudio_ir::ObjectState;
use salsa_spatial::params::{ParameterTile, SceneParameterEncoder};
use salsa_spatial::{
    CHECKPOINT_SAMPLES, EnhancementTile, MAX_OBJECTS, MultistreamPacket, PARAM_BAND_COUNT,
    SAMPLE_RATE, SalsaSpatialFrame, SalsaSpatialStreamConfig, TILE_SAMPLES, TrajectoryKnot,
};
use serde::Serialize;

use crate::downmix::build_transport;
use crate::measurement::MeasurementContext;
use crate::rd::search_tile;
use crate::trajectory::{fit_trajectory, position_at};

/// Encoder options.
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    pub master: PathBuf,
    pub output: PathBuf,
    pub core_bitrates: [u32; 5],
    pub beta: f64,
    pub max_frames: Option<usize>,
    pub reference_start: u64,
    pub headroom_db: Option<f64>,
    /// Minimum horizontal panner gain at the ceiling, in [0, 1).
    pub ceiling_horizontal_gain: f64,
}

/// The encoder pass currently processing source samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeStage {
    Headroom,
    Encoding,
}

/// Progress within one pass, relative to the selected source interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeProgress {
    pub stage: EncodeStage,
    pub completed_samples: u64,
    pub total_samples: u64,
}

/// Per-tile accounting from actual serialized output.
#[derive(Debug, Clone, Serialize)]
pub struct TileReport {
    pub start_sample: u64,
    pub core_bytes: u64,
    pub parameter_bytes: u64,
    pub checkpoint_bytes: u64,
    pub configuration_bytes: u64,
    pub other_scene_bytes: u64,
    pub metadata_bytes: u64,
    pub framing_bytes: u64,
    pub transport_bytes: u64,
    pub object_count: usize,
    pub active_objects: usize,
}

/// Whole-program report.
#[derive(Debug, Clone, Serialize)]
pub struct EncodeReport {
    pub ceiling_horizontal_gain: f64,
    pub headroom_gain: f64,
    pub core_bytes: u64,
    pub parameter_bytes: u64,
    pub checkpoint_bytes: u64,
    pub configuration_bytes: u64,
    pub other_scene_bytes: u64,
    pub metadata_bytes: u64,
    pub framing_bytes: u64,
    pub transport_bytes: u64,
    pub total_bytes: u64,
    pub duration_samples: u64,
    pub core_bitrate: f64,
    pub metadata_bitrate: f64,
    pub total_bitrate: f64,
    pub per_tile_metadata: Vec<TileReport>,
    pub peak_metadata_bitrate: f64,
    pub object_count: usize,
    pub opus_wrapper: &'static str,
    pub opus_native: String,
    /// Unsupported authored properties reported by the DAMF conversion path.
    pub warnings: Vec<String>,
}

struct TileJob {
    source: TileSource,
    core_knots: Vec<Vec<TrajectoryKnot>>,
    meta_knots: Vec<Vec<TrajectoryKnot>>,
    core: [Vec<f32>; 7],
    packets: Vec<MultistreamPacket>,
}

struct ProcessedTile {
    frame: SalsaSpatialFrame,
    core_bytes: u64,
    active_objects: usize,
    frames: usize,
}

fn checkpoint_due(start_sample: u64, program_start: u64) -> bool {
    start_sample == program_start
        || start_sample.saturating_sub(program_start) % CHECKPOINT_SAMPLES == 0
}

fn bitrate(bytes: u64, samples: u64) -> f64 {
    if samples == 0 {
        return 0.0;
    }
    bytes as f64 * 8.0 * f64::from(SAMPLE_RATE) / samples as f64
}

fn quantize_gain(gain: f64) -> f64 {
    ((gain * 4096.0).round() / 4096.0).clamp(1.0 / 4096.0, 4.0)
}

fn measure_downmix_peak(
    manifest: &std::path::Path,
    start: u64,
    duration: u64,
    panner: CorePanner,
    progress: &mut impl FnMut(EncodeProgress),
) -> Result<f64> {
    progress(EncodeProgress {
        stage: EncodeStage::Headroom,
        completed_samples: 0,
        total_samples: duration,
    });
    let mut reader = SourceReader::open(manifest)?;
    reader.seek(start)?;
    let mut peak = 0.0f64;
    let mut remaining = duration;
    let mut last_positions = vec![[0.0; 3]; reader.info().objects.len()];
    let mut first_tile = true;
    while remaining > 0 {
        let frames = remaining.min(TILE_SAMPLES as u64) as usize;
        let Some(tile) = reader.read_tile(frames)? else {
            break;
        };
        let (knots, _) = fit_tile_knots(&tile, &mut last_positions, first_tile, panner);
        first_tile = false;
        peak = peak.max(build_transport(&tile, &knots, 1.0, panner, BaseFold::v1(), 2).peak());
        remaining -= tile.frames as u64;
        progress(EncodeProgress {
            stage: EncodeStage::Headroom,
            completed_samples: duration - remaining,
            total_samples: duration,
        });
    }
    Ok(peak)
}

fn choose_headroom(
    options: &EncodeOptions,
    duration: u64,
    panner: CorePanner,
    progress: &mut impl FnMut(EncodeProgress),
) -> Result<f64> {
    let gain = if let Some(db) = options.headroom_db {
        let gain = 10f64.powf(db / 20.0);
        if !gain.is_finite() || gain <= 0.0 {
            bail!("--headroom-db must describe a positive linear gain");
        }
        gain
    } else {
        let peak = measure_downmix_peak(
            &options.master,
            options.reference_start,
            duration,
            panner,
            progress,
        )
        .context("headroom prepass")?;
        let target_peak = 10f64.powf(-1.0 / 20.0);
        if peak > target_peak {
            target_peak / peak
        } else {
            1.0
        }
    };
    Ok(quantize_gain(gain))
}

fn fit_tile_knots(
    source: &TileSource,
    last_positions: &mut [[f64; 3]],
    first_tile: bool,
    panner: CorePanner,
) -> (Vec<Vec<TrajectoryKnot>>, Vec<Vec<TrajectoryKnot>>) {
    let object_count = source.object_count;
    let mut core = Vec::with_capacity(object_count);
    let mut metadata = Vec::with_capacity(object_count);
    let mut control_samples = Vec::new();
    for event in &source.events {
        let sample = source.start_sample + event.offset.max(0) as u64;
        control_samples.push(sample);
    }
    for object in 0..object_count {
        let (knots, dense, is_static) = fit_trajectory(
            source.start_sample,
            source.frames,
            object_count,
            object,
            &source.positions,
            &control_samples,
            panner,
        );
        let end_position = dense
            .last()
            .map(|(_, position)| *position)
            .unwrap_or(last_positions[object]);
        let needs_update = first_tile || !is_static || end_position != last_positions[object];
        if !knots.is_empty() {
            core.push(knots.clone());
            if needs_update {
                metadata.push(knots);
            } else {
                metadata.push(Vec::new());
            }
        } else {
            core.push(Vec::new());
            metadata.push(Vec::new());
        }
        last_positions[object] = end_position;
    }
    (core, metadata)
}

fn initial_objects(source: &TileSource, knots: &[Vec<TrajectoryKnot>]) -> Vec<ObjectState> {
    (0..source.object_count)
        .map(|object| {
            let position = position_at(&knots[object], source.start_sample as i64);
            ObjectState {
                id: object as u32 + 1,
                position: position.map(|value| value as f32),
                gain: 1.0,
                enabled: source.is_active(0, object),
                size: None,
                spread: None,
            }
        })
        .collect()
}

fn make_config(
    source: &TileSource,
    core_knots: &[Vec<TrajectoryKnot>],
    meta_knots: &[Vec<TrajectoryKnot>],
    headroom_gain: f64,
    options: &EncodeOptions,
    core_panner: CorePannerConfig,
) -> SalsaSpatialStreamConfig {
    let mut config = SalsaSpatialStreamConfig {
        object_count: source.object_count,
        headroom_gain,
        initial_objects: initial_objects(source, core_knots),
        initial_trajectories: meta_knots.to_vec(),
        core_bitrates: options.core_bitrates,
        beta: options.beta,
        core_panner,
        ..SalsaSpatialStreamConfig::default()
    };
    if core_panner.algorithm_version() == 2 {
        config.format_version = 2;
        config.signal_models[0].algorithm_version = 2;
    }
    config
}

#[allow(clippy::too_many_arguments)]
fn process_job(
    job: &TileJob,
    next: Option<&TileJob>,
    measurement: &mut MeasurementContext,
    program_start: u64,
    codec: &mut MultistreamCodec,
    encoder: &mut SceneParameterEncoder,
    previous_base: &[[i32; PARAM_BAND_COUNT]],
    previous_ell: &[[i32; PARAM_BAND_COUNT]],
    beta: f64,
    checkpoint_due: bool,
) -> Result<ProcessedTile> {
    let local_start = job.source.start_sample - program_start;
    let prefix = measurement.prefix_frames();
    let suffix = next.map_or(0, |job| job.source.frames.min(salsa_spatial::FFT_SIZE / 2));
    let decoded = codec
        .take_aligned(
            local_start - prefix as u64,
            prefix + job.source.frames + suffix,
        )
        .context("aligning locally decoded core")?;
    let measure_start = std::time::Instant::now();
    let measurements = measurement.measure_tile(
        &job.source,
        &job.core,
        &decoded,
        &job.core_knots,
        next.map(|job| (&job.source, &job.core)),
    );
    let measure_ms = measure_start.elapsed().as_secs_f64() * 1000.0;
    // The checkpoint describes the state immediately before this tile.
    let checkpoint = checkpoint_due.then(|| encoder.checkpoint(job.source.start_sample));
    let search_start = std::time::Instant::now();
    let search = search_tile(
        encoder,
        &measurements,
        job.source.start_sample,
        job.source.frames,
        &job.meta_knots,
        previous_base,
        previous_ell,
        beta,
    )?;
    let search_ms = search_start.elapsed().as_secs_f64() * 1000.0;
    let tile: ParameterTile = search.tile;

    let encode_start = std::time::Instant::now();
    let enhancement_bytes = encoder
        .encode_tile(&tile)
        .context("encoding parameter tile")?;
    let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
    log::info!(
        "tile {}: measure {measure_ms:.1} ms, search {search_ms:.1} ms, encode {encode_ms:.1} ms",
        job.source.start_sample
    );
    let active_objects = tile
        .anchors
        .first()
        .map(|anchor| anchor.active.iter().filter(|active| **active).count())
        .unwrap_or(0);

    let frame = SalsaSpatialFrame {
        sample_rate: SAMPLE_RATE,
        start_sample: job.source.start_sample,
        frames: job.source.frames,
        packets: job.packets.clone(),
        enhancement: EnhancementTile {
            start_sample: job.source.start_sample,
            frames: job.source.frames,
            bytes: enhancement_bytes.clone(),
        },
        checkpoint,
        parameters: None,
    };

    let core_bytes = job
        .packets
        .iter()
        .map(|packet| {
            packet
                .substreams
                .iter()
                .map(|stream| stream.len() as u64)
                .sum::<u64>()
        })
        .sum();

    Ok(ProcessedTile {
        frame,
        core_bytes,
        active_objects,
        frames: job.source.frames,
    })
}

fn tile_report(
    start_sample: u64,
    processed: &ProcessedTile,
    sizes: &salsa_spatial::RecordSizes,
    transport_bytes: u64,
    object_count: usize,
) -> TileReport {
    let metadata_bytes = sizes.metadata_bytes();
    let framing_bytes = transport_bytes
        .saturating_sub(processed.core_bytes)
        .saturating_sub(metadata_bytes);
    TileReport {
        start_sample,
        core_bytes: processed.core_bytes,
        parameter_bytes: sizes.parameter_bytes,
        checkpoint_bytes: sizes.checkpoint_bytes,
        configuration_bytes: sizes.configuration_bytes,
        other_scene_bytes: sizes.other_scene_bytes,
        metadata_bytes,
        framing_bytes,
        transport_bytes,
        object_count,
        active_objects: processed.active_objects,
    }
}

/// Encode a DAMF master to a Salsa Ogg Opus stream.
pub fn encode(options: &EncodeOptions) -> Result<EncodeReport> {
    encode_with_progress(options, |_| {})
}

/// Encode with progress notifications on the calling thread.
///
/// Each pass starts at zero. Encoding progress counts source samples written
/// to the output. The final notification follows a successful output flush.
/// An explicit headroom gain skips the headroom pass.
pub fn encode_with_progress(
    options: &EncodeOptions,
    mut progress: impl FnMut(EncodeProgress),
) -> Result<EncodeReport> {
    if options.beta < 0.0 || !options.beta.is_finite() {
        bail!("beta must be finite and non-negative");
    }
    if options.core_bitrates.iter().any(|bitrate| *bitrate == 0) {
        bail!("core bitrates must be positive");
    }

    let core_panner = CorePannerConfig::new(options.ceiling_horizontal_gain)?;
    let panner = CorePanner::from_config(core_panner);
    let probe = SourceReader::open(&options.master)?;
    let info = probe.info().clone();
    let object_count = info.objects.len();
    if object_count > MAX_OBJECTS {
        bail!("source has {object_count} objects, the v1 profile supports {MAX_OBJECTS}");
    }
    let total_frames = info
        .total_frames
        .context("DAMF essence length is unknown; pass --max-frames")?;
    let start = options.reference_start.min(total_frames);
    let available = total_frames - start;
    let duration = options
        .max_frames
        .map(|frames| (frames as u64).min(available))
        .unwrap_or(available);
    drop(probe);

    let opus = salsa_spatial_core_decoder::version_info();
    let headroom_gain = choose_headroom(options, duration, panner, &mut progress)?;

    progress(EncodeProgress {
        stage: EncodeStage::Encoding,
        completed_samples: 0,
        total_samples: duration,
    });

    let mut reader = SourceReader::open(&options.master)?;
    reader.seek(start)?;
    let mut codec: Option<MultistreamCodec> = None;
    let mut encoder = SceneParameterEncoder::new(object_count);
    let mut writer: Option<salsa_spatial::OggSalsaWriter> = None;
    let mut pending: Option<TileJob> = None;
    let mut measurement = MeasurementContext::new(headroom_gain, panner, BaseFold::v1());
    let mut last_base = vec![[0i32; PARAM_BAND_COUNT]; object_count];
    let mut previous_ell = vec![[0i32; PARAM_BAND_COUNT]; object_count];
    let mut last_positions = vec![[0.0f64; 3]; object_count];
    let mut first_tile = true;

    let file = File::create(&options.output)
        .with_context(|| format!("creating {}", options.output.display()))?;
    let mut output = BufWriter::new(file);

    let mut remaining = duration;
    let mut processed_samples = 0u64;
    let mut core_bytes = 0u64;
    let mut parameter_bytes = 0u64;
    let mut checkpoint_bytes = 0u64;
    let mut configuration_bytes = 0u64;
    let mut other_scene_bytes = 0u64;
    let mut metadata_bytes = 0u64;
    let mut framing_bytes = 0u64;
    let mut transport_bytes = 0u64;
    let mut tile_reports = Vec::new();
    let mut peak_metadata_bitrate = 0.0f64;

    while remaining > 0 {
        let frames = remaining.min(TILE_SAMPLES as u64) as usize;
        let Some(source) = reader.read_tile(frames)? else {
            break;
        };
        let frames = source.frames;
        let (core_knots, meta_knots) =
            fit_tile_knots(&source, &mut last_positions, first_tile, panner);
        if codec.is_none() {
            let config = make_config(
                &source,
                &core_knots,
                &meta_knots,
                headroom_gain,
                options,
                core_panner,
            );
            let new_codec = MultistreamCodec::new(config.clone())?;
            log::info!("Opus lookahead: {} samples", new_codec.lookahead_samples());
            let pre_skip = new_codec.lookahead_samples() as u16;
            let (new_writer, header) =
                salsa_spatial::OggSalsaWriter::new(config, 0x5341_4c53, pre_skip)?;
            output.write_all(&header)?;
            transport_bytes += header.len() as u64;
            framing_bytes += header.len() as u64;
            codec = Some(new_codec);
            writer = Some(new_writer);
        }
        let fold = writer
            .as_ref()
            .map(|writer| writer.config().fold())
            .unwrap_or_else(salsa_spatial::BaseFold::v1);
        let transport = build_transport(&source, &core_knots, headroom_gain, panner, fold, 2);
        let core = transport.observation_order();
        let packets = codec
            .as_mut()
            .expect("multistream codec exists")
            .encode_tile(&transport)?;

        let job = TileJob {
            source,
            core_knots,
            meta_knots,
            core,
            packets,
        };
        first_tile = false;
        remaining -= frames as u64;

        if let Some(previous) = pending.take() {
            let start_sample = previous.source.start_sample;
            let end_sample = start_sample + previous.source.frames as u64;
            let due = checkpoint_due(start_sample, start);
            let processed = process_job(
                &previous,
                Some(&job),
                &mut measurement,
                start,
                codec.as_mut().expect("multistream codec exists"),
                &mut encoder,
                &last_base,
                &previous_ell,
                options.beta,
                due,
            )?;
            let mut sizes = salsa_spatial::RecordSizes::default();
            let transport = writer
                .as_mut()
                .expect("writer exists")
                .write_frame_with_sizes(&processed.frame, &mut sizes)?;
            output.write_all(&transport)?;

            let report = tile_report(
                start_sample,
                &processed,
                &sizes,
                transport.len() as u64,
                object_count,
            );
            core_bytes += report.core_bytes;
            parameter_bytes += report.parameter_bytes;
            checkpoint_bytes += report.checkpoint_bytes;
            configuration_bytes += report.configuration_bytes;
            other_scene_bytes += report.other_scene_bytes;
            metadata_bytes += report.metadata_bytes;
            framing_bytes += report.framing_bytes;
            transport_bytes += report.transport_bytes;
            let tile_rate = bitrate(report.metadata_bytes, processed.frames as u64);
            peak_metadata_bitrate = peak_metadata_bitrate.max(tile_rate);
            processed_samples += processed.frames as u64;
            tile_reports.push(report);
            progress(EncodeProgress {
                stage: EncodeStage::Encoding,
                completed_samples: processed_samples,
                total_samples: duration,
            });

            let state = encoder.snapshot(end_sample);
            last_base = state.power_model.base_half_db;
            previous_ell = state.power.ell_half_db;
        }
        pending = Some(job);
    }

    let mut tail_packet = None;
    if let Some(codec) = codec.as_mut() {
        tail_packet = codec.flush().context("flushing the local core decoder")?;
    }

    if let Some(last) = pending.take() {
        let start_sample = last.source.start_sample;
        let due = checkpoint_due(start_sample, start);
        let processed = process_job(
            &last,
            None,
            &mut measurement,
            start,
            codec.as_mut().expect("multistream codec exists"),
            &mut encoder,
            &last_base,
            &previous_ell,
            options.beta,
            due,
        )?;
        let mut sizes = salsa_spatial::RecordSizes::default();
        let transport = writer
            .as_mut()
            .expect("writer exists")
            .write_frame_with_sizes(&processed.frame, &mut sizes)?;
        output.write_all(&transport)?;

        let report = tile_report(
            start_sample,
            &processed,
            &sizes,
            transport.len() as u64,
            object_count,
        );
        core_bytes += report.core_bytes;
        parameter_bytes += report.parameter_bytes;
        checkpoint_bytes += report.checkpoint_bytes;
        configuration_bytes += report.configuration_bytes;
        other_scene_bytes += report.other_scene_bytes;
        metadata_bytes += report.metadata_bytes;
        framing_bytes += report.framing_bytes;
        transport_bytes += report.transport_bytes;
        let tile_rate = bitrate(report.metadata_bytes, processed.frames as u64);
        peak_metadata_bitrate = peak_metadata_bitrate.max(tile_rate);
        processed_samples += processed.frames as u64;
        tile_reports.push(report);
    }

    if let (Some(writer), Some(packet)) = (writer.as_mut(), tail_packet) {
        let tail = writer.write_tail(&packet)?;
        output.write_all(&tail)?;
        transport_bytes += tail.len() as u64;
        framing_bytes += tail.len() as u64;
    }
    if let Some(writer) = writer.as_mut() {
        let tail = writer.finish();
        output.write_all(&tail)?;
        transport_bytes += tail.len() as u64;
        framing_bytes += tail.len() as u64;
    }
    output.flush()?;
    progress(EncodeProgress {
        stage: EncodeStage::Encoding,
        completed_samples: processed_samples,
        total_samples: duration,
    });

    Ok(EncodeReport {
        ceiling_horizontal_gain: options.ceiling_horizontal_gain,
        headroom_gain,
        core_bytes,
        parameter_bytes,
        checkpoint_bytes,
        configuration_bytes,
        other_scene_bytes,
        metadata_bytes,
        framing_bytes,
        transport_bytes,
        total_bytes: transport_bytes,
        duration_samples: processed_samples,
        core_bitrate: bitrate(core_bytes, processed_samples),
        metadata_bitrate: bitrate(metadata_bytes, processed_samples),
        total_bitrate: bitrate(transport_bytes, processed_samples),
        per_tile_metadata: tile_reports,
        peak_metadata_bitrate,
        object_count,
        opus_wrapper: opus.wrapper,
        opus_native: opus.native,
        warnings: info.warnings.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_schedule_follows_the_program_start() {
        let start = 91;
        assert!(checkpoint_due(start, start));
        assert!(!checkpoint_due(start + 4_800, start));
        assert!(checkpoint_due(start + CHECKPOINT_SAMPLES, start));
        assert!(checkpoint_due(start + 2 * CHECKPOINT_SAMPLES, start));
        assert!(!checkpoint_due(
            start + 2 * CHECKPOINT_SAMPLES + 4_800,
            start
        ));
    }

    #[test]
    fn bitrate_accounting_uses_every_byte_and_source_duration() {
        assert_eq!(bitrate(0, 0), 0.0);
        assert!((bitrate(120_000, 48_000) - 960_000.0).abs() < 1e-9);
        assert!((bitrate(60_000, 24_000) - 960_000.0).abs() < 1e-9);
    }

    #[test]
    fn parsed_stream_roundtrips_if_available() {
        use objectaudio_ir::SpatialParser;
        let Ok(path) = std::env::var("SALSA_TEST_OGG") else {
            return;
        };
        let bytes = std::fs::read(path).expect("test stream");
        let mut reader = salsa_spatial::SalsaSpatialParser::new();
        let mut frames = reader.push_bytes(&bytes).expect("parse stream");
        frames.extend(reader.flush().expect("flush stream"));
        assert_eq!(frames.len(), 10);
        assert_eq!(
            frames.iter().map(|frame| frame.frames).sum::<usize>(),
            48_000
        );
        assert!(reader.config().is_some());
        assert!(frames.iter().all(|frame| frame.packets.len() == 6));
        assert!(
            frames
                .iter()
                .all(|frame| !frame.enhancement.bytes.is_empty())
        );
        assert_eq!(frames.last().unwrap().frames, 4_800);
        assert!(frames.last().unwrap().checkpoint.is_some());
    }

    #[test]
    fn decodes_written_stream_if_available() {
        use objectaudio_ir::{SpatialDecoder, SpatialParser};
        let Ok(path) = std::env::var("SALSA_TEST_OGG") else {
            return;
        };
        let bytes = std::fs::read(path).expect("test stream");
        let mut reader = salsa_spatial::SalsaSpatialParser::new();
        let mut frames = reader.push_bytes(&bytes).expect("parse stream");
        frames.extend(reader.flush().expect("flush stream"));
        let config = reader.config().cloned().expect("stream config");
        let mut decoder = salsa_spatial_dsp::SalsaSpatialDecoder::new(config).expect("decoder");
        let mut samples = 0usize;
        for frame in &frames {
            let decoded = decoder.decode(frame).expect("decode tile");
            assert_eq!(decoded.pcm.channels().len(), 1 + decoded.objects.len());
            samples += decoded.frames;
        }
        if let Some(last) = decoder.finish().expect("drain decoder") {
            samples += last.frames;
        }
        // A finite stream ends at the Opus lookahead boundary; the decoder
        // discards the leading lookahead and has no extra flush packet.
        assert!(samples + 120 >= 48_000, "decoded {samples} samples");
    }

    #[test]
    fn gain_quantization_is_deterministic() {
        assert_eq!(quantize_gain(0.5), 0.5);
        assert_eq!(
            quantize_gain(0.50001),
            (0.50001f64 * 4096.0).round() / 4096.0
        );
        assert!(quantize_gain(0.0) > 0.0);
    }
}
