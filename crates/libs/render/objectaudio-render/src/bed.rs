//! Source bed coordinates and output speaker anchors.

use crate::layout::SpeakerLayout;
use crate::speaker::SpeakerChannel;

/// Q15 source position of a bed channel converted to a fixed object.
///
/// Uses the single ADM/Q15 conversion in `objectaudio-ir`.
pub fn bed_channel_q15(channel: SpeakerChannel) -> Option<[u16; 3]> {
    Some(objectaudio_ir::adm_to_q15(channel.adm_position()))
}

/// Anchor a speaker label to the same-named speaker of `layout`.
pub fn layout_bed_anchor(layout: &SpeakerLayout, channel: SpeakerChannel) -> Option<[u16; 3]> {
    layout.speaker_position_q15(channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout_by_name;

    #[test]
    fn bed_positions_match_the_canonical_conversion() {
        assert_eq!(bed_channel_q15(SpeakerChannel::Left), Some([0, 0, 0]));
        assert_eq!(bed_channel_q15(SpeakerChannel::Right), Some([32767, 0, 0]));
        assert_eq!(bed_channel_q15(SpeakerChannel::Center), Some([16384, 0, 0]));
        assert_eq!(
            bed_channel_q15(SpeakerChannel::LeftSurround),
            Some([0, 16384, 0])
        );
    }

    #[test]
    fn anchors_use_the_target_layout_speaker() {
        let layout = layout_by_name("7.1.4").unwrap();
        assert_eq!(
            layout_bed_anchor(layout, SpeakerChannel::LeftRearSurround),
            Some([0, 32767, 0])
        );
        assert_eq!(
            layout_bed_anchor(layout, SpeakerChannel::LeftSurround),
            Some([0, 16384, 0])
        );
        assert_eq!(
            layout_bed_anchor(layout, SpeakerChannel::TopFrontLeft),
            Some([7928, 7928, 32767])
        );
        assert_eq!(layout_bed_anchor(layout, SpeakerChannel::Lfe), None);
        assert_eq!(layout_bed_anchor(layout, SpeakerChannel::Lfe2), None);
    }

    #[test]
    fn a_missing_speaker_has_no_anchor() {
        let layout = layout_by_name("2.0").unwrap();
        assert_eq!(layout_bed_anchor(layout, SpeakerChannel::Center), None);
    }
}
