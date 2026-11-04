//! IEC 61937 burst framing.
//!
//! The parser consumes the carrier as emitted by the HAL/S/PDIF boundary: a
//! stream of little-endian 16-bit words. Payload bytes are returned in codec
//! byte order, which is the word-swapped order relative to the carrier. That
//! swap happens here and nowhere else.

pub const SYNC1: u16 = 0xF872;
pub const SYNC2: u16 = 0x4E1F;

/// IEC 61937 data type for E-AC-3.
pub const EAC3_DATA_TYPE: u8 = 21;
/// IEC 61937 data type for TrueHD in MAT.
pub const TRUEHD_DATA_TYPE: u8 = 22;

/// Burst repetition period for E-AC-3 (2 channels x 4 bytes x 6144 frames).
pub const EAC3_BURST_BYTES: usize = 24_576;
/// Burst repetition period for TrueHD MAT (8 channels x 2 bytes x 3840 frames).
pub const TRUEHD_BURST_BYTES: usize = 61_440;
/// Byte stride of one E-AC-3 carrier frame.
pub const EAC3_CARRIER_BYTES_PER_FRAME: u16 = 4;
/// Byte stride of one TrueHD MAT carrier frame.
pub const TRUEHD_CARRIER_BYTES_PER_FRAME: u16 = 16;
/// Carrier rate used by the inspected mpv/HAL path.
pub const CARRIER_SAMPLE_RATE: u32 = 192_000;

const BURST_HEADER_BYTES: usize = 8;
const MAX_PAYLOAD_BYTES: usize = TRUEHD_BURST_BYTES - BURST_HEADER_BYTES;

/// Compressed payload carried by an IEC 61937 burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Eac3,
    TrueHd,
}

impl Codec {
    /// Carrier data type that selects this codec.
    pub const fn data_type(self) -> u8 {
        match self {
            Self::Eac3 => EAC3_DATA_TYPE,
            Self::TrueHd => TRUEHD_DATA_TYPE,
        }
    }

    /// Burst repetition period in bytes.
    pub const fn burst_bytes(self) -> usize {
        match self {
            Self::Eac3 => EAC3_BURST_BYTES,
            Self::TrueHd => TRUEHD_BURST_BYTES,
        }
    }

    /// Bytes per carrier frame at the 192 kHz carrier rate.
    pub const fn carrier_bytes_per_frame(self) -> u16 {
        match self {
            Self::Eac3 => EAC3_CARRIER_BYTES_PER_FRAME,
            Self::TrueHd => TRUEHD_CARRIER_BYTES_PER_FRAME,
        }
    }

    /// Select a codec from an IEC 61937 data type.
    pub const fn from_data_type(data_type: u8) -> Option<Self> {
        match data_type {
            EAC3_DATA_TYPE => Some(Self::Eac3),
            TRUEHD_DATA_TYPE => Some(Self::TrueHd),
            _ => None,
        }
    }
}

/// Header fields shared by every recognised burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurstInfo {
    /// Absolute byte offset of the burst sync word in the carrier stream.
    pub carrier_offset: u64,
    /// Session generation that produced this burst.
    pub generation: u64,
    /// Raw `Pc` data type (bits 0..6), without the error flag.
    pub data_type: u8,
    /// `Pc` error flag (bit 7).
    pub error: bool,
    /// `Pc` data-type-dependent info (bits 8..12).
    pub data_type_dependent: u8,
    /// `Pc` bitstream number (bits 13..15).
    pub bitstream_number: u8,
    /// Raw `Pd` length code.
    pub length_code: u16,
}

/// A complete E-AC-3 or TrueHD burst with payload in codec byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Burst {
    pub info: BurstInfo,
    pub codec: Codec,
    /// Payload bytes after the carrier word swap.
    pub payload: Vec<u8>,
}

/// A pause/null burst (`data_type == 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullBurst {
    pub info: BurstInfo,
}

/// A syntactically valid burst whose data type is not handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedBurst {
    pub info: BurstInfo,
}

/// Why a burst could not be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedReason {
    /// The length code exceeds the burst repetition period.
    LengthExceedsBurst { capacity: usize },
    /// A compressed payload must have an even byte count.
    OddPayload,
}

/// A burst that failed framing validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedBurst {
    pub info: BurstInfo,
    pub reason: MalformedReason,
}

/// Event produced by the IEC 61937 framing parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Burst(Burst),
    Null(NullBurst),
    Unsupported(UnsupportedBurst),
    Malformed(MalformedBurst),
}

/// Cumulative framing counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CarrierStats {
    pub bursts: u64,
    pub null_bursts: u64,
    pub unsupported_bursts: u64,
    pub malformed_bursts: u64,
    pub resynced_bytes: u64,
    pub max_payload_bytes: usize,
}

