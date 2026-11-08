//! Quality/checkpoint evaluation for a salsa-spatial stream.
//!
//! Usage:
//! `cargo run -p salsa-spatial-encoder --example evaluate -- \
//!      <master.atmos> <stream.ogg> [--max-frames N] [--out-prefix PATH]`
//!
//! The tool reads the original DAMF essence with the encoder's `SourceReader`
//! (authored gain/activity applied exactly once), decodes the IAMF stream with
//! the standalone decoder, and reports normalized RMS errors at the
//! checkpoints the design document can compute directly:
//!
//! * object waveform error,
//! * the same object scene rendered to `7.1.4` and `9.1.6`,
//! * the decoded Opus core bed against the lossless configured observation
//!   downmix.
//!
//! Measurements that need external tools (ViSQOL/SPAUQ, binaural HRTF, DD+
//! Atmos) are listed explicitly in the report's `notes` field.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use objectaudio_ir::SpatialParser;
use objectaudio_ir::{SceneTimeline, SpatialDecoder, adm_to_q15_f64};
use objectaudio_render::{
    BLOCK_SAMPLES, SpeakerControls, SpeakerMetadata, SpeakerRenderer, layout_by_name,
};
use salsa_spatial::{SAMPLE_RATE, SalsaSpatialParser, TILE_SAMPLES};
use salsa_spatial_core_decoder::OpusBedDecoder;
use salsa_spatial_dsp::{CorePanner, SalsaSpatialDecoder};
use salsa_spatial_encoder::source::SourceReader;
use serde::Serialize;

/// The document's single reference-relative error floor.
const SILENCE_FLOOR_DB: f64 = -60.0;
/// Absolute reference RMS below which the error ratio is undefined.
// Roughly -120 dBFS. The first-second excerpt used for smoke testing is
// 1-2 LSB of 24-bit audio, which is digital silence for this metric.
const SILENCE_RMS: f64 = 1.0e-6;

#[derive(Serialize)]
struct Report {
    duration_samples: u64,
    object_count: usize,
    frames_compared: usize,
    reference_start: u64,
    reference_rms_db: f64,
    reference_samples: usize,
    candidate_samples: usize,
    requested_samples: usize,
    alignment_trimmed: bool,
    object_error: ObjectErrorReport,
    render_714: RenderReport,
    render_916: RenderReport,
    core_bed: CoreBedReport,
    checkpoints: Vec<Checkpoint>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct ObjectErrorReport {
    aggregate_db: f64,
    worst: Vec<ObjectErrorEntry>,
}

#[derive(Serialize)]
struct ObjectErrorEntry {
    object: usize,
    error_db: f64,
}

#[derive(Serialize)]
struct RenderReport {
    layout: String,
    channels: usize,
    error_db: f64,
}

#[derive(Serialize)]
struct CoreBedReport {
    error_db: f64,
    frames_compared: usize,
    reference_frames: usize,
    decoded_frames: usize,
    lookahead_dropped: usize,
    trimmed: bool,
    note: String,
}

#[derive(Serialize)]
struct Checkpoint {
    label: String,
    error_db: f64,
    description: String,
}

struct Arguments {
    master: PathBuf,
    stream: PathBuf,
    max_frames: Option<usize>,
    reference_start: u64,
    out_prefix: Option<PathBuf>,
}

fn main() -> Result<()> {
    let arguments = parse_arguments()?;
    let report = evaluate(&arguments)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn parse_arguments() -> Result<Arguments> {
    let mut args = std::env::args().skip(1);
    let master = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("missing <master.atmos> path"))?;
    let stream = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("missing <stream.ogg> path"))?;

