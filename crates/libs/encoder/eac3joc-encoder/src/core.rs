// SPDX-License-Identifier: MPL-2.0
//! Streaming DAMF -> E-AC-3 JOC encoder.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use objectaudio_damf::{Manifest, Metadata};
use reac3e::{
    Acmod, BsiMetadata, ChannelLayout, ChannelOrder, Coupling, EncoderConfig, FrameEdits,
    FrameEncoder, PlanarFrame, SkipMetadata,
};

use crate::emdf::{self, EmdfProtector, KeyedProtector, ObjectPos, PublicCrcProtector};
use crate::{joc, joc_analysis, lfe, render};

/// Per-object fine/coarse fallback strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JocSelect {
    Knapsack,
    Greedy,
}

/// JOC band mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JocMode {
    Auto,
    PerBand,
    Broadband,
}

/// JOC quantization mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JocQuant {
    Auto,
    Fine,
    Coarse,
}

/// reac3e core options.
#[derive(Debug, Clone)]
pub struct CoreOptions {
    pub bitrate: u32,
    pub dialnorm: i8,
    pub coupling: Coupling,
    pub coupling_start: Option<u8>,
    pub stereo_rematrixing: bool,
    pub block_switch: bool,
    pub aht: bool,
    pub cutoff_hz: Option<u32>,
    pub metadata: BsiMetadata,
}

/// JOC/OAMD/EMDF options.
#[derive(Debug, Clone)]
pub struct JocOptions {
    pub lambda: f64,
    pub ridge: f64,
    pub emdf_budget: usize,
    pub select: JocSelect,
    pub mode: JocMode,
    pub quant: JocQuant,
    pub refine_rounds: usize,
    pub rd_passes: usize,
    pub smooth: f64,
    pub power_gate: f64,
    pub target_block: usize,
    pub addbsi_complexity: Option<u8>,
    pub dynrng_profile: u8,
    pub compr_profile: u8,
}

/// Encoder configuration from the CLI.
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    pub pan: render::PanMode,
    pub gain_db: f64,
    pub surround_phase_shift: bool,
    pub core: CoreOptions,
    pub joc: JocOptions,
    pub emdf_key: Option<Vec<u8>>,
    pub emdf_key_id: u8,
}