/// Streaming IEC 61937 framing parser.
///
/// Input may be fragmented at any byte boundary. The parser keeps at most one
/// incomplete burst buffered and recognises the E-AC-3 and TrueHD burst periods
/// so padding cannot be mistaken for another burst.
#[derive(Debug)]
pub struct Iec61937Parser {
    buffer: Vec<u8>,
    offset: u64,
    generation: u64,
    stats: CarrierStats,
}

impl Default for Iec61937Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Iec61937Parser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            offset: 0,
            generation: 0,
            stats: CarrierStats::default(),
        }
    }

    /// Session generation. Incremented by [`Self::reset`].
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Absolute offset of the first byte not yet consumed.
    pub const fn carrier_offset(&self) -> u64 {
        self.offset
    }

    pub const fn stats(&self) -> CarrierStats {
        self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = CarrierStats::default();
    }

    /// Number of bytes buffered for an incomplete burst.
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Drop partial input and start a new session generation.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.offset = 0;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Feed carrier bytes and return every complete event.
    pub fn push(&mut self, data: &[u8]) -> Vec<Event> {
        self.buffer.extend_from_slice(data);
        let mut events = Vec::new();
        let mut consumed = 0usize;

        loop {
            let available = &self.buffer[consumed..];
            if available.len() < BURST_HEADER_BYTES {
                break;
            }

            match find_sync(available) {
                Some(0) => {}
                Some(skip) => {
                    self.stats.resynced_bytes += skip as u64;
                    consumed += skip;
                    continue;
                }
                None => {
                    // Keep a partial preamble without changing the carrier's
                    // 16-bit word alignment, even at an odd input boundary.
                    let drop = available.len().saturating_sub(6) & !1;
                    if drop == 0 {
                        break;
                    }
                    self.stats.resynced_bytes += drop as u64;
                    consumed += drop;
                    break;
                }
            }

            let pa = read_u16(&self.buffer[consumed..]);
            let pb = read_u16(&self.buffer[consumed + 2..]);
            debug_assert_eq!((pa, pb), (SYNC1, SYNC2));
            let pc = read_u16(&self.buffer[consumed + 4..]);
            let length_code = read_u16(&self.buffer[consumed + 6..]);

            let info = BurstInfo {
                carrier_offset: self.offset + consumed as u64,
                generation: self.generation,
                data_type: (pc & 0x007F) as u8,
                error: pc & 0x0080 != 0,
                data_type_dependent: ((pc >> 8) & 0x001F) as u8,
                bitstream_number: ((pc >> 13) & 0x0007) as u8,
                length_code,
            };

            if info.data_type == 0 {
                let skip = skip_unknown_burst(&self.buffer[consumed..]);
                if available.len() < skip {
                    break;
                }
                events.push(Event::Null(NullBurst { info }));
                self.stats.null_bursts += 1;
                consumed += skip;
                continue;
            }

            let Some(codec) = Codec::from_data_type(info.data_type) else {
                let skip = skip_unknown_burst(&self.buffer[consumed..]);
                if available.len() < skip {
                    break;
                }
                events.push(Event::Unsupported(UnsupportedBurst { info }));
                self.stats.unsupported_bursts += 1;
                consumed += skip;
                continue;
            };

            let capacity = codec.burst_bytes() - BURST_HEADER_BYTES;
            if length_code as usize > capacity {
                events.push(Event::Malformed(MalformedBurst {
                    info,
                    reason: MalformedReason::LengthExceedsBurst { capacity },
                }));
                self.stats.malformed_bursts += 1;
                consumed += BURST_HEADER_BYTES;
                continue;
            }
            if !length_code.is_multiple_of(2) {
                events.push(Event::Malformed(MalformedBurst {
                    info,
                    reason: MalformedReason::OddPayload,
                }));
                self.stats.malformed_bursts += 1;
                consumed += BURST_HEADER_BYTES;
                continue;
            }

            if available.len() < codec.burst_bytes() {
                break;
            }

            let payload_start = consumed + BURST_HEADER_BYTES;
            let payload_end = payload_start + length_code as usize;
            let payload = swap_words(&self.buffer[payload_start..payload_end]);
            self.stats.bursts += 1;
            self.stats.max_payload_bytes = self.stats.max_payload_bytes.max(payload.len());
            events.push(Event::Burst(Burst {
                info,
                codec,
                payload,
            }));
            consumed += codec.burst_bytes();
        }

        self.buffer.drain(..consumed);
        self.offset += consumed as u64;
        events
    }
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn find_sync(bytes: &[u8]) -> Option<usize> {
    let words = bytes.len() / 2;
    (0..words.saturating_sub(1)).find_map(|word| {
        let index = word * 2;
        (read_u16(&bytes[index..]) == SYNC1 && read_u16(&bytes[index + 2..]) == SYNC2)
            .then_some(index)
    })
}

fn skip_unknown_burst(bytes: &[u8]) -> usize {
    let length_code = read_u16(&bytes[6..]) as usize;
    let payload = length_code.min(MAX_PAYLOAD_BYTES);
    BURST_HEADER_BYTES + (payload & !1)
}