    let mut max_frames = None;
    let mut reference_start = 0u64;
    let mut out_prefix = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--max-frames" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--max-frames needs a value"))?;
                let frames = value
                    .parse::<usize>()
                    .with_context(|| format!("parsing --max-frames {value}"))?;
                if frames == 0 {
                    bail!("--max-frames must be positive");
                }
                max_frames = Some(frames);
            }
            "--reference-start" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--reference-start needs a value"))?;
                reference_start = value
                    .parse::<u64>()
                    .with_context(|| format!("parsing --reference-start {value}"))?;
            }
            "--out-prefix" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--out-prefix needs a value"))?;
                out_prefix = Some(PathBuf::from(value));
            }
            other => bail!("unknown argument {other}"),
        }
    }

    Ok(Arguments {
        master,
        stream,
        max_frames,
        reference_start,
        out_prefix,
    })
}

fn evaluate(arguments: &Arguments) -> Result<Report> {
    // 1. Parse the transport. The declared duration is the sum of the frame
    // durations carried by the stream itself.
    let bytes = std::fs::read(&arguments.stream)
        .with_context(|| format!("reading {}", arguments.stream.display()))?;
    let mut parser = SalsaSpatialParser::new();
    let mut stream_frames = parser.push_bytes(&bytes).context("parsing Salsa stream")?;
    stream_frames.extend(parser.flush().context("flushing Salsa parser")?);
    let config = parser
        .config()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("stream has no salsa-spatial configuration"))?;
    let declared_samples: usize = stream_frames.iter().map(|frame| frame.frames).sum();
    let requested = arguments.max_frames.unwrap_or(declared_samples);
    if requested == 0 {
        bail!("stream declares no audio samples");
    }

    // 2. Read the original source for the requested program span. Authored
    // gain and activity are applied exactly once inside `SourceReader`.
    let mut source = SourceReader::open(&arguments.master)?;
    source.seek(arguments.reference_start)?;
    let source_info = source.info().clone();
    let object_count = config.object_count;
    if source_info.objects.len() != object_count {
        bail!(
            "source has {} objects but the stream declares {object_count}",
            source_info.objects.len()
        );
    }

    let mut reference_lfe: Vec<f32> = Vec::new();
    let mut reference_objects: Vec<Vec<f32>> = vec![Vec::new(); object_count];
    let mut reference_positions: Vec<[f32; 3]> = Vec::new();
    let mut remaining = requested;
    while remaining > 0 {
        let want = remaining.min(TILE_SAMPLES);
        let Some(tile) = source
            .read_tile(want)
            .context("reading the reference source")?
        else {
            break;
        };
        let frames = tile.frames;
        if frames == 0 {
            break;
        }
        reference_lfe.extend_from_slice(&tile.lfe_pcm);
        for (object, samples) in reference_objects.iter_mut().enumerate() {
            samples.extend_from_slice(&tile.object_pcm[object]);
        }
        reference_positions.extend_from_slice(&tile.positions);
        remaining = remaining.saturating_sub(frames);
    }
    let reference_samples = reference_lfe.len();
    if reference_samples == 0 {
        bail!("reference source produced no samples");
    }

    // 3. Decode the full transport with the standalone decoder. Its PCM is
    // LFE first and then the stable source objects, unity object gain, on the
    // source clock after exactly one lookahead removal.
    let mut decoder = SalsaSpatialDecoder::new(config.clone())?;
    let mut decoded_frames = Vec::new();
    let mut candidate_lfe: Vec<f32> = Vec::new();
    let mut candidate_objects: Vec<Vec<f32>> = vec![Vec::new(); object_count];
    for frame in &stream_frames {
        let spatial = decoder
            .decode(frame)
            .with_context(|| format!("decoding tile at {}", frame.start_sample))?;
        append_spatial(
            &spatial,
            object_count,
            &mut candidate_lfe,
            &mut candidate_objects,
        )?;
        if spatial.frames > 0 {
            decoded_frames.push(spatial);
        }
        if candidate_lfe.len() >= requested {
            break;
        }
    }
    if candidate_lfe.len() < requested
        && let Some(tail) = decoder.finish().context("draining the decoder")?
    {
        append_spatial(
            &tail,
            object_count,
            &mut candidate_lfe,
            &mut candidate_objects,
        )?;
        if tail.frames > 0 {
            decoded_frames.push(tail);
        }
    }
    let candidate_samples = candidate_lfe.len();

    // Candidate trajectory positions come from the decoded state timeline, so
    // within-frame events and ramps are evaluated exactly. Reference positions
    // are the authored per-sample source positions. Both are sampled once per
    // renderer block, at the block start.
    let mut candidate_positions = vec![[0.0f32; 3]; candidate_samples.saturating_mul(object_count)];
    build_candidate_positions(&decoded_frames, object_count, &mut candidate_positions)?;

    let frames_compared = reference_samples.min(candidate_samples);
    if frames_compared == 0 {
        bail!("reference and candidate share no samples");
    }

    // 4. Object waveform error.
    let object_error = measure_object_error(
        &reference_objects,
        &candidate_objects,
        frames_compared,
        object_count,
    );

    // 5. Rendered-scene error for both target layouts.
    let render_714 = measure_render(
        "7.1.4",
        object_count,
        &reference_lfe,
        &reference_objects,
        &reference_positions,
        &candidate_lfe,
        &candidate_objects,
        &candidate_positions,
        frames_compared,
        arguments.out_prefix.as_deref(),
    )?;
    let render_916 = measure_render(
        "9.1.6",
        object_count,
        &reference_lfe,
        &reference_objects,
        &reference_positions,
        &candidate_lfe,
        &candidate_objects,
        &candidate_positions,
        frames_compared,
        arguments.out_prefix.as_deref(),
    )?;

    // 6. Configured observation mix vs the decoded Opus core.
    let core_bed = measure_core_bed(
        &stream_frames,
        &config,
        &reference_lfe,
        &reference_objects,
        &reference_positions,
    )?;

    if let Some(prefix) = arguments.out_prefix.as_deref() {
        write_object_wavs(
            prefix,
            &reference_lfe,
            &reference_objects,
            &candidate_lfe,
            &candidate_objects,
            object_count,
            frames_compared,
        )?;
    }

    let alignment_trimmed = reference_samples != candidate_samples;
    let reference_rms_squared: f64 = reference_objects
        .iter()
        .flat_map(|object| object.iter().take(frames_compared))
        .chain(reference_lfe.iter().take(frames_compared))
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        / (frames_compared.saturating_mul(object_count + 1).max(1) as f64);
    let reference_rms_db = 20.0 * reference_rms_squared.max(1.0e-30).sqrt().log10();
    let mut notes = vec![
        "ViSQOL and SPAUQ are external procedures and are not invoked; only normalized RMS waveform and rendered-scene errors are reported.".to_owned(),
        "Binaural HRTF rendering and listening tests are not measured by this tool.".to_owned(),
        "The lossless-core checkpoints with an unrestricted per-band estimator and with the unquantized v1 covariance model require encoder-side experiments that are not available here.".to_owned(),
        "The official clustered master and DD+ Atmos external comparison are not available.".to_owned(),
        "Reference positions are the authored per-sample positions. Candidate positions follow a persistent SceneTimeline with the decoded snapshot modes and events. Both renderers sample positions at each 32-sample block start.".to_owned(),
    ];
    if alignment_trimmed {
        notes.push(format!(
            "Reference and candidate lengths differ ({reference_samples} vs {candidate_samples} samples); both were trimmed to {frames_compared} samples."
        ));
    }
    if core_bed.trimmed {
        notes.push(format!(
            "Decoded core is {} samples after one lookahead drop of {} samples; the core comparison was trimmed to {} frames.",
            core_bed.decoded_frames, core_bed.lookahead_dropped, core_bed.frames_compared
        ));
    }

    let reference_energy: f64 = reference_objects
        .iter()
        .flat_map(|object| object.iter().take(frames_compared))
        .chain(reference_lfe.iter().take(frames_compared))
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum();
    if is_near_silence(
        reference_energy,
        frames_compared.saturating_mul(object_count + 1),
    ) {
        notes.push(
            "The reference program is near digital silence; the -60 dB reference-relative floor is applied to every normalized error."
                .to_owned(),
        );
    }

    let checkpoints = vec![
        Checkpoint {
            label: "original_source_render".to_owned(),
            error_db: 0.0,
            description: "Reference source itself; ground truth with no clustering.".to_owned(),
        },
        Checkpoint {
            label: "lossless_core_bed".to_owned(),
            error_db: core_bed.error_db,
            description:
                "Decoded Opus core bed vs the lossless configured observation downmix built from authored object positions; includes trajectory quantization and core coding."
                    .to_owned(),
        },
        Checkpoint {
            label: "decoded_core".to_owned(),
            error_db: object_error.aggregate_db,
            description:
                "Decoded object PCM vs the source objects after headroom restoration; includes core loss, reconstruction, and quantized parameters."
                    .to_owned(),
        },
        Checkpoint {
            label: "final_compressed_parameters_and_transport".to_owned(),
            error_db: render_714.error_db,
            description:
                "Final IAMF transport decoded and rendered to 7.1.4 vs the same reference render."
                    .to_owned(),
        },
    ];

    Ok(Report {
        duration_samples: frames_compared as u64,
        object_count,
        frames_compared,
        reference_start: arguments.reference_start,
        reference_rms_db,
        reference_samples,
        candidate_samples,
        requested_samples: requested,
        alignment_trimmed,
        object_error,
        render_714,
        render_916,
        core_bed,
        checkpoints,
        notes,
    })
}

