//! Bed/channel identity and the JOC core-lane plan.
//!
//! It decides which decoded bed channel feeds each JOC input lane; it never
//! touches the DSP.

use crate::error::{Eac3JocError, Result};
use crate::joc::{MAX_CORE_CHANNELS, num_channels_for_config};
use crate::transport::{Codec, SyncFrameHeader};

pub use objectaudio_ir::SpeakerPosition as Position;

pub const fn position_bit(position: Position) -> u32 {
    match position {
        Position::Left => 0x1,
        Position::Right => 0x2,
        Position::Center => 0x4,
        Position::Lfe => 0x8,
        Position::LeftRearSurround => 0x10,
        Position::RightRearSurround => 0x20,
        Position::CenterSurround => 0x100,
        Position::LeftSurround => 0x200,
        Position::RightSurround => 0x400,
        Position::TopSurround => 0x800,
        Position::VerticalHeightLeft => 0x1000,
        Position::VerticalHeightCenter => 0x2000,
        Position::VerticalHeightRight => 0x4000,
        Position::TopSurroundLeft => 0x8000,
        Position::TopSurroundRight => 0x20000,
        _ => 0,
    }
}

pub fn position_name(position: Position) -> &'static str {
    position.short_name()
}

/// Table E2.5 expansion; the field's most significant bit is table bit 0.
#[allow(clippy::needless_range_loop)]
pub fn expand_chanmap(chanmap: u16, out: &mut Vec<Position>) {
    const SINGLES: [Position; 16] = [
        Position::Left,
        Position::Center,
        Position::Right,
        Position::LeftSurround,
        Position::RightSurround,
        Position::None,
        Position::None,
        Position::CenterSurround,
        Position::TopSurround,
        Position::None,
        Position::None,
        Position::None,
        Position::VerticalHeightCenter,
        Position::None,
        Position::Lfe2,
        Position::Lfe,
    ];
    for bit in 0..16 {
        if (chanmap >> (15 - bit)) & 1 == 0 {
            continue;
        }
        match bit {
            5 => {
                out.push(Position::LeftSurround);
                out.push(Position::RightSurround);
            }
            6 => {
                out.push(Position::LeftRearSurround);
                out.push(Position::RightRearSurround);
            }
            9 | 10 => {}
            11 => {
                out.push(Position::VerticalHeightLeft);
                out.push(Position::VerticalHeightRight);
            }
            13 => {
                out.push(Position::TopSurroundLeft);
                out.push(Position::TopSurroundRight);
            }
            _ => out.push(SINGLES[bit]),
        }
    }
}

/// A/52 `acmod` -> full-band positions in decoding order (LFE excluded).
pub fn acmod_positions(acmod: u8, out: &mut Vec<Position>) {
    match acmod {
        0 | 1 => out.push(Position::Center),
        2 => {
            out.push(Position::Left);
            out.push(Position::Right);
        }
        3 => {
            out.push(Position::Left);
            out.push(Position::Center);
            out.push(Position::Right);
        }
        4 => {
            out.push(Position::Left);
            out.push(Position::Right);
            out.push(Position::CenterSurround);
        }
        5 => {
            out.push(Position::Left);
            out.push(Position::Center);
            out.push(Position::Right);
            out.push(Position::CenterSurround);
        }
        6 => {
            out.push(Position::Left);
            out.push(Position::Right);
            out.push(Position::LeftSurround);
            out.push(Position::RightSurround);
        }
        _ => {
            out.push(Position::Left);
            out.push(Position::Center);
            out.push(Position::Right);
            out.push(Position::LeftSurround);
            out.push(Position::RightSurround);
        }
    }
}

/// Full-band channel count declared by an A/52 `acmod`.
///
/// Derived from [`acmod_positions`] so the parser and the decoder cannot
/// disagree about the table.
pub fn acmod_channel_count(acmod: u8) -> usize {
    let mut positions = Vec::with_capacity(5);
    acmod_positions(acmod, &mut positions);
    positions.len()
}

/// The extra pair a `joc_dmx_config_idx` declares; empty for configs 0 and 3.
pub fn expected_pair(dmx_config_idx: u8, out: &mut Vec<Position>) {
    if dmx_config_idx == 1 {
        out.push(Position::LeftRearSurround);
        out.push(Position::RightRearSurround);
    } else if dmx_config_idx == 2 || dmx_config_idx == 4 {
        out.push(Position::VerticalHeightLeft);
        out.push(Position::VerticalHeightRight);
    }
}

