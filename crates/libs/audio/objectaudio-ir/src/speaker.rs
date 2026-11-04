//! The single speaker/position model shared by decoders and renderers.
//!
//! Codec wire formats keep their own index tables, but every semantic
//! position and every ADM coordinate lives here.

/// Number of labelled OAMD bed channels (ETSI TS 103 420, Table 12).
pub const BED_CHANNEL_COUNT: usize = 17;

/// A standard loudspeaker position in ADM normalized Cartesian coordinates
/// (x right, y forward, z up).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum SpeakerPosition {
    Left = 0,
    Right,
    Center,
    Lfe,
    LeftSurround,
    RightSurround,
    LeftRearSurround,
    RightRearSurround,
    TopFrontLeft,
    TopFrontRight,
    TopSideLeft,
    TopSideRight,
    TopBackLeft,
    TopBackRight,
    LeftWide,
    RightWide,
    Lfe2,

    /// Rear centre (`acmod` 4/5, `Cs`).
    CenterSurround,
    /// Top centre (`Ts`).
    TopSurround,
    /// Left vertical height (`Vhl`).
    VerticalHeightLeft,
    /// Centre vertical height (`Vhc`).
    VerticalHeightCenter,
    /// Right vertical height (`Vhr`).
    VerticalHeightRight,
    /// Left top surround (`Lts`).
    TopSurroundLeft,
    /// Right top surround (`Rts`).
    TopSurroundRight,
    /// No speaker; used by the E-AC-3 chanmap expansion.
    None,
}

/// The OAMD bed channels in wire order. The first 17 [`SpeakerPosition`]
/// variants are exactly this table, so the wire index is the discriminant.
pub const BED_CHANNELS: [SpeakerPosition; BED_CHANNEL_COUNT] = [
    SpeakerPosition::Left,
    SpeakerPosition::Right,
    SpeakerPosition::Center,
    SpeakerPosition::Lfe,
    SpeakerPosition::LeftSurround,
    SpeakerPosition::RightSurround,
    SpeakerPosition::LeftRearSurround,
    SpeakerPosition::RightRearSurround,
    SpeakerPosition::TopFrontLeft,
    SpeakerPosition::TopFrontRight,
    SpeakerPosition::TopSideLeft,
    SpeakerPosition::TopSideRight,
    SpeakerPosition::TopBackLeft,
    SpeakerPosition::TopBackRight,
    SpeakerPosition::LeftWide,
    SpeakerPosition::RightWide,
    SpeakerPosition::Lfe2,
];

