// SPDX-License-Identifier: MPL-2.0
//! CLI for the salsa DAMF -> E-AC-3 JOC encoder.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use eac3joc_encoder::reac3e::{
    AdConverterType, BsiMetadata, Coupling, DolbyHeadphoneMode, DolbySurroundExMode,
    DolbySurroundMode, DownmixMode, RoomType, ServiceType,
};
use eac3joc_encoder::{
    CoreOptions, EncodeOptions, JocMode, JocOptions, JocQuant, JocSelect, PanMode,
};

#[derive(Parser)]
#[command(
    name = "salsa-eac3joc-encode",
    version,
    about = "Encode a Dolby Atmos Master (DAMF) to E-AC-3 JOC"
)]
struct Cli {
    /// Path to the `.atmos` manifest.
    master: PathBuf,
    /// Output E-AC-3 elementary stream.
    #[arg(short, long)]
    out: PathBuf,

    // ---- reac3e core -----------------------------------------------------
    /// Core E-AC-3 bitrate in bits/s (metadata rides on top).
    #[arg(long)]
    core_bitrate: u32,
    /// Dialogue normalization, -31..=-1 dB.
    #[arg(long, default_value_t = -21, allow_hyphen_values = true)]
    dialnorm: i8,
    /// Channel coupling mode.
    #[arg(long, value_enum, default_value = "off")]
    coupling: CouplingArg,
    /// Coupling start band 0..=15.
    #[arg(long)]
    coupling_start: Option<u8>,
    /// Disable stereo rematrixing.
    #[arg(long)]
    no_stereo_rematrixing: bool,
    /// Disable block switching.
    #[arg(long)]
    no_block_switch: bool,
    /// Enable experimental AHT.
    #[arg(long)]
    aht: bool,
    /// Override the coded bandwidth cutoff in Hz.
    #[arg(long)]
    cutoff_hz: Option<u32>,

    // ---- BSI metadata ----------------------------------------------------
    #[arg(long, value_enum, default_value = "main")]
    service_type: ServiceTypeArg,
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    copyright: Option<bool>,
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    original: Option<bool>,
    /// Mixing level 80..=111 dB.
    #[arg(long)]
    mixing_level: Option<u8>,
    #[arg(long, value_enum)]
    room_type: Option<RoomTypeArg>,
    #[arg(long, value_enum)]
    ad_converter_type: Option<AdConverterTypeArg>,
    #[arg(long, value_enum)]
    dolby_surround_mode: Option<DolbySurroundModeArg>,
    #[arg(long, value_enum)]
    dolby_headphone_mode: Option<DolbyHeadphoneModeArg>,
    #[arg(long, value_enum)]
    dolby_surround_ex_mode: Option<DolbySurroundExModeArg>,
    /// Preferred stereo downmix (none disables the mixing metadata block).
    #[arg(long, value_enum, default_value = "loro")]
    dmix_mode: DownmixModeArg,
    #[arg(long, default_value_t = std::f32::consts::FRAC_1_SQRT_2)]
    ltrt_center_mix_level: f32,
    #[arg(long, default_value_t = std::f32::consts::FRAC_1_SQRT_2)]
    ltrt_surround_mix_level: f32,
    #[arg(long, default_value_t = std::f32::consts::FRAC_1_SQRT_2)]
    loro_center_mix_level: f32,
    #[arg(long, default_value_t = std::f32::consts::FRAC_1_SQRT_2)]
    loro_surround_mix_level: f32,