pub fn chanmap_extra_pair(core: &[Position], chanmap: u16, out: &mut Vec<Position>) {
    let mut declared = Vec::new();
    expand_chanmap(chanmap, &mut declared);
    for position in declared {
        if !core.contains(&position) {
            push_unique(out, position);
        }
    }
}

fn push_unique(positions: &mut Vec<Position>, position: Position) {
    if position != Position::None && !positions.contains(&position) {
        positions.push(position);
    }
}

pub fn bed_channels_for_core(core_channels: usize) -> usize {
    if core_channels == 7 { 8 } else { 6 }
}

/// `bed[lane] = decoded PCM channel index feeding JOC lane 'lane'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BedPlan {
    pub channels: usize,
    pub lfe_channel: Option<usize>,
    pub lanes: [Option<usize>; MAX_CORE_CHANNELS],
    pub extra_pair_slot: [Option<usize>; 2],
}

impl Default for BedPlan {
    fn default() -> Self {
        Self {
            channels: 0,
            lfe_channel: None,
            lanes: [None; MAX_CORE_CHANNELS],
            extra_pair_slot: [None; 2],
        }
    }
}

const EXTRA_PAIR_SLOTS: [[i8; 2]; 5] = [[-1, -1], [6, 7], [8, 9], [-1, -1], [8, 9]];

fn variant_error(message: impl Into<String>) -> Eac3JocError {
    Eac3JocError::UnsupportedVariant(message.into())
}

fn position_list(positions: &[Position]) -> String {
    let mut text = String::from("[");
    for (index, position) in positions.iter().enumerate() {
        if index != 0 {
            text.push(',');
        }
        text.push_str(position_name(*position));
    }
    text.push(']');
    text
}

fn sorted_unique(mut positions: Vec<Position>) -> Vec<Position> {
    positions.sort_by_key(|position| position_bit(*position));
    positions.dedup();
    positions
}

/// Bed plan from the actual substream headers.
pub fn compute_bed_plan(
    core: Option<&SyncFrameHeader>,
    dependent: Option<&SyncFrameHeader>,
    dmx_config_idx: u8,
) -> Result<BedPlan> {
    let core_channels = usize::from(
        num_channels_for_config(u32::from(dmx_config_idx)).ok_or_else(|| {
            variant_error(format!(
                "unsupported_configuration: unknown JOC downmix configuration {dmx_config_idx}"
            ))
        })?,
    );
    let mut core_positions = Vec::new();
    if let Some(core) = core {
        acmod_positions(core.acmod, &mut core_positions);
        if core.lfeon {
            core_positions.push(Position::Lfe);
        }
    }
    let mut extra = Vec::new();
    let mut locations = core_positions.clone();
    if let Some(dependent) = dependent
        && let Some(chanmap) = dependent.chanmap
    {
        let mut declared = Vec::new();
        expand_chanmap(chanmap, &mut declared);
        chanmap_extra_pair(&core_positions, chanmap, &mut extra);
        locations.extend(declared);
    }
    let ordered = sorted_unique(locations);
    plan_from_ordered(ordered, &extra, core_channels, dmx_config_idx)
}

/// Bed plan driven by `joc_dmx_config_idx` alone, for callers without headers.
pub fn compute_bed_plan_for_config(dmx_config_idx: u8) -> Result<BedPlan> {
    let core_channels = usize::from(
        num_channels_for_config(u32::from(dmx_config_idx)).ok_or_else(|| {
            variant_error(format!(
                "unsupported_configuration: unknown JOC downmix configuration {dmx_config_idx}"
            ))
        })?,
    );
    let mut extra = Vec::new();
    expected_pair(dmx_config_idx, &mut extra);
    let mut ordered = vec![
        Position::Left,
        Position::Right,
        Position::Center,
        Position::Lfe,
        Position::LeftSurround,
        Position::RightSurround,
    ];
    ordered.extend(extra.iter().copied());
    let ordered = sorted_unique(ordered);
    plan_from_ordered(ordered, &extra, core_channels, dmx_config_idx)
}

