//! Speaker-bed declarations from the OAMD wire format.
//!
//! The semantic positions live in `objectaudio-ir`; this module only knows the
//! OAMD wire index of each bed channel.

pub use objectaudio_ir::{BED_CHANNEL_COUNT, BED_CHANNELS, SpeakerPosition};

/// A speaker-anchored bed channel.
pub type BedChannel = SpeakerPosition;

/// The content bed declared by the bitstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BedLayout {
    pub present: bool,
    pub bed_only: bool,
    pub complete: bool,
    pub mask: u32,
    pub name: Option<&'static str>,
    /// Non-LFE bed channels, in OAMD Table 12 order.
    pub channels: Vec<BedChannel>,
}

impl BedLayout {
    pub fn from_mask(mask: u32, bed_only: bool) -> Self {
        let mask = mask & ((1u32 << BED_CHANNEL_COUNT) - 1);
        let members = BED_CHANNELS
            .iter()
            .copied()
            .filter(|channel| (mask & channel.bed_bit()) != 0)
            .collect::<Vec<_>>();
        let channels = members
            .iter()
            .copied()
            .filter(|channel| !channel.is_lfe())
            .collect::<Vec<_>>();
        let name = objectaudio_ir::layout_name_for(&members);
        Self {
            present: mask != 0,
            bed_only,
            complete: name.is_some(),
            mask,
            name,
            channels,
        }
    }

    pub fn contains(&self, channel: BedChannel) -> bool {
        self.mask & channel.bed_bit() != 0
    }

    pub fn iter(&self) -> impl Iterator<Item = BedChannel> + '_ {
        self.channels.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_standard_layouts() {
        let mask = [
            BedChannel::Left,
            BedChannel::Right,
            BedChannel::LeftSurround,
            BedChannel::RightSurround,
            BedChannel::Center,
            BedChannel::Lfe,
        ]
        .into_iter()
        .fold(0, |mask, channel| mask | channel.bed_bit());
        let bed = BedLayout::from_mask(mask, true);
        assert_eq!(bed.name, Some("5.1"));
        assert!(bed.present);
        assert!(bed.complete);
        assert!(bed.bed_only);
        assert!(bed.contains(BedChannel::Lfe));
        assert!(!bed.contains(BedChannel::Lfe2));
    }

    #[test]
    fn lfe_only_bed_is_not_named() {
        let bed = BedLayout::from_mask(BedChannel::Lfe.bed_bit(), false);
        assert!(bed.present);
        assert_eq!(bed.name, None);
        assert!(!bed.complete);
        assert!(bed.channels.is_empty());
    }

    #[test]
    fn a_side_only_quad_is_not_the_canonical_4_0() {
        let mask = [
            BedChannel::Left,
            BedChannel::Right,
            BedChannel::LeftSurround,
            BedChannel::RightSurround,
        ]
        .into_iter()
        .fold(0, |mask, channel| mask | channel.bed_bit());
        assert_eq!(BedLayout::from_mask(mask, true).name, None);
    }

    #[test]
    fn positions_match_the_canonical_table() {
        assert_eq!(BedChannel::Left.adm_position(), [-1.0, 1.0, 0.0]);
        assert_eq!(BedChannel::TopSideRight.adm_position(), [1.0, 0.0, 1.0]);
    }
}