/// Encode `atmos` to a raw E-AC-3 JOC elementary stream at `out`.
pub fn encode(atmos: &Path, out: &Path, options: &EncodeOptions) -> Result<()> {
    ensure!(
        (1..=511).contains(&options.joc.emdf_budget),
        "EMDF budget must be 1..=511 bytes"
    );
    ensure!(
        options.joc.target_block < 6,
        "EMDF target block must be 0..=5 for a 6-block frame"
    );
    ensure!(
        (0.0..=1.0).contains(&options.joc.smooth),
        "JOC smoothing must be between 0 and 1"
    );
    ensure!(
        options.joc.power_gate >= 0.0,
        "JOC power gate must be non-negative"
    );
    ensure!(
        options
            .joc
            .addbsi_complexity
            .is_none_or(|complexity| (1..=16).contains(&complexity)),
        "addbsi complexity must be 1..=16"
    );
    ensure!(
        options.joc.dynrng_profile < 8 && options.joc.compr_profile < 8,
        "DRC profiles are 3-bit values (0..=7)"
    );
    let manifest = Manifest::load(atmos)?;
    let metadata = Metadata::load(&manifest.metadata)?;
    ensure!(
        metadata.sample_rate == 48_000,
        "reac3e supports 48 kHz only (DAMF is {} Hz)",
        metadata.sample_rate
    );

    let scene = objectaudio_damf::DamfScene::resolve(&manifest, &metadata)?;
    for warning in scene.warnings.iter().chain(
        &scene
            .report(objectaudio_damf::Consumer::Encode)
            .approximations,
    ) {
        log::warn!("{warning}");
    }
    let nobj = scene.objects.len();
    ensure!(nobj > 0, "DAMF has no dynamic objects");
    ensure!(
        nobj <= 15,
        "JOC supports at most 15 dynamic objects, got {nobj}"
    );
    let object_count = nobj + 1; // + LFE bed
    let complexity = options
        .joc
        .addbsi_complexity
        .unwrap_or(object_count.min(16) as u8);
    let addbsi = [0x01u8, complexity];

    let (mut encoder, spf) = build_core(&options.core)?;
    let dmx_config = if options.surround_phase_shift { 3 } else { 0 };

    // Stage 1: stream the downmix through the in-process FFmpeg LFE filter.
    let mut downmix = render::DownmixReader::new(
        &manifest,
        &scene,
        options.gain_db,
        options.pan,
        options.surround_phase_shift,
    )?;
    let info = downmix.info();
    ensure!(info.frames > 0, "DAMF essence has no audio frames");
    log::info!(
        "downmix: {} frames at {} Hz, {} dynamic objects",
        info.frames,
        info.sample_rate,
        info.object_count
    );
    let mut lfe_filter = lfe::LfeFilter::new()?;
    let mut filtered: VecDeque<f32> = VecDeque::new();
    let mut downmix_done = false;
    let mut filtered_samples = 0usize;

    let output =
        BufWriter::new(File::create(out).with_context(|| format!("creating {}", out.display()))?);
    let mut output = output;

    let mut analyzer = joc_analysis::JocAnalyzer::new(
        &manifest,
        &scene,
        options.pan,
        options.joc.lambda,
        options.joc.ridge,
        options.joc.refine_rounds,
        options.joc.rd_passes,
        options.joc.smooth,
        options.joc.power_gate,
    )?;
    let program_info = emdf::encode_program_info(
        options.surround_phase_shift,
        options.joc.dynrng_profile,
        options.joc.compr_profile,
    );
    let signer = options.emdf_key.as_ref().map(|key| KeyedProtector {
        key: key.clone(),
        key_id: options.emdf_key_id,
    });
    let protector: &dyn EmdfProtector = match &signer {
        Some(signer) => signer,
        None => &PublicCrcProtector,
    };

    let mut offset = 0u64;
    let mut frame_index = 0usize;
    let mut written_bytes = 0usize;
    let mut emdf_total = 0usize;
    let mut emdf_max = 0usize;
    let mut coarse_frames = 0usize;
    let mut broadband_frames = 0usize;

    while offset < info.frames {
        let want = ((info.frames - offset) as usize).min(spf);
        let need = want * 6;
        while filtered.len() < need && !downmix_done {
            match downmix.next_block()? {
                Some(block) => {
                    let out = lfe_filter.push(&block)?;
                    filtered_samples += out.len();
                    filtered.extend(out);
                }
                None => {
                    let tail = lfe_filter.flush()?;
                    filtered_samples += tail.len();
                    filtered.extend(tail);
                    downmix_done = true;
                }
            }
        }
        ensure!(
            filtered.len() >= need,
            "FFmpeg LFE filter produced {} samples, need {need} for frame {frame_index}",
            filtered.len()
        );
        let mut interleaved = Vec::with_capacity(need);
        for _ in 0..need {
            interleaved.push(filtered.pop_front().expect("filtered sample"));
        }

        let mut channels: Vec<Vec<f32>> = (0..6).map(|_| vec![0f32; want]).collect();
        for (index, sample) in interleaved.iter().enumerate() {
            channels[index % 6][index / 6] = *sample;
        }
        let slices: Vec<&[f32]> = channels.iter().map(|channel| channel.as_slice()).collect();

        let start = offset;
        let end = start + spf as u64;
        let objects: Vec<ObjectPos> = render::positions_at(&scene, end)
            .into_iter()
            .map(|position| ObjectPos {
                x: position[0],
                y: position[1],
                z: position[2],
            })
            .collect();
        let bypass: Vec<bool> = scene
            .objects
            .iter()
            .map(|object| object.track.metadata_at(end).trim_bypass)
            .collect();
        let calibration =
            bypass
                .iter()
                .any(|bypass| *bypass)
                .then(|| objectaudio_ir::ProgramCalibration {
                    bypass,
                    ..objectaudio_ir::ProgramCalibration::default()
                });
        let oamd = emdf::encode_oamd(&objects, calibration)?;
        let matrices = analyzer
            .matrices_for_frame(start, end)
            .map_err(|error| anyhow::anyhow!("JOC band analysis at frame {frame_index}: {error}"))?
            .to_vec();
        let rates = analyzer.object_rates().to_vec();

        let (payload, mode) = choose_payload(
            &oamd,
            &program_info,
            &matrices,
            &rates,
            nobj,
            (frame_index % 1024) as u16,
            dmx_config,
            &options.joc,
            protector,
        );
        ensure!(
            payload.len() <= 511,
            "EMDF payload is {} B; the skip field allows at most 511 B \
             (raise --joc-lambda or lower --core-bitrate)",
            payload.len()
        );
        if payload.len() > options.joc.emdf_budget {
            log::debug!(
                "frame {frame_index}: EMDF {} B exceeds the soft budget {} B",
                payload.len(),
                options.joc.emdf_budget
            );
        }
        emdf_total += payload.len();
        emdf_max = emdf_max.max(payload.len());
        match mode {
            1 => coarse_frames += 1,
            2 | 3 => broadband_frames += 1,
            _ => {}
        }

        let metadata_in_frame = SkipMetadata::new(&addbsi, &payload, options.joc.target_block);
        let frame = encoder.encode_frame_with_metadata(
            &PlanarFrame::new(&slices, ChannelOrder::Smpte),
            Some(metadata_in_frame),
        )?;

        if frame_index == 0 {
            log::debug!("frame 0 edits: {:?}", frame.edits);
        }
        let bytes = match &signer {
            Some(signer) => sign_frame(
                signer,
                frame.bytes,
                &frame.edits,
                &addbsi,
                &payload,
                options.joc.target_block,
            )?,
            None => frame.bytes,
        };
        written_bytes += bytes.len();
        output.write_all(&bytes)?;

        offset += spf as u64;
        frame_index += 1;
    }

    if let Some(frame) = encoder.flush()? {
        written_bytes += frame.bytes.len();
        output.write_all(&frame.bytes)?;
    }
    ensure!(
        downmix_done && filtered.is_empty() && filtered_samples == info.frames as usize * 6,
        "LFE filter length mismatch: produced {filtered_samples} samples, expected {}",
        info.frames * 6
    );
    output.flush()?;

    let seconds = info.frames as f64 / 48_000.0;
    let total_bitrate = written_bytes as f64 * 8.0 / seconds;
    log::info!(
        "wrote {}: {} frames, {} bytes, {:.3} kbps (core {:.3} kbps), {nobj} objects",
        out.display(),
        frame_index,
        written_bytes,
        total_bitrate / 1000.0,
        f64::from(options.core.bitrate) / 1000.0
    );
    log::info!(
        "EMDF avg {:.1} B/frame, max {emdf_max} B, coarse frames {coarse_frames}, broadband frames {broadband_frames}",
        emdf_total as f64 / frame_index.max(1) as f64
    );
    Ok(())
}