impl SpeakerPosition {
    /// Resolve a DAMF/OAMD bed name or a common speaker-name alias.
    /// Names are case-insensitive. Codec wire indices are resolved separately.
    pub fn from_bed_name(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_uppercase().as_str() {
            "L" => Self::Left,
            "R" => Self::Right,
            "C" => Self::Center,
            "LFE" => Self::Lfe,
            "LSS" | "LS" => Self::LeftSurround,
            "RSS" | "RS" => Self::RightSurround,
            "LRS" | "LB" => Self::LeftRearSurround,
            "RRS" | "RB" => Self::RightRearSurround,
            "LFH" | "TFL" | "LTF" => Self::TopFrontLeft,
            "RFH" | "TFR" | "RTF" => Self::TopFrontRight,
            "LTS" | "TSL" | "LTM" => Self::TopSideLeft,
            "RTS" | "TSR" | "RTM" => Self::TopSideRight,
            "LRH" | "TBL" | "TRL" | "LTR" => Self::TopBackLeft,
            "RRH" | "TBR" | "TRR" | "RTR" => Self::TopBackRight,
            "LW" => Self::LeftWide,
            "RW" => Self::RightWide,
            "LFE2" => Self::Lfe2,
            _ => return None,
        })
    }

    /// Wire index of an OAMD bed channel, or `None` for a non-bed position.
    pub const fn bed_index(self) -> Option<usize> {
        if (self as usize) < BED_CHANNEL_COUNT {
            Some(self as usize)
        } else {
            None
        }
    }

    pub const fn from_bed_index(index: usize) -> Option<Self> {
        if index < BED_CHANNEL_COUNT {
            Some(BED_CHANNELS[index])
        } else {
            None
        }
    }

    /// Single-bit mask for an OAMD bed channel.
    pub const fn bed_bit(self) -> u32 {
        match self.bed_index() {
            Some(index) => 1u32 << index,
            None => 0,
        }
    }

    pub const fn is_bed(self) -> bool {
        self.bed_index().is_some()
    }

    pub const fn is_lfe(self) -> bool {
        matches!(self, Self::Lfe | Self::Lfe2)
    }

    /// Source channel position for conversion to a fixed object.
    /// Bed coordinates follow ETSI TS 103 420, Table B.10 (x right, y forward,
    /// z up). These coordinates do not define output speaker geometry.
    /// LFE channels use a separate signal path; their coordinates are placeholders.
    pub const fn adm_position(self) -> [f32; 3] {
        match self {
            Self::Left => [-1.0, 1.0, 0.0],
            Self::Right => [1.0, 1.0, 0.0],
            Self::Center => [0.0, 1.0, 0.0],
            Self::Lfe => [0.0, 0.0, 0.0],
            Self::LeftSurround => [-1.0, 0.0, 0.0],
            Self::RightSurround => [1.0, 0.0, 0.0],
            Self::LeftRearSurround => [-1.0, -1.0, 0.0],
            Self::RightRearSurround => [1.0, -1.0, 0.0],
            Self::TopFrontLeft => [-1.0, 1.0, 1.0],
            Self::TopFrontRight => [1.0, 1.0, 1.0],
            Self::TopSideLeft => [-1.0, 0.0, 1.0],
            Self::TopSideRight => [1.0, 0.0, 1.0],
            Self::TopBackLeft => [-1.0, -1.0, 1.0],
            Self::TopBackRight => [1.0, -1.0, 1.0],
            Self::LeftWide => [-1.0, 21.0 / 31.0, 0.0],
            Self::RightWide => [1.0, 21.0 / 31.0, 0.0],
            Self::Lfe2 => [0.0, 0.0, 0.0],
            Self::CenterSurround => [0.0, -1.0, 0.0],
            Self::TopSurround => [0.0, 0.0, 1.0],
            Self::VerticalHeightLeft => [-1.0, 1.0, 1.0],
            Self::VerticalHeightCenter => [0.0, 1.0, 1.0],
            Self::VerticalHeightRight => [1.0, 1.0, 1.0],
            Self::TopSurroundLeft => [-1.0, 0.0, 1.0],
            Self::TopSurroundRight => [1.0, 0.0, 1.0],
            Self::None => [0.0, 0.0, 0.0],
        }
    }

    pub const fn short_name(self) -> &'static str {
        match self {
            Self::Left => "L",
            Self::Right => "R",
            Self::Center => "C",
            Self::Lfe => "LFE",
            Self::LeftSurround => "Ls",
            Self::RightSurround => "Rs",
            Self::LeftRearSurround => "Lrs",
            Self::RightRearSurround => "Rrs",
            Self::TopFrontLeft => "Ltf",
            Self::TopFrontRight => "Rtf",
            Self::TopSideLeft => "Ltm",
            Self::TopSideRight => "Rtm",
            Self::TopBackLeft => "Ltr",
            Self::TopBackRight => "Rtr",
            Self::LeftWide => "Lw",
            Self::RightWide => "Rw",
            Self::Lfe2 => "LFE2",
            Self::CenterSurround => "Cs",
            Self::TopSurround => "Ts",
            Self::VerticalHeightLeft => "Vhl",
            Self::VerticalHeightCenter => "Vhc",
            Self::VerticalHeightRight => "Vhr",
            Self::TopSurroundLeft => "Lts",
            Self::TopSurroundRight => "Rts",
            Self::None => "?",
        }
    }
}

/// A named standard speaker layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardLayout {
    pub name: &'static str,
    pub channels: &'static [SpeakerPosition],
}

