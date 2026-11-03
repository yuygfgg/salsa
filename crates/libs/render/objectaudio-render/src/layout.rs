//! Speaker layout tables.

use crate::speaker::SpeakerChannel;

use crate::layouts::LAYOUTS;

/// Maximum number of render points in one region.
pub const MAX_POINTS: usize = 15;
/// Maximum number of axis groups per axis.
pub const MAX_GROUPS: usize = 4;
/// Maximum number of supported output channels.
pub const MAX_CHANNELS: usize = 16;

/// One speaker position in Q15 coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakerPoint {
    pub coordinate_q15: [u16; 3],
    pub speaker_id: u8,
}

/// A mono or stereo speaker group on one panning axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisGroup {
    pub size: u8,
    pub indices: [u8; 3],
}

/// Panning geometry for one elevation region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionGeometry {
    pub point_count: u8,
    pub mode: u8,
    pub axis0_group_count: u8,
    pub axis1_group_count: u8,
    pub points: [SpeakerPoint; MAX_POINTS],
    pub axis0_groups: [AxisGroup; MAX_GROUPS],
    pub axis1_groups: [AxisGroup; MAX_GROUPS],
}

/// A standard speaker layout and its renderer geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakerLayout {
    pub name: &'static str,
    pub out_ch_config: u8,
    pub speaker_bitfield: u32,
    pub channel_count: u8,
    /// Internal renderer channel -> standard output channel.
    pub standard_from_internal: [u8; 16],
    /// Standard channels in output order.
    pub channels: &'static [SpeakerChannel],
    pub regions: [RegionGeometry; 7],
}

impl SpeakerLayout {
    pub fn channel_count(&self) -> usize {
        usize::from(self.channel_count)
    }

    pub fn has_lfe(&self) -> bool {
        self.speaker_bitfield & 0x4 != 0
    }

    /// Ls/Rs/Lrs/Rrs, the labels the OAMD trim configuration index counts.
    pub fn surround_channels(&self) -> u32 {
        self.channels
            .iter()
            .filter(|channel| {
                matches!(
                    channel,
                    SpeakerChannel::LeftSurround
                        | SpeakerChannel::RightSurround
                        | SpeakerChannel::LeftRearSurround
                        | SpeakerChannel::RightRearSurround
                )
            })
            .count() as u32
    }

    /// Ltf/Rtf/Ltm/Rtm/Ltr/Rtr, the labels the OAMD trim configuration index
    /// counts.
    pub fn height_channels(&self) -> u32 {
        self.channels
            .iter()
            .filter(|channel| {
                matches!(
                    channel,
                    SpeakerChannel::TopFrontLeft
                        | SpeakerChannel::TopFrontRight
                        | SpeakerChannel::TopSideLeft
                        | SpeakerChannel::TopSideRight
                        | SpeakerChannel::TopBackLeft
                        | SpeakerChannel::TopBackRight
                )
            })
            .count() as u32
    }

    /// One of the seven layouts that carry OAMD trim and front-back balance
    /// semantics; wider layouts leave the programme untrimmed.
    pub fn carries_trim(&self) -> bool {
        matches!(
            self.name,
            "2.0" | "3.1" | "4.0" | "5.1" | "7.1" | "5.1.2" | "5.1.4"
        )
    }

    /// Index of `channel` in the standard output order.
    pub fn standard_index(&self, channel: SpeakerChannel) -> Option<usize> {
        self.channels
            .iter()
            .position(|candidate| *candidate == channel)
    }
}

/// All supported layouts.
pub fn layouts() -> &'static [SpeakerLayout] {
    &LAYOUTS
}

pub fn layout_by_name(name: &str) -> Option<&'static SpeakerLayout> {
    LAYOUTS.iter().find(|layout| layout.name == name)
}

pub fn layout_by_out_ch_config(config: u8) -> Option<&'static SpeakerLayout> {
    LAYOUTS.iter().find(|layout| layout.out_ch_config == config)
}

pub fn layout_by_speaker_bitfield(bitfield: u32) -> Option<&'static SpeakerLayout> {
    LAYOUTS
        .iter()
        .find(|layout| layout.speaker_bitfield == bitfield)
}

pub fn layout_by_channel_count(count: usize) -> Option<&'static SpeakerLayout> {
    LAYOUTS
        .iter()
        .find(|layout| layout.channel_count() == count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_layout() {
        let names: Vec<&str> = layouts().iter().map(|layout| layout.name).collect();
        assert_eq!(
            names,
            vec![
                "2.0", "3.1", "4.0", "5.1", "7.1", "5.1.2", "5.1.4", "7.1.2", "7.1.4", "9.1.4",
                "9.1.6"
            ]
        );
    }

    #[test]
    fn channel_counts_match_the_table() {
        let expected = [2, 4, 4, 6, 8, 8, 10, 10, 12, 14, 16];
        for (layout, count) in layouts().iter().zip(expected) {
            assert_eq!(layout.channel_count(), count, "{}", layout.name);
            assert_eq!(layout.channels.len(), count, "{}", layout.name);
        }
    }

    #[test]
    fn lookups_are_consistent() {
        for layout in layouts() {
            assert_eq!(layout_by_name(layout.name), Some(layout));
            assert_eq!(
                layout_by_out_ch_config(layout.out_ch_config),
                Some(layout),
                "{}",
                layout.name
            );
            assert_eq!(
                layout_by_speaker_bitfield(layout.speaker_bitfield),
                Some(layout),
                "{}",
                layout.name
            );
        }
    }

    #[test]
    fn standard_channel_orders_match_the_table() {
        assert_eq!(
            layout_by_name("7.1.4").unwrap().channels,
            &[
                SpeakerChannel::Left,
                SpeakerChannel::Right,
                SpeakerChannel::Center,
                SpeakerChannel::Lfe,
                SpeakerChannel::LeftSurround,
                SpeakerChannel::RightSurround,
                SpeakerChannel::LeftRearSurround,
                SpeakerChannel::RightRearSurround,
                SpeakerChannel::TopFrontLeft,
                SpeakerChannel::TopFrontRight,
                SpeakerChannel::TopBackLeft,
                SpeakerChannel::TopBackRight,
            ]
        );
        assert_eq!(
            layout_by_name("4.0").unwrap().channels,
            &[
                SpeakerChannel::Left,
                SpeakerChannel::Right,
                SpeakerChannel::LeftRearSurround,
                SpeakerChannel::RightRearSurround,
            ]
        );
    }

    #[test]
    fn names_and_members_come_from_the_ir_layout_table() {
        for layout in layouts() {
            assert_eq!(
                objectaudio_ir::layout_name_for(layout.channels),
                Some(layout.name),
                "{}",
                layout.name
            );
        }
    }

    #[test]
    fn standard_from_internal_is_a_permutation() {
        for layout in layouts() {
            let mut seen = [false; MAX_CHANNELS];
            let count = layout.channel_count();
            for internal in 0..count {
                let standard = usize::from(layout.standard_from_internal[internal]);
                assert!(standard < count, "{}", layout.name);
                assert!(!seen[standard], "{}", layout.name);
                seen[standard] = true;
            }
        }
    }
}