fn build_core(options: &CoreOptions) -> Result<(FrameEncoder, usize)> {
    let config = EncoderConfig {
        layout: ChannelLayout {
            acmod: Acmod::ThreeTwo,
            lfe: true,
        },
        bitrate_bps: options.bitrate,
        dialnorm: options.dialnorm,
        metadata: options.metadata.clone(),
        coupling: options.coupling,
        coupling_start: options.coupling_start,
        stereo_rematrixing: options.stereo_rematrixing,
        cutoff_hz: options.cutoff_hz,
    };
    let mut encoder = FrameEncoder::new(config)?;
    encoder.set_block_switch(options.block_switch);
    encoder.set_aht_enabled(options.aht);
    let spf = encoder.samples_per_frame();
    ensure!(
        spf == 1536,
        "core bitrate {} selects {} samples/frame; JOC Atmos requires 6-block frames \
         (use a core bitrate at or below 1024000 bps)",
        options.bitrate,
        spf
    );
    Ok((encoder, spf))
}

/// Choose a fine/coarse/broadband JOC payload that fits the skip-field budget.
#[allow(clippy::too_many_arguments)]
fn choose_payload(
    oamd: &[u8],
    program_info: &[u8],
    matrices: &[[[f32; 5]; joc::ENC_BANDS]],
    rates: &[joc_analysis::ObjectRate],
    nobj: usize,
    seq: u16,
    dmx_config: u8,
    options: &JocOptions,
    protector: &dyn EmdfProtector,
) -> (Vec<u8>, u8) {
    let make = |joc_payload: Vec<u8>| {
        emdf::wrap_emdf_with(
            &[
                (emdf::PAYLOAD_OAMD, oamd.to_vec()),
                (emdf::PAYLOAD_JOC, joc_payload),
                (2, program_info.to_vec()),
                (1, vec![0x02, 0x00]),
            ],
            protector,
        )
    };
    let modes: &[bool] = match options.mode {
        JocMode::Auto => &[true, false],
        JocMode::PerBand => &[true],
        JocMode::Broadband => &[false],
    };
    let budget = options.emdf_budget;
    let mut chosen: Option<(Vec<u8>, u8)> = None;
    for &per_band in modes {
        let mut quants = match options.quant {
            JocQuant::Coarse => vec![0u8; nobj],
            JocQuant::Auto | JocQuant::Fine => vec![1u8; nobj],
        };
        let mut payload = make(joc::encode_joc_per_object(
            matrices, seq, &quants, per_band, dmx_config,
        ));
        if payload.len() > budget && options.quant != JocQuant::Coarse {
            match options.select {
                JocSelect::Greedy => {
                    let mut order = Vec::with_capacity(nobj);
                    for object in 0..nobj {
                        let mut candidate = quants.clone();
                        candidate[object] = 0;
                        let smaller = make(joc::encode_joc_per_object(
                            matrices, seq, &candidate, per_band, dmx_config,
                        ));
                        order.push((payload.len().saturating_sub(smaller.len()), object));
                    }
                    order.sort_by_key(|entry| std::cmp::Reverse(entry.0));
                    for (_, object) in order {
                        quants[object] = 0;
                        payload = make(joc::encode_joc_per_object(
                            matrices, seq, &quants, per_band, dmx_config,
                        ));
                        if payload.len() <= budget {
                            break;
                        }
                    }
                }
                JocSelect::Knapsack => {
                    let rate_vec: Vec<(usize, usize, f64)> = rates
                        .iter()
                        .map(|rate| (rate.fine_bits, rate.coarse_bits, rate.coarse_distortion))
                        .collect();
                    let header = joc::joc_header_bits(nobj);
                    let fine_bits: usize = rates.iter().map(|rate| rate.fine_bits).sum();
                    let joc_bytes_fine = (header + fine_bits).div_ceil(8);
                    let fixed = payload.len().saturating_sub(joc_bytes_fine);
                    let mut current = budget;
                    let mut selected = None;
                    for _ in 0..4 {
                        let available = current.saturating_sub(fixed + 1) * 8;
                        let matrix_budget = available.saturating_sub(header);
                        if let Some(mask) = joc::select_coarse_objects(&rate_vec, matrix_budget) {
                            let q: Vec<u8> = mask
                                .iter()
                                .map(|&coarse| if coarse { 0 } else { 1 })
                                .collect();
                            let candidate = make(joc::encode_joc_per_object(
                                matrices, seq, &q, per_band, dmx_config,
                            ));
                            if candidate.len() <= current {
                                selected = Some((q, candidate));
                                break;
                            }
                        }
                        if current == 1 {
                            break;
                        }
                        current -= 1;
                    }
                    if let Some((q, candidate)) = selected {
                        quants = q;
                        payload = candidate;
                    } else {
                        quants = vec![0u8; nobj];
                        payload = make(joc::encode_joc_per_object(
                            matrices, seq, &quants, per_band, dmx_config,
                        ));
                    }
                }
            }
        }
        let mode = match (per_band, quants.contains(&0)) {
            (true, false) => 0,
            (true, true) => 1,
            (false, false) => 2,
            (false, true) => 3,
        };
        let fits = payload.len() <= budget;
        if chosen
            .as_ref()
            .is_none_or(|(best, _)| fits || payload.len() < best.len())
        {
            chosen = Some((payload, mode));
        }
        if fits {
            break;
        }
    }
    chosen.expect("at least one JOC encoding attempt")
}