/// Standard channel sets for bed declarations. Renderers can support a subset
/// and supply their own output geometry and PCM order.
pub const STANDARD_LAYOUTS: &[StandardLayout] = &[
    StandardLayout {
        name: "2.0",
        channels: &[SpeakerPosition::Left, SpeakerPosition::Right],
    },
    StandardLayout {
        name: "3.0",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
        ],
    },
    StandardLayout {
        name: "3.1",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
        ],
    },
    StandardLayout {
        name: "4.0",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
        ],
    },
    StandardLayout {
        name: "5.0",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
        ],
    },
    StandardLayout {
        name: "5.1",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
        ],
    },
    StandardLayout {
        name: "5.1.2",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::TopSideLeft,
            SpeakerPosition::TopSideRight,
        ],
    },
    StandardLayout {
        name: "5.1.4",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::TopFrontLeft,
            SpeakerPosition::TopFrontRight,
            SpeakerPosition::TopBackLeft,
            SpeakerPosition::TopBackRight,
        ],
    },
    StandardLayout {
        name: "7.1",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
        ],
    },
    StandardLayout {
        name: "7.1.2",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
            SpeakerPosition::TopSideLeft,
            SpeakerPosition::TopSideRight,
        ],
    },
    StandardLayout {
        name: "7.1.4",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
            SpeakerPosition::TopFrontLeft,
            SpeakerPosition::TopFrontRight,
            SpeakerPosition::TopBackLeft,
            SpeakerPosition::TopBackRight,
        ],
    },
    StandardLayout {
        name: "7.1.6",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
            SpeakerPosition::TopFrontLeft,
            SpeakerPosition::TopFrontRight,
            SpeakerPosition::TopSideLeft,
            SpeakerPosition::TopSideRight,
            SpeakerPosition::TopBackLeft,
            SpeakerPosition::TopBackRight,
        ],
    },
    StandardLayout {
        name: "9.1.2",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
            SpeakerPosition::LeftWide,
            SpeakerPosition::RightWide,
            SpeakerPosition::TopSideLeft,
            SpeakerPosition::TopSideRight,
        ],
    },
    StandardLayout {
        name: "9.1.4",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
            SpeakerPosition::LeftWide,
            SpeakerPosition::RightWide,
            SpeakerPosition::TopFrontLeft,
            SpeakerPosition::TopFrontRight,
            SpeakerPosition::TopBackLeft,
            SpeakerPosition::TopBackRight,
        ],
    },
    StandardLayout {
        name: "9.1.6",
        channels: &[
            SpeakerPosition::Left,
            SpeakerPosition::Right,
            SpeakerPosition::Center,
            SpeakerPosition::Lfe,
            SpeakerPosition::LeftSurround,
            SpeakerPosition::RightSurround,
            SpeakerPosition::LeftRearSurround,
            SpeakerPosition::RightRearSurround,
            SpeakerPosition::LeftWide,
            SpeakerPosition::RightWide,
            SpeakerPosition::TopFrontLeft,
            SpeakerPosition::TopFrontRight,
            SpeakerPosition::TopSideLeft,
            SpeakerPosition::TopSideRight,
            SpeakerPosition::TopBackLeft,
            SpeakerPosition::TopBackRight,
        ],
    },
];

/// Name of a standard layout whose member set equals `channels`.
pub fn layout_name_for(channels: &[SpeakerPosition]) -> Option<&'static str> {
    STANDARD_LAYOUTS
        .iter()
        .find(|layout| {
            layout.channels.len() == channels.len()
                && channels
                    .iter()
                    .all(|channel| layout.channels.contains(channel))
        })
        .map(|layout| layout.name)
}

/// ADM normalized Cartesian -> Q15 triple.
///
/// The rule is fixed here: clamp to the ADM range, scale by 32768, round half
/// away from zero, then clamp to 0..=32767. Negative elevation has no Q15
/// representation and is clamped to zero.
pub fn adm_to_q15(position: [f32; 3]) -> [u16; 3] {
    adm_to_q15_f64(position.map(f64::from))
}

/// [`adm_to_q15`] in `f64`, for renderers that keep intermediate precision.
pub fn adm_to_q15_f64(position: [f64; 3]) -> [u16; 3] {
    let quantise = |value: f64| {
        (value.clamp(0.0, 1.0) * 32768.0)
            .round()
            .clamp(0.0, 32767.0) as u16
    };
    let x = (position[0].clamp(-1.0, 1.0) + 1.0) * 0.5;
    let y = (1.0 - position[1].clamp(-1.0, 1.0)) * 0.5;
    let z = position[2].clamp(0.0, 1.0);
    [quantise(x), quantise(y), quantise(z)]
}