    // ---- JOC -------------------------------------------------------------
    /// Huffman-aware rate-distortion weight (0 disables refinement).
    #[arg(long, default_value_t = 1e-3)]
    joc_lambda: f64,
    /// Diagonal loading relative to the per-band core covariance trace.
    #[arg(long, default_value_t = 1e-3)]
    joc_ridge: f64,
    /// EMDF skip-field budget in bytes (511 is the hard limit).
    #[arg(long, default_value_t = 511)]
    emdf_budget: usize,
    /// Per-object fine/coarse fallback strategy.
    #[arg(long, value_enum, default_value = "knapsack")]
    joc_select: JocSelectArg,
    /// Band mode: auto falls back from per-band to broadband.
    #[arg(long, value_enum, default_value = "auto")]
    joc_mode: JocModeArg,
    /// Quantizer mode: auto enables the adaptive per-object fallback.
    #[arg(long, value_enum, default_value = "auto")]
    joc_quant: JocQuantArg,
    /// Quantization-aware coordinate-descent rounds.
    #[arg(long, default_value_t = 2)]
    joc_refine_rounds: usize,
    /// Huffman-aware rate-distortion passes.
    #[arg(long, default_value_t = 1)]
    joc_rd_passes: usize,
    /// Causal covariance smoothing weight (0..1).
    #[arg(long, default_value_t = 0.9)]
    joc_smooth: f64,
    /// Per-band power gate relative to the core energy.
    #[arg(long, default_value_t = 1e-8)]
    joc_power_gate: f64,
    /// Audio block carrying the EMDF skip field (0..5).
    #[arg(long, default_value_t = 2)]
    emdf_target_block: usize,
    /// Override the addbsi complexity index (default: object count, capped at 16).
    #[arg(long)]
    addbsi_complexity: Option<u8>,
    /// Dynamic-range profile written to programme information (3 bits).
    #[arg(long, default_value_t = 4)]
    drc_dynrng_profile: u8,
    /// Compression profile written to programme information (3 bits).
    #[arg(long, default_value_t = 4)]
    drc_compr_profile: u8,