fn append_spatial(
    spatial: &objectaudio_ir::SpatialFrame,
    object_count: usize,
    lfe: &mut Vec<f32>,
    objects: &mut [Vec<f32>],
) -> Result<()> {
    let channels = spatial.pcm.channels();
    if channels.len() != object_count + 1 {
        bail!(
            "decoded frame has {} channels, expected {}",
            channels.len(),
            object_count + 1
        );
    }
    lfe.extend_from_slice(&channels[0]);
    for object in 0..object_count {
        objects[object].extend_from_slice(&channels[object + 1]);
    }
    Ok(())
}

fn build_candidate_positions(
    frames: &[objectaudio_ir::SpatialFrame],
    object_count: usize,
    positions: &mut [[f32; 3]],
) -> Result<()> {
    let mut start = 0usize;
    let mut scene: Option<SceneTimeline> = None;
    for spatial in frames {
        let payload = spatial.to_payload();
        if let Some(scene) = scene.as_mut() {
            scene.submit(&payload, start as i64)?;
        } else {
            scene = Some(
                SceneTimeline::from_payload(&payload, start as i64)
                    .context("building the decoded scene timeline")?,
            );
        }
        let scene = scene.as_mut().expect("scene initialized");
        for offset in 0..spatial.frames {
            let states = scene.states_at((start + offset) as i64);
            let base = (start + offset) * object_count;
            for object in 0..object_count {
                positions[base + object] =
                    states.get(object).map_or([0.0; 3], |state| state.position);
            }
        }
        start += spatial.frames;
        scene.discard_before(start as i64);
    }
    Ok(())
}

