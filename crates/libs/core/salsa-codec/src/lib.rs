//! Codec identity and timing data shared by every Salsa app.
//!
//! The carrier, decoder and UI layers used to keep independent `Codec` enums
//! and delay constants. Keeping them here means a codec is described in one
//! place without pulling GStreamer into the generic libraries.

/// A compressed audio codec understood by the Salsa pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Eac3,
    TrueHd,
    SalsaSpatial,
}

impl Codec {
    pub const ALL: [Self; 3] = [Self::Eac3, Self::TrueHd, Self::SalsaSpatial];

    /// Lower-case configuration/CLI name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Eac3 => "eac3",
            Self::TrueHd => "truehd",
            Self::SalsaSpatial => "salsa-spatial",
        }
    }

    /// Short display name.
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Eac3 => "EC3",
            Self::TrueHd => "TrueHD",
            Self::SalsaSpatial => "salsa-spatial",
        }
    }

    /// Parse a configuration or CLI value. Accepts the common file extensions
    /// in addition to the canonical names.
    pub fn from_label(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "eac3" | "ec3" => Some(Self::Eac3),
            "truehd" | "thd" | "mlp" => Some(Self::TrueHd),
            "salsa-spatial" | "salsaspatial" | "spatial" => Some(Self::SalsaSpatial),
            _ => None,
        }
    }

    /// Static timing description for this codec.
    pub const fn spec(self) -> CodecSpec {
        match self {
            Self::Eac3 => EAC3_SPEC,
            Self::TrueHd => TRUEHD_SPEC,
            Self::SalsaSpatial => SALSA_SPATIAL_SPEC,
        }
    }

    /// Decoder signal delay, in samples, folded into the timestamps.
    pub const fn signal_delay_samples(self) -> u32 {
        self.spec().signal_delay_samples
    }

    /// Parser lookahead, in samples, needed before an access unit can be
    /// emitted. This is a scheduling requirement, not a signal delay.
    pub const fn parser_lookahead_samples(self) -> u32 {
        self.spec().parser_lookahead_samples
    }
}

/// Static description of a codec's timing and framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecSpec {
    pub codec: Codec,
    pub label: &'static str,
    pub signal_delay_samples: u32,
    pub parser_lookahead_samples: u32,
    pub frame_samples: u32,
    pub sample_rate: u32,
}

/// Decoder delay of the E-AC-3 / JOC path, in 48 kHz samples.
pub const EAC3_DECODER_DELAY: u32 = 1473;
/// One E-AC-3 audio frame: 6 blocks x 256 samples.
pub const EAC3_FRAME_SAMPLES: u32 = 1536;
/// Decode sample rate used by every Salsa renderer.
pub const SAMPLE_RATE: u32 = 48_000;
/// S/PDIF carrier sample rate used by the virtual device.
pub const CARRIER_SAMPLE_RATE: u32 = 192_000;

pub const EAC3_SPEC: CodecSpec = CodecSpec {
    codec: Codec::Eac3,
    label: "eac3",
    signal_delay_samples: EAC3_DECODER_DELAY,
    parser_lookahead_samples: EAC3_FRAME_SAMPLES,
    frame_samples: EAC3_FRAME_SAMPLES,
    sample_rate: SAMPLE_RATE,
};

pub const TRUEHD_SPEC: CodecSpec = CodecSpec {
    codec: Codec::TrueHd,
    label: "truehd",
    signal_delay_samples: 0,
    parser_lookahead_samples: 40,
    frame_samples: 40,
    sample_rate: SAMPLE_RATE,
};

/// One salsa-spatial parameter/Opus packet: 20 ms at 48 kHz.
pub const SALSA_SPATIAL_FRAME_SAMPLES: u32 = 960;
/// One complete salsa-spatial tile: five packets, 100 ms at 48 kHz.
pub const SALSA_SPATIAL_TILE_SAMPLES: u32 = 4800;

/// Salsa-spatial timing.
///
/// The transport parser needs a whole 100 ms tile before it can emit a frame.
/// The signal delay is zero here because the codec DSP owns the final sample
/// alignment and reports it from the decoder's sample clock; folding an extra
/// delay into the carrier timestamps would double-count it.
pub const SALSA_SPATIAL_SPEC: CodecSpec = CodecSpec {
    codec: Codec::SalsaSpatial,
    label: "salsa-spatial",
    signal_delay_samples: 0,
    parser_lookahead_samples: SALSA_SPATIAL_TILE_SAMPLES,
    frame_samples: SALSA_SPATIAL_FRAME_SAMPLES,
    sample_rate: SAMPLE_RATE,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip() {
        for codec in Codec::ALL {
            assert_eq!(Codec::from_label(codec.label()), Some(codec));
            assert_eq!(codec.spec().codec, codec);
        }
    }

    #[test]
    fn extensions_map_to_codecs() {
        assert_eq!(Codec::from_label("EC3"), Some(Codec::Eac3));
        assert_eq!(Codec::from_label("mlp"), Some(Codec::TrueHd));
        assert_eq!(Codec::from_label("IAMF"), None);
        assert_eq!(
            Codec::from_label("salsa-spatial"),
            Some(Codec::SalsaSpatial)
        );
        assert_eq!(Codec::from_label("flac"), None);
    }

    #[test]
    fn salsa_spatial_spec_matches_the_profile() {
        let spec = Codec::SalsaSpatial.spec();
        assert_eq!(spec.label, "salsa-spatial");
        assert_eq!(Codec::SalsaSpatial.short_label(), "salsa-spatial");
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.frame_samples, 960);
        assert_eq!(spec.parser_lookahead_samples, 4800);
        assert_eq!(spec.signal_delay_samples, 0);
    }
}