    // ---- downmix / output ------------------------------------------------
    #[arg(long, value_enum, default_value = "hard71")]
    pan: PanModeArg,
    /// Makeup gain on the five main channels (not LFE), in dB.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    gain_db: f64,
    /// Do not apply DME's +90 degree Ls/Rs all-pass (writes dmx_config=0).
    #[arg(long)]
    no_surround_phase_shift: bool,
    /// Dolby EMDF key: raw hex, `@path` to a hex file, or `@path` to a ddpmeta key set.
    #[arg(long, env = "DOLBY_EMDF_KEY")]
    emdf_key: Option<String>,
    /// key_id (0..7) written alongside a supplied key.
    #[arg(long, default_value_t = 0)]
    emdf_key_id: u8,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CouplingArg {
    Auto,
    On,
    Off,
}

impl From<CouplingArg> for Coupling {
    fn from(value: CouplingArg) -> Self {
        match value {
            CouplingArg::Auto => Coupling::Auto,
            CouplingArg::On => Coupling::On,
            CouplingArg::Off => Coupling::Off,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ServiceTypeArg {
    Main,
    Effects,
    VisuallyImpaired,
    HearingImpaired,
    Commentary,
    Emergency,
    VoiceOver,
    Karaoke,
}

impl From<ServiceTypeArg> for ServiceType {
    fn from(value: ServiceTypeArg) -> Self {
        match value {
            ServiceTypeArg::Main => ServiceType::Main,
            ServiceTypeArg::Effects => ServiceType::Effects,
            ServiceTypeArg::VisuallyImpaired => ServiceType::VisuallyImpaired,
            ServiceTypeArg::HearingImpaired => ServiceType::HearingImpaired,
            ServiceTypeArg::Commentary => ServiceType::Commentary,
            ServiceTypeArg::Emergency => ServiceType::Emergency,
            ServiceTypeArg::VoiceOver => ServiceType::VoiceOver,
            ServiceTypeArg::Karaoke => ServiceType::Karaoke,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RoomTypeArg {
    NotIndicated,
    Large,
    Small,
}

impl From<RoomTypeArg> for RoomType {
    fn from(value: RoomTypeArg) -> Self {
        match value {
            RoomTypeArg::NotIndicated => RoomType::NotIndicated,
            RoomTypeArg::Large => RoomType::Large,
            RoomTypeArg::Small => RoomType::Small,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AdConverterTypeArg {
    Standard,
    Hdcd,
}

impl From<AdConverterTypeArg> for AdConverterType {
    fn from(value: AdConverterTypeArg) -> Self {
        match value {
            AdConverterTypeArg::Standard => AdConverterType::Standard,
            AdConverterTypeArg::Hdcd => AdConverterType::Hdcd,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DolbySurroundModeArg {
    NotIndicated,
    Off,
    On,
}

impl From<DolbySurroundModeArg> for DolbySurroundMode {
    fn from(value: DolbySurroundModeArg) -> Self {
        match value {
            DolbySurroundModeArg::NotIndicated => DolbySurroundMode::NotIndicated,
            DolbySurroundModeArg::Off => DolbySurroundMode::Off,
            DolbySurroundModeArg::On => DolbySurroundMode::On,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DolbyHeadphoneModeArg {
    NotIndicated,
    Off,
    On,
}

impl From<DolbyHeadphoneModeArg> for DolbyHeadphoneMode {
    fn from(value: DolbyHeadphoneModeArg) -> Self {
        match value {
            DolbyHeadphoneModeArg::NotIndicated => DolbyHeadphoneMode::NotIndicated,
            DolbyHeadphoneModeArg::Off => DolbyHeadphoneMode::Off,
            DolbyHeadphoneModeArg::On => DolbyHeadphoneMode::On,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DolbySurroundExModeArg {
    NotIndicated,
    Off,
    On,
    DpliiZ,
}

impl From<DolbySurroundExModeArg> for DolbySurroundExMode {
    fn from(value: DolbySurroundExModeArg) -> Self {
        match value {
            DolbySurroundExModeArg::NotIndicated => DolbySurroundExMode::NotIndicated,
            DolbySurroundExModeArg::Off => DolbySurroundExMode::Off,
            DolbySurroundExModeArg::On => DolbySurroundExMode::On,
            DolbySurroundExModeArg::DpliiZ => DolbySurroundExMode::DpliiZ,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DownmixModeArg {
    None,
    Ltrt,
    Loro,
    Dplii,
}

impl From<DownmixModeArg> for DownmixMode {
    fn from(value: DownmixModeArg) -> Self {
        match value {
            DownmixModeArg::None => unreachable!(),
            DownmixModeArg::Ltrt => DownmixMode::LtRt,
            DownmixModeArg::Loro => DownmixMode::LoRo,
            DownmixModeArg::Dplii => DownmixMode::Dplii,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PanModeArg {
    Vbap,
    Hard51,
    Hard71,
}

impl From<PanModeArg> for PanMode {
    fn from(value: PanModeArg) -> Self {
        match value {
            PanModeArg::Vbap => PanMode::Vbap,
            PanModeArg::Hard51 => PanMode::Hard51,
            PanModeArg::Hard71 => PanMode::Hard71,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum JocSelectArg {
    Knapsack,
    Greedy,
}

impl From<JocSelectArg> for JocSelect {
    fn from(value: JocSelectArg) -> Self {
        match value {
            JocSelectArg::Knapsack => JocSelect::Knapsack,
            JocSelectArg::Greedy => JocSelect::Greedy,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum JocModeArg {
    Auto,
    PerBand,
    Broadband,
}

impl From<JocModeArg> for JocMode {
    fn from(value: JocModeArg) -> Self {
        match value {
            JocModeArg::Auto => JocMode::Auto,
            JocModeArg::PerBand => JocMode::PerBand,
            JocModeArg::Broadband => JocMode::Broadband,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum JocQuantArg {
    Auto,
    Fine,
    Coarse,
}

impl From<JocQuantArg> for JocQuant {
    fn from(value: JocQuantArg) -> Self {
        match value {
            JocQuantArg::Auto => JocQuant::Auto,
            JocQuantArg::Fine => JocQuant::Fine,
            JocQuantArg::Coarse => JocQuant::Coarse,
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let emdf_key = cli
        .emdf_key
        .as_deref()
        .map(|key| load_emdf_key(key, cli.emdf_key_id))
        .transpose()?;
    if emdf_key.is_some() {
        log::info!("EMDF frame signing enabled (key_id={})", cli.emdf_key_id);
    } else {
        log::warn!(
            "no --emdf-key: output uses the public-CRC placeholder and will not pass Dolby validation"
        );
    }

    let (preferred_stereo_downmix, levels) = match cli.dmix_mode {
        DownmixModeArg::None => (None, [None; 4]),
        mode => (
            Some(mode.into()),
            [
                Some(cli.ltrt_center_mix_level),
                Some(cli.ltrt_surround_mix_level),
                Some(cli.loro_center_mix_level),
                Some(cli.loro_surround_mix_level),
            ],
        ),
    };
    let metadata = BsiMetadata {
        service_type: cli.service_type.into(),
        copyright: cli.copyright,
        original: cli.original,
        mixing_level: cli.mixing_level,
        room_type: cli.room_type.map(Into::into),
        dolby_surround_mode: cli.dolby_surround_mode.map(Into::into),
        dolby_headphone_mode: cli.dolby_headphone_mode.map(Into::into),
        dolby_surround_ex_mode: cli.dolby_surround_ex_mode.map(Into::into),
        ad_converter_type: cli.ad_converter_type.map(Into::into),
        preferred_stereo_downmix,
        ltrt_center_mix_level: levels[0],
        ltrt_surround_mix_level: levels[1],
        loro_center_mix_level: levels[2],
        loro_surround_mix_level: levels[3],
    };

    let options = EncodeOptions {
        pan: cli.pan.into(),
        gain_db: cli.gain_db,
        surround_phase_shift: !cli.no_surround_phase_shift,
        core: CoreOptions {
            bitrate: cli.core_bitrate,
            dialnorm: cli.dialnorm,
            coupling: cli.coupling.into(),
            coupling_start: cli.coupling_start,
            stereo_rematrixing: !cli.no_stereo_rematrixing,
            block_switch: !cli.no_block_switch,
            aht: cli.aht,
            cutoff_hz: cli.cutoff_hz,
            metadata,
        },
        joc: JocOptions {
            lambda: cli.joc_lambda,
            ridge: cli.joc_ridge,
            emdf_budget: cli.emdf_budget,
            select: cli.joc_select.into(),
            mode: cli.joc_mode.into(),
            quant: cli.joc_quant.into(),
            refine_rounds: cli.joc_refine_rounds,
            rd_passes: cli.joc_rd_passes,
            smooth: cli.joc_smooth,
            power_gate: cli.joc_power_gate,
            target_block: cli.emdf_target_block,
            addbsi_complexity: cli.addbsi_complexity,
            dynrng_profile: cli.drc_dynrng_profile,
            compr_profile: cli.drc_compr_profile,
        },
        emdf_key,
        emdf_key_id: cli.emdf_key_id,
    };
    eac3joc_encoder::encode(&cli.master, &cli.out, &options)?;
    Ok(())
}

/// `ddpmeta` binary key-set magic (version 1).
const KEY_MAGIC: [u8; 8] = [0x89, b'E', b'M', b'K', 0x0D, 0x0A, 0x1A, 0x0A];

fn parse_key_set(bytes: &[u8], key_id: u8) -> Result<Vec<u8>> {
    anyhow::ensure!(
        bytes.len() >= 10 && bytes[..8] == KEY_MAGIC,
        "bad EMDF key file magic"
    );
    anyhow::ensure!(
        bytes[8] == 1,
        "unsupported EMDF key file version {}",
        bytes[8]
    );
    let count = bytes[9] as usize;
    let mut pos = 10usize;
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        anyhow::ensure!(pos + 6 <= bytes.len(), "truncated EMDF key file");
        let id = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        let len = u16::from_le_bytes(bytes[pos + 4..pos + 6].try_into().unwrap()) as usize;
        pos += 6;
        anyhow::ensure!(pos + len <= bytes.len(), "truncated EMDF key");
        ids.push(id);
        if id == u32::from(key_id) {
            anyhow::ensure!(len > 0, "empty EMDF key");
            return Ok(bytes[pos..pos + len].to_vec());
        }
        pos += len;
    }
    anyhow::bail!("EMDF key_id {key_id} not found (available: {ids:?})");
}

fn load_emdf_key(value: &str, key_id: u8) -> Result<Vec<u8>> {
    let Some(path) = value.strip_prefix('@') else {
        return parse_emdf_key(value);
    };
    let bytes = std::fs::read(path).with_context(|| format!("reading key file {path}"))?;
    if bytes.starts_with(&KEY_MAGIC) {
        return parse_key_set(&bytes, key_id);
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("key file {path} is neither binary nor hex text"))?;
    parse_emdf_key(&text)
}

fn parse_emdf_key(value: &str) -> Result<Vec<u8>> {
    let hex: String = value.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    anyhow::ensure!(
        !hex.is_empty() && hex.len().is_multiple_of(2),
        "key must be non-empty, even-length hex"
    );
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(Into::into))
        .collect()
}