fn plan_from_ordered(
    ordered: Vec<Position>,
    extra: &[Position],
    core_channels: usize,
    dmx_config_idx: u8,
) -> Result<BedPlan> {
    for position in &ordered {
        if position_bit(*position) == 0 {
            return Err(variant_error(format!(
                "bed_channel_mapping_unsupported: unmapped channel position {}",
                position_name(*position)
            )));
        }
    }
    let expected_channels = bed_channels_for_core(core_channels);
    if ordered.len() != expected_channels {
        return Err(variant_error(format!(
            "core_channel_count_mismatch: the bed declares {} channels, configuration {dmx_config_idx} needs {expected_channels}",
            ordered.len()
        )));
    }
    let mut plan = BedPlan {
        channels: ordered.len(),
        ..BedPlan::default()
    };
    if let Some(index) = ordered
        .iter()
        .position(|position| *position == Position::Lfe)
    {
        plan.lfe_channel = Some(index);
    }

    let mut wanted = vec![
        Position::Left,
        Position::Right,
        Position::Center,
        Position::LeftSurround,
        Position::RightSurround,
    ];
    wanted.extend_from_slice(extra);
    if wanted.len() != core_channels {
        return Err(variant_error(format!(
            "chanmap_cfg_mismatch: the bed cannot supply {core_channels} JOC core channels (found {})",
            wanted.len()
        )));
    }
    for (lane, position) in wanted.iter().enumerate() {
        let found = ordered.iter().position(|candidate| candidate == position);
        match found {
            Some(index) => plan.lanes[lane] = Some(index),
            None => {
                return Err(variant_error(format!(
                    "bed_channel_mapping_unsupported: channel {} is missing from the bed {}",
                    position_name(*position),
                    position_list(&ordered)
                )));
            }
        }
    }
    if dmx_config_idx < 5 {
        let slots = EXTRA_PAIR_SLOTS[dmx_config_idx as usize];
        plan.extra_pair_slot = [
            (slots[0] >= 0).then_some(slots[0] as usize),
            (slots[1] >= 0).then_some(slots[1] as usize),
        ];
    }
    Ok(plan)
}

/// Whether a decoded substream header is the independent core substream.
pub fn is_core_stream(header: &SyncFrameHeader) -> bool {
    header.codec == Codec::Ac3 || header.stream_type == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(acmod: u8, lfeon: bool) -> SyncFrameHeader {
        SyncFrameHeader {
            codec: Codec::Eac3,
            stream_type: 0,
            substream_id: 0,
            acmod,
            lfeon,
            bsid: 16,
            sample_rate: 48_000,
            num_blocks_code: 3,
            chanmap: None,
            size: 0,
        }
    }

    #[test]
    fn acmod_channel_count_is_derived_from_the_position_table() {
        assert_eq!(acmod_channel_count(0), 1);
        assert_eq!(acmod_channel_count(1), 1);
        assert_eq!(acmod_channel_count(2), 2);
        assert_eq!(acmod_channel_count(3), 3);
        assert_eq!(acmod_channel_count(4), 3);
        assert_eq!(acmod_channel_count(5), 4);
        assert_eq!(acmod_channel_count(6), 4);
        assert_eq!(acmod_channel_count(7), 5);
    }

    #[test]
    fn config_three_is_5_1() {
        let plan = compute_bed_plan(Some(&header(7, true)), None, 3).unwrap();
        assert_eq!(plan.channels, 6);
        assert_eq!(plan.lfe_channel, Some(3));
        assert_eq!(plan.lanes[0], Some(0)); // L
        assert_eq!(plan.lanes[4], Some(5)); // Rs
        assert_eq!(plan.extra_pair_slot, [None, None]);
    }

    #[test]
    fn config_one_uses_the_chanmap_pair() {
        let mut dependent = header(7, false);
        dependent.stream_type = 1;
        dependent.chanmap = Some(0x0200); // Lrs/Rrs (Table E2.5 bit 6)
        let plan = compute_bed_plan(Some(&header(7, true)), Some(&dependent), 1).unwrap();
        assert_eq!(plan.channels, 8);
        assert_eq!(plan.lanes[5], Some(4)); // Lrs
        assert_eq!(plan.lanes[6], Some(5)); // Rrs
        assert_eq!(plan.extra_pair_slot, [Some(6), Some(7)]);
    }

    #[test]
    fn config_alone_matches_the_core_header_path() {
        let from_config = compute_bed_plan_for_config(3).unwrap();
        let from_header = compute_bed_plan(Some(&header(7, true)), None, 3).unwrap();
        assert_eq!(from_config.channels, from_header.channels);
        assert_eq!(from_config.lanes, from_header.lanes);
        assert_eq!(from_config.lfe_channel, from_header.lfe_channel);
    }

    #[test]
    fn chanmap_expansion_splits_pairs() {
        let mut positions = Vec::new();
        expand_chanmap(0x6000, &mut positions); // bits 1 and 2
        assert_eq!(positions, vec![Position::Center, Position::Right]);
    }
}