/// Q15 triple -> ADM normalized Cartesian.
pub fn q15_to_adm(position: [u16; 3]) -> [f64; 3] {
    let x = f64::from(position[0]) / 32768.0;
    let y = f64::from(position[1]) / 32768.0;
    let z = f64::from(position[2]) / 32768.0;
    [x * 2.0 - 1.0, 1.0 - y * 2.0, z]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bed_names_and_aliases_resolve_to_fixed_source_positions() {
        use SpeakerPosition as S;
        let cases: &[(S, &[&str], [f32; 3])] = &[
            (S::Left, &["L"], [-1.0, 1.0, 0.0]),
            (S::Right, &["R"], [1.0, 1.0, 0.0]),
            (S::Center, &["C"], [0.0, 1.0, 0.0]),
            (S::Lfe, &["LFE"], [0.0; 3]),
            (S::LeftSurround, &["Lss", "Ls"], [-1.0, 0.0, 0.0]),
            (S::RightSurround, &["Rss", "Rs"], [1.0, 0.0, 0.0]),
            (S::LeftRearSurround, &["Lrs", "Lb"], [-1.0, -1.0, 0.0]),
            (S::RightRearSurround, &["Rrs", "Rb"], [1.0, -1.0, 0.0]),
            (S::TopFrontLeft, &["Lfh", "Tfl", "Ltf"], [-1.0, 1.0, 1.0]),
            (S::TopFrontRight, &["Rfh", "Tfr", "Rtf"], [1.0, 1.0, 1.0]),
            (S::TopSideLeft, &["Lts", "Tsl", "Ltm"], [-1.0, 0.0, 1.0]),
            (S::TopSideRight, &["Rts", "Tsr", "Rtm"], [1.0, 0.0, 1.0]),
            (
                S::TopBackLeft,
                &["Lrh", "Tbl", "Trl", "Ltr"],
                [-1.0, -1.0, 1.0],
            ),
            (
                S::TopBackRight,
                &["Rrh", "Tbr", "Trr", "Rtr"],
                [1.0, -1.0, 1.0],
            ),
            (S::LeftWide, &["Lw"], [-1.0, 21.0 / 31.0, 0.0]),
            (S::RightWide, &["Rw"], [1.0, 21.0 / 31.0, 0.0]),
            (S::Lfe2, &["LFE2"], [0.0; 3]),
        ];
        assert_eq!(cases.len(), BED_CHANNEL_COUNT);
        for (channel, names, position) in cases {
            assert_eq!(channel.adm_position(), *position);
            for name in *names {
                for spelling in [
                    name.to_lowercase(),
                    name.to_uppercase(),
                    format!(" {name} "),
                ] {
                    assert_eq!(SpeakerPosition::from_bed_name(&spelling), Some(*channel));
                }
            }
        }
        assert_eq!(SpeakerPosition::from_bed_name("unknown"), None);
    }

    #[test]
    fn every_bed_channel_round_trips_through_its_wire_index() {
        for (index, channel) in BED_CHANNELS.iter().enumerate() {
            assert_eq!(channel.bed_index(), Some(index));
            assert_eq!(SpeakerPosition::from_bed_index(index), Some(*channel));
            assert_eq!(channel.bed_bit(), 1 << index);
        }
        assert_eq!(SpeakerPosition::from_bed_index(BED_CHANNEL_COUNT), None);
        assert_eq!(SpeakerPosition::CenterSurround.bed_index(), None);
    }

    #[test]
    fn layout_names_are_unique_and_use_the_member_set() {
        let mut names = std::collections::HashSet::new();
        for layout in STANDARD_LAYOUTS {
            assert!(names.insert(layout.name), "duplicate {}", layout.name);
            assert_eq!(layout_name_for(layout.channels), Some(layout.name));
        }
    }

    #[test]
    fn q15_round_trips_at_the_speaker_grid() {
        assert_eq!(adm_to_q15([-1.0, 1.0, 0.0]), [0, 0, 0]);
        assert_eq!(adm_to_q15([0.0, 1.0, 0.0]), [16384, 0, 0]);
        assert_eq!(adm_to_q15([1.0, 1.0, 0.0]), [32767, 0, 0]);
        assert_eq!(adm_to_q15([0.0, 0.0, 1.0]), [16384, 16384, 32767]);
        assert_eq!(q15_to_adm([16384, 0, 0])[1], 1.0);
        assert_eq!(q15_to_adm([0, 0, 0]), [-1.0, 1.0, 0.0]);
    }

    #[test]
    fn every_layout_member_is_a_known_speaker() {
        for layout in STANDARD_LAYOUTS {
            assert!(!layout.channels.is_empty(), "{}", layout.name);
            for channel in layout.channels {
                assert!(*channel != SpeakerPosition::None, "{}", layout.name);
            }
        }
    }
}
