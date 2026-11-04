#![forbid(unsafe_code)]
#![doc = "IEC 61937 carrier framing and Dolby E-AC-3 / TrueHD MAT extraction."]
//!
//! This crate is the platform-independent transport layer for the virtual S/PDIF
//! device. It owns burst framing, the carrier word swap, E-AC-3 syncframe
//! boundaries, TrueHD MAT removal and the relative carrier timeline. It has no
//! CoreAudio, GStreamer or FFmpeg dependency, so the same code can serve a
//! later Linux backend.
//!
//! The TrueHD MAT layout follows FFmpeg's `spdifenc.c`. FFmpeg's `spdifdec.c`
//! has no TrueHD path, so there is no upstream demuxer to reuse for that mode.

mod carrier;
mod eac3;
mod truehd;

pub use carrier::{
    Burst, BurstInfo, CARRIER_SAMPLE_RATE, CarrierStats, Codec, EAC3_BURST_BYTES,
    EAC3_CARRIER_BYTES_PER_FRAME, EAC3_DATA_TYPE, Event, Iec61937Parser, MalformedBurst,
    MalformedReason, NullBurst, SYNC1, SYNC2, TRUEHD_BURST_BYTES, TRUEHD_CARRIER_BYTES_PER_FRAME,
    TRUEHD_DATA_TYPE, UnsupportedBurst,
};
pub use eac3::{Eac3AccessUnit, Eac3Depacketizer, Eac3Stats};
pub use truehd::{
    MAT_END_CODE, MAT_FRAME_BYTES, MAT_MIDDLE_CODE, MAT_START_CODE, TrueHdAccessUnit,
    TrueHdDepacketizer, TrueHdStats,
};

/// A recovered codec access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessUnit {
    Eac3(Eac3AccessUnit),
    TrueHd(TrueHdAccessUnit),
}

impl AccessUnit {
    pub const fn generation(&self) -> u64 {
        match self {
            Self::Eac3(unit) => unit.generation,
            Self::TrueHd(unit) => unit.generation,
        }
    }

    pub const fn carrier_offset(&self) -> u64 {
        match self {
            Self::Eac3(unit) => unit.carrier_offset,
            Self::TrueHd(unit) => unit.carrier_offset,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Eac3(unit) => &unit.bytes,
            Self::TrueHd(unit) => &unit.bytes,
        }
    }
}

/// Convenience depacketizer that connects carrier framing to both codecs.
#[derive(Debug)]
pub struct Depacketizer {
    carrier: Iec61937Parser,
    eac3: Eac3Depacketizer,
    truehd: TrueHdDepacketizer,
    codec: Option<Codec>,
}

impl Depacketizer {
    /// `sample_rate` is the decoded TrueHD access-unit sample rate.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            carrier: Iec61937Parser::new(),
            eac3: Eac3Depacketizer::new(),
            truehd: TrueHdDepacketizer::new(sample_rate),
            codec: None,
        }
    }

    pub fn reset(&mut self) {
        self.carrier.reset();
        self.eac3.reset();
        self.truehd.reset();
        self.codec = None;
    }

    pub const fn generation(&self) -> u64 {
        self.carrier.generation()
    }

    pub const fn codec(&self) -> Option<Codec> {
        self.codec
    }

    pub const fn carrier_stats(&self) -> CarrierStats {
        self.carrier.stats()
    }

    pub const fn eac3_stats(&self) -> Eac3Stats {
        self.eac3.stats()
    }

    pub const fn truehd_stats(&self) -> TrueHdStats {
        self.truehd.stats()
    }

    /// Feed carrier bytes and return every complete access unit.
    ///
    /// Null, unsupported and malformed bursts are counted by the carrier parser
    /// but do not produce a codec access unit.
    pub fn push(&mut self, data: &[u8]) -> Vec<AccessUnit> {
        let mut units = Vec::new();
        for event in self.carrier.push(data) {
            let Event::Burst(burst) = event else {
                continue;
            };

            if self.codec != Some(burst.codec) {
                self.eac3.reset();
                self.truehd.reset();
                self.codec = Some(burst.codec);
            }

            match burst.codec {
                Codec::Eac3 => {
                    let offset = burst.info.carrier_offset + 8;
                    units.extend(
                        self.eac3
                            .push(&burst.payload, offset, burst.info.generation)
                            .into_iter()
                            .map(AccessUnit::Eac3),
                    );
                }
                Codec::TrueHd => {
                    let offset = burst.info.carrier_offset + 8;
                    units.extend(
                        self.truehd
                            .push_mat_frame(&burst.payload, offset, burst.info.generation)
                            .into_iter()
                            .map(AccessUnit::TrueHd),
                    );
                }
            }
        }
        units
    }
}