/// Swap the two bytes of each carrier word.
fn swap_words(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for pair in bytes.as_chunks::<2>().0 {
        out.push(pair[1]);
        out.push(pair[0]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn burst(data_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SYNC1.to_le_bytes());
        out.extend_from_slice(&SYNC2.to_le_bytes());
        out.extend_from_slice(&(data_type as u16).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        for pair in payload.as_chunks::<2>().0 {
            out.push(pair[1]);
            out.push(pair[0]);
        }
        out
    }

    fn padded_burst(data_type: u8, payload: &[u8], period: usize) -> Vec<u8> {
        let mut out = burst(data_type, payload);
        out.resize(period, 0);
        out
    }

    #[test]
    fn reassembles_fragmented_eac3_bursts() {
        let payload = (0u8..64).collect::<Vec<_>>();
        let carrier = padded_burst(EAC3_DATA_TYPE, &payload, EAC3_BURST_BYTES);
        let mut parser = Iec61937Parser::new();
        let mut events = Vec::new();
        for chunk in carrier.chunks(7) {
            events.extend(parser.push(chunk));
        }
        assert_eq!(events.len(), 1);
        let Event::Burst(burst) = &events[0] else {
            panic!("expected burst");
        };
        assert_eq!(burst.codec, Codec::Eac3);
        assert_eq!(burst.payload, payload);
        assert_eq!(burst.info.carrier_offset, 0);
        assert_eq!(parser.pending_bytes(), 0);
    }

    #[test]
    fn resync_preserves_word_alignment_at_every_input_boundary() {
        let mut carrier = vec![0; 16];
        carrier.extend(padded_burst(
            EAC3_DATA_TYPE,
            &[1, 2, 3, 4],
            EAC3_BURST_BYTES,
        ));
        for split in 0..40 {
            let mut parser = Iec61937Parser::new();
            let mut events = parser.push(&carrier[..split]);
            events.extend(parser.push(&carrier[split..]));
            assert_eq!(events.len(), 1, "split at {split}");
            let Event::Burst(burst) = &events[0] else {
                panic!("expected burst at split {split}");
            };
            assert_eq!(burst.info.carrier_offset, 16);
            assert_eq!(burst.payload, [1, 2, 3, 4]);
        }
    }

    #[test]
    fn handles_multiple_bursts_and_partial_tail() {
        let first = padded_burst(EAC3_DATA_TYPE, &[1, 2, 3, 4], EAC3_BURST_BYTES);
        let second = padded_burst(EAC3_DATA_TYPE, &[5, 6, 7, 8], EAC3_BURST_BYTES);
        let mut carrier = first;
        carrier.extend_from_slice(&second);
        carrier.extend_from_slice(&[0xF8, 0x72]);

        let mut parser = Iec61937Parser::new();
        let events = parser.push(&carrier);
        assert_eq!(events.len(), 2);
        assert_eq!(parser.pending_bytes(), 2);
        let Event::Burst(second_burst) = &events[1] else {
            panic!("expected burst");
        };
        assert_eq!(second_burst.info.carrier_offset, EAC3_BURST_BYTES as u64);
        assert_eq!(second_burst.payload, [5, 6, 7, 8]);
    }

    #[test]
    fn recognises_null_and_unsupported_bursts() {
        let mut carrier = burst(0, &[]);
        carrier.resize(EAC3_BURST_BYTES, 0);
        carrier.extend_from_slice(&padded_burst(9, &[1, 2], EAC3_BURST_BYTES));
        let mut parser = Iec61937Parser::new();
        let events = parser.push(&carrier);
        assert!(matches!(events[0], Event::Null(_)));
        assert!(matches!(events[1], Event::Unsupported(_)));
    }

    #[test]
    fn rejects_bad_lengths_and_resyncs() {
        let mut carrier = burst(EAC3_DATA_TYPE, &[1, 2]);
        carrier[6..8].copy_from_slice(&(EAC3_BURST_BYTES as u16).to_le_bytes());
        carrier.extend_from_slice(&padded_burst(EAC3_DATA_TYPE, &[3, 4], EAC3_BURST_BYTES));
        let mut parser = Iec61937Parser::new();
        let events = parser.push(&carrier);
        assert!(matches!(events[0], Event::Malformed(_)));
        assert!(events.iter().any(|event| matches!(event, Event::Burst(_))));
    }

    #[test]
    fn reset_starts_a_new_generation_and_drops_partial_input() {
        let carrier = padded_burst(EAC3_DATA_TYPE, &[1, 2], EAC3_BURST_BYTES);
        let mut parser = Iec61937Parser::new();
        assert!(parser.push(&carrier[..10]).is_empty());
        parser.reset();
        assert_eq!(parser.generation(), 1);
        assert_eq!(parser.pending_bytes(), 0);
        let events = parser.push(&carrier);
        let Event::Burst(burst) = &events[0] else {
            panic!("expected burst");
        };
        assert_eq!(burst.info.generation, 1);
        assert_eq!(burst.info.carrier_offset, 0);
    }
}