fn measure_object_error(
    reference: &[Vec<f32>],
    candidate: &[Vec<f32>],
    frames: usize,
    object_count: usize,
) -> ObjectErrorReport {
    let mut total_reference = 0.0f64;
    let mut total_difference = 0.0f64;
    let mut per_object = Vec::with_capacity(object_count);
    for (object, (reference, candidate)) in reference.iter().zip(candidate).enumerate() {
        let mut reference_energy = 0.0f64;
        let mut difference_energy = 0.0f64;
        for sample in 0..frames {
            let reference_value = f64::from(reference[sample]);
            let candidate_value = f64::from(candidate[sample]);
            reference_energy += reference_value * reference_value;
            let difference = candidate_value - reference_value;
            difference_energy += difference * difference;
        }
        total_reference += reference_energy;
        total_difference += difference_energy;
        per_object.push(ObjectErrorEntry {
            object,
            error_db: normalized_error_from_sums(reference_energy, difference_energy, frames),
        });
    }
    per_object.sort_by(|a, b| {
        b.error_db
            .partial_cmp(&a.error_db)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    per_object.truncate(5);
    ObjectErrorReport {
        aggregate_db: normalized_error_from_sums(
            total_reference,
            total_difference,
            frames.saturating_mul(object_count),
        ),
        worst: per_object,
    }
}

#[allow(clippy::too_many_arguments)]
fn measure_render(
    layout_name: &str,
    object_count: usize,
    reference_lfe: &[f32],
    reference_objects: &[Vec<f32>],
    reference_positions: &[[f32; 3]],
    candidate_lfe: &[f32],
    candidate_objects: &[Vec<f32>],
    candidate_positions: &[[f32; 3]],
    frames: usize,
    out_prefix: Option<&Path>,
) -> Result<RenderReport> {
    let layout = layout_by_name(layout_name)
        .ok_or_else(|| anyhow::anyhow!("layout {layout_name} is not registered"))?;
    let controls = SpeakerControls::default_for(layout);
    let output_channels = layout.channel_count();
    let input_channels = object_count + 1;

    let mut reference_output = vec![0.0f64; frames * output_channels];
    let mut candidate_output = vec![0.0f64; frames * output_channels];
    let mut reference_renderer = SpeakerRenderer::new(layout, object_count);
    let mut candidate_renderer = SpeakerRenderer::new(layout, object_count);
    let mut reference_input = vec![0.0f32; BLOCK_SAMPLES * input_channels];
    let mut candidate_input = vec![0.0f32; BLOCK_SAMPLES * input_channels];
    let mut block_output = vec![0.0f64; BLOCK_SAMPLES * output_channels];

    for start in (0..frames).step_by(BLOCK_SAMPLES) {
        let count = BLOCK_SAMPLES.min(frames - start);
        reference_input.fill(0.0);
        candidate_input.fill(0.0);
        for offset in 0..count {
            let sample = start + offset;
            let base = offset * input_channels;
            reference_input[base] = reference_lfe[sample];
            candidate_input[base] = candidate_lfe[sample];
            for object in 0..object_count {
                reference_input[base + 1 + object] = reference_objects[object][sample];
                candidate_input[base + 1 + object] = candidate_objects[object][sample];
            }
        }

        let reference_block: Vec<[u16; 3]> = (0..object_count)
            .map(|object| {
                let position = reference_positions[start * object_count + object];
                adm_to_q15_f64(position.map(f64::from))
            })
            .collect();
        let candidate_block: Vec<[u16; 3]> = (0..object_count)
            .map(|object| {
                let position = candidate_positions[start * object_count + object];
                adm_to_q15_f64(position.map(f64::from))
            })
            .collect();
        let reference_metadata = [SpeakerMetadata::new(0, 0, &reference_block)];
        let candidate_metadata = [SpeakerMetadata::new(0, 0, &candidate_block)];

        reference_renderer.process(
            &reference_input,
            BLOCK_SAMPLES,
            &reference_metadata,
            &controls,
            &mut block_output,
        )?;
        reference_output[start * output_channels..(start + count) * output_channels]
            .copy_from_slice(&block_output[..count * output_channels]);

        candidate_renderer.process(
            &candidate_input,
            BLOCK_SAMPLES,
            &candidate_metadata,
            &controls,
            &mut block_output,
        )?;
        candidate_output[start * output_channels..(start + count) * output_channels]
            .copy_from_slice(&block_output[..count * output_channels]);
    }

    let (reference_energy, difference_energy) =
        interleaved_energies(&reference_output, &candidate_output);
    let error_db = normalized_error_from_sums(
        reference_energy,
        difference_energy,
        frames.saturating_mul(output_channels),
    );

    if let Some(prefix) = out_prefix {
        let suffix = layout_name.replace('.', "");
        write_wav_f64(
            &with_suffix(prefix, &format!("ref_{suffix}.wav")),
            output_channels,
            frames,
            &reference_output,
        )?;
        write_wav_f64(
            &with_suffix(prefix, &format!("cand_{suffix}.wav")),
            output_channels,
            frames,
            &candidate_output,
        )?;
    }

    Ok(RenderReport {
        layout: layout_name.to_owned(),
        channels: output_channels,
        error_db,
    })
}

fn measure_core_bed(
    stream_frames: &[salsa_spatial::SalsaSpatialFrame],
    config: &salsa_spatial::SalsaSpatialStreamConfig,
    reference_lfe: &[f32],
    reference_objects: &[Vec<f32>],
    reference_positions: &[[f32; 3]],
) -> Result<CoreBedReport> {
    let object_count = config.object_count;
    let headroom_gain = config.headroom_gain;
    let panner = CorePanner::from_config(config.core_panner);
    let reference_frames = reference_lfe.len();
    let mut reference_core: [Vec<f32>; 8] =
        std::array::from_fn(|_| Vec::with_capacity(reference_frames));
    for sample in 0..reference_frames {
        let mut column_sum = [0.0f64; 7];
        for object in 0..object_count {
            let position = reference_positions[sample * object_count + object];
            let column = panner.observation_column(
                [
                    f64::from(position[0]),
                    f64::from(position[1]),
                    f64::from(position[2]),
                ],
                config.fold(),
            );
            let value = f64::from(reference_objects[object][sample]);
            for channel in 0..7 {
                column_sum[channel] += column[channel] * value;
            }
        }
        for channel in 0..7 {
            reference_core[channel].push((column_sum[channel] * headroom_gain) as f32);
        }
        reference_core[7].push((f64::from(reference_lfe[sample]) * headroom_gain) as f32);
    }

    let mut decoder = OpusBedDecoder::from_config(config)?;
    let lookahead = decoder.lookahead_samples();
    let mut decoded_core: [Vec<f32>; 8] = std::array::from_fn(|_| Vec::new());
    for frame in stream_frames {
        for packet in &frame.packets {
            let bed = decoder
                .decode(packet)
                .context("decoding a core packet group")?;
            for channel in 0..8 {
                decoded_core[channel].extend_from_slice(&bed[channel]);
            }
        }
        if decoded_core[0].len() >= reference_frames + lookahead {
            break;
        }
    }
    if lookahead > 0 {
        for channel in &mut decoded_core {
            let drop = lookahead.min(channel.len());
            channel.drain(0..drop);
        }
    }
    let decoded_frames = decoded_core[0].len();
    let frames_compared = reference_frames.min(decoded_frames);
    let mut reference_energy = 0.0f64;
    let mut difference_energy = 0.0f64;
    for channel in 0..8 {
        for sample in 0..frames_compared {
            let reference_value = f64::from(reference_core[channel][sample]);
            let candidate_value = f64::from(decoded_core[channel][sample]);
            reference_energy += reference_value * reference_value;
            let difference = candidate_value - reference_value;
            difference_energy += difference * difference;
        }
    }
    let error_db = normalized_error_from_sums(
        reference_energy,
        difference_energy,
        frames_compared.saturating_mul(8),
    );
    let trimmed = frames_compared < reference_frames || frames_compared < decoded_frames;
    Ok(CoreBedReport {
        error_db,
        frames_compared,
        reference_frames,
        decoded_frames,
        lookahead_dropped: lookahead,
        trimmed,
        note: "Reference is the lossless configured observation mix from authored positions and source objects scaled by the stream headroom gain; candidate is the raw decoded Opus core after one program-level lookahead drop.".to_owned(),
    })
}

fn interleaved_energies(reference: &[f64], candidate: &[f64]) -> (f64, f64) {
    let mut reference_energy = 0.0f64;
    let mut difference_energy = 0.0f64;
    for (reference_value, candidate_value) in reference.iter().zip(candidate) {
        reference_energy += reference_value * reference_value;
        let difference = candidate_value - reference_value;
        difference_energy += difference * difference;
    }
    (reference_energy, difference_energy)
}

fn normalized_error_from_sums(
    reference_energy: f64,
    difference_energy: f64,
    samples: usize,
) -> f64 {
    if samples == 0 {
        return SILENCE_FLOOR_DB;
    }
    let mean_square = reference_energy / samples as f64;
    if !mean_square.is_finite() || mean_square <= SILENCE_RMS * SILENCE_RMS {
        return SILENCE_FLOOR_DB;
    }
    let ratio = (difference_energy / reference_energy).sqrt();
    let error_db = 20.0 * ratio.log10();
    error_db.max(SILENCE_FLOOR_DB)
}

fn is_near_silence(reference_energy: f64, samples: usize) -> bool {
    if samples == 0 {
        return true;
    }
    let mean_square = reference_energy / samples as f64;
    !mean_square.is_finite() || mean_square <= SILENCE_RMS * SILENCE_RMS
}

fn with_suffix(prefix: &Path, suffix: &str) -> PathBuf {
    let mut name = prefix.as_os_str().to_owned();
    name.push("_");
    name.push(suffix);
    PathBuf::from(name)
}

fn write_object_wavs(
    prefix: &Path,
    reference_lfe: &[f32],
    reference_objects: &[Vec<f32>],
    candidate_lfe: &[f32],
    candidate_objects: &[Vec<f32>],
    object_count: usize,
    frames: usize,
) -> Result<()> {
    let channels = object_count + 1;
    let reference = planar_to_interleaved(reference_lfe, reference_objects, frames, object_count);
    let candidate = planar_to_interleaved(candidate_lfe, candidate_objects, frames, object_count);
    write_wav_f32(
        &with_suffix(prefix, "ref_objects.wav"),
        channels,
        frames,
        &reference,
    )?;
    write_wav_f32(
        &with_suffix(prefix, "cand_objects.wav"),
        channels,
        frames,
        &candidate,
    )?;
    Ok(())
}

fn planar_to_interleaved(
    lfe: &[f32],
    objects: &[Vec<f32>],
    frames: usize,
    object_count: usize,
) -> Vec<f32> {
    let channels = object_count + 1;
    let mut interleaved = Vec::with_capacity(frames * channels);
    for sample in 0..frames {
        interleaved.push(lfe[sample]);
        for object in objects.iter().take(object_count) {
            interleaved.push(object[sample]);
        }
    }
    interleaved
}

fn write_wav_f64(path: &Path, channels: usize, frames: usize, samples: &[f64]) -> Result<()> {
    let interleaved: Vec<f32> = samples.iter().map(|value| *value as f32).collect();
    write_wav_f32(path, channels, frames, &interleaved)
}

fn write_wav_f32(path: &Path, channels: usize, frames: usize, samples: &[f32]) -> Result<()> {
    let expected = frames * channels;
    if samples.len() < expected {
        bail!(
            "WAV {} has {} samples, expected {expected}",
            path.display(),
            samples.len()
        );
    }
    let mut file =
        BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let data_bytes = (expected * 4) as u32;
    let riff_bytes = data_bytes + 36;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_bytes.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&3u16.to_le_bytes())?; // IEEE float
    file.write_all(&(channels as u16).to_le_bytes())?;
    file.write_all(&SAMPLE_RATE.to_le_bytes())?;
    let byte_rate = SAMPLE_RATE * channels as u32 * 4;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&((channels * 4) as u16).to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    for sample in &samples[..expected] {
        file.write_all(&sample.to_le_bytes())?;
    }
    file.flush()?;
    Ok(())
}