fn sign_frame(
    signer: &KeyedProtector,
    mut bytes: Vec<u8>,
    edits: &FrameEdits,
    addbsi: &[u8],
    emdf: &[u8],
    target_block: usize,
) -> Result<Vec<u8>> {
    let start = edits
        .emdf_start_bit
        .context("EMDF start bit missing from the encoded frame")?;
    let excluded = excluded_ranges(edits, addbsi.len(), emdf.len(), target_block);
    signer
        .sign_frame(&mut bytes, start, emdf, &excluded)
        .map_err(|error| anyhow::anyhow!("EMDF frame signing failed: {error}"))?;
    Ok(bytes)
}

fn excluded_ranges(
    edits: &FrameEdits,
    addbsi_len: usize,
    emdf_len: usize,
    target_block: usize,
) -> Vec<(usize, usize)> {
    let mut ranges = vec![(0usize, 32usize)];
    if edits.info_end_bit > edits.infomdate_bit {
        ranges.push((edits.infomdate_bit, edits.info_end_bit));
    }
    if addbsi_len > 0 {
        ranges.push((
            edits.addbsie_bit,
            edits.addbsie_bit + 1 + 6 + addbsi_len * 8,
        ));
    }
    ranges.push((edits.skipfld_bit, edits.skipfld_bit + 1));
    for (block, &point) in edits.block_skip_points.iter().enumerate() {
        let bits = if block == target_block {
            10 + emdf_len * 8
        } else {
            1
        };
        ranges.push((point, point + bits));
    }
    ranges.push((edits.audio_end_bit, edits.frame_size * 8));
    ranges
}
