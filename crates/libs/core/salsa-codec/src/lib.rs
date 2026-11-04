//! Codec identity and the timing/transport data shared by every Salsa app and
//! GStreamer plugin.
//!
//! The carrier, decoder and UI layers used to keep independent `Codec` enums,
//! parser/decoder names, caps strings and delay constants. Keeping them here
//! means a codec is described in one place.

/// A compressed audio codec understood by the Salsa pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Eac3,
    TrueHd,
}

impl Codec {
    pub const ALL: [Self; 2] = [Self::Eac3, Self::TrueHd];

    /// Lower-case configuration/CLI name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Eac3 => "eac3",
            Self::TrueHd => "truehd",
        }
    }

    /// Short display name.
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Eac3 => "EC3",
            Self::TrueHd => "TrueHD",
        }
    }

    /// Parse a configuration or CLI value. Accepts the common file extensions
    /// in addition to the canonical names.
    pub fn from_label(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "eac3" | "ec3" => Some(Self::Eac3),
            "truehd" | "thd" | "mlp" => Some(Self::TrueHd),
            _ => None,
        }
    }

    /// Static pipeline description for this codec.
    pub const fn spec(self) -> CodecSpec {
        match self {
            Self::Eac3 => EAC3_SPEC,
            Self::TrueHd => TRUEHD_SPEC,
        }
    }

    pub const fn appsrc_caps(self) -> &'static str {
        self.spec().caps
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

/// Static description of a codec's GStreamer transport and timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecSpec {
    pub codec: Codec,
    pub label: &'static str,
    pub caps: &'static str,
    pub parser: &'static str,
    pub decoder: &'static str,
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
    caps: "audio/x-eac3",
    parser: elements::EAC3_PARSE,
    decoder: elements::EAC3_DEC,
    signal_delay_samples: EAC3_DECODER_DELAY,
    parser_lookahead_samples: EAC3_FRAME_SAMPLES,
    frame_samples: EAC3_FRAME_SAMPLES,
    sample_rate: SAMPLE_RATE,
};

pub const TRUEHD_SPEC: CodecSpec = CodecSpec {
    codec: Codec::TrueHd,
    label: "truehd",
    caps: "audio/x-true-hd",
    parser: elements::TRUEHD_PARSE,
    decoder: elements::TRUEHD_DEC,
    signal_delay_samples: 0,
    parser_lookahead_samples: 40,
    frame_samples: 40,
    sample_rate: SAMPLE_RATE,
};

/// GStreamer element factory names registered by the Salsa plugins.
pub mod elements {
    pub const EAC3_PARSE: &str = "salsaeac3jocparse";
    pub const EAC3_DEC: &str = "salsaeac3jocdec";
    pub const TRUEHD_PARSE: &str = "salsatruehdparse";
    pub const TRUEHD_DEC: &str = "salsatruehdatmosdec";
    pub const OAR: &str = "salsaoar";
    pub const HRTF_RENDER: &str = "salsahrtfrender";
    pub const OBJECT_AUDIO_ADAPT: &str = "salsaobjectaudioadapt";
    pub const DAMF_SRC: &str = "salsadamfsrc";
    pub const DAMF_SINK: &str = "salsadamfsink";
}

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
        assert_eq!(Codec::from_label("flac"), None);
    }
}
