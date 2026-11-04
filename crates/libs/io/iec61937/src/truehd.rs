//! Dolby TrueHD-in-MAT extraction and the relative carrier timeline.
//!
//! The MAT structure is the one produced by FFmpeg's `spdifenc.c` for mpv:
//! a 20-byte start code, a 12-byte middle code at byte 30708 and a 16-byte end
//! code at byte 61408 inside a 61424-byte frame. Access units can cross those
//! codes and can cross MAT-frame boundaries. The input timing field in each
//! access unit and the physical carrier spacing are both preserved here.

use crate::carrier::CARRIER_SAMPLE_RATE;

/// Size of the MAT payload after the carrier burst header.
pub const MAT_FRAME_BYTES: usize = 61_424;
/// MAT start code at payload offset 0.
pub const MAT_START_CODE: [u8; 20] = [
    0x07, 0x9E, 0x00, 0x03, 0x84, 0x01, 0x01, 0x01, 0x80, 0x00, 0x56, 0xA5, 0x3B, 0xF4, 0x81, 0x83,
    0x49, 0x80, 0x77, 0xE0,
];
/// MAT middle code at payload offset 30708.
pub const MAT_MIDDLE_CODE: [u8; 12] = [
    0xC3, 0xC1, 0x42, 0x49, 0x3B, 0xFA, 0x82, 0x83, 0x49, 0x80, 0x77, 0xE0,
];
/// MAT end code at payload offset 61408.
pub const MAT_END_CODE: [u8; 16] = [
    0xC3, 0xC2, 0xC0, 0xC4, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x97, 0x11,
];

const MAT_START_OFFSET: usize = 0;
const MAT_MIDDLE_OFFSET: usize = 30_708;
const MAT_END_OFFSET: usize = MAT_FRAME_BYTES - MAT_END_CODE.len();
const TRUEHD_MIN_AU_BYTES: usize = 6;
const TRUEHD_MAX_AU_BYTES: usize = 0x1000 * 2;
/// Largest MAT code byte count that can be charged to one access unit.
const MAX_TIMING_SKEW_BYTES: i64 = 512;
/// Bound the reassembly buffer to a few MAT frames.
const MAX_PENDING_BYTES: usize = MAT_FRAME_BYTES * 3;

/// One TrueHD access unit recovered from the MAT transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrueHdAccessUnit {
    /// Session generation that produced the access unit.
    pub generation: u64,
    /// Absolute carrier byte offset of the access unit start.
    pub carrier_offset: u64,
    /// Raw 16-bit input timing field from the access-unit header.
    pub input_timing: u16,
    /// Unwrapped access-unit timeline, in audio samples, relative to the first
    /// access unit of this depacketizer session.
    pub sample_offset: u64,
    /// Complete TrueHD access unit, including the length and timing header.
    pub bytes: Vec<u8>,
}

/// Cumulative TrueHD transport counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrueHdStats {
    pub access_units: u64,
    pub mat_frames: u64,
    pub malformed_mat_frames: u64,
    pub padding_bytes: u64,
    pub resets: u64,
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    start: usize,
    len: usize,
    carrier_offset: u64,
}

/// Streaming TrueHD MAT depacketizer.
///
/// `sample_rate` is the TrueHD access-unit sample rate declared by the decoder
/// caps (48 kHz in the initial scope). It is only used to convert the input
/// timing delta into carrier byte spacing; payload bytes are never resampled.
#[derive(Debug)]
pub struct TrueHdDepacketizer {
    bytes_per_sample: u64,
    data: Vec<u8>,
    chunks: Vec<Chunk>,
    stats: TrueHdStats,
    prev_timing: Option<u16>,
    prev_carrier_offset: Option<u64>,
    sample_offset: u64,
}

impl TrueHdDepacketizer {
    pub fn new(sample_rate: u32) -> Self {
        let bytes_per_sample = u64::from(CARRIER_SAMPLE_RATE)
            * u64::from(crate::carrier::TRUEHD_CARRIER_BYTES_PER_FRAME)
            / u64::from(sample_rate.max(1));
        Self {
            bytes_per_sample,
            data: Vec::new(),
            chunks: Vec::new(),
            stats: TrueHdStats::default(),
            prev_timing: None,
            prev_carrier_offset: None,
            sample_offset: 0,
        }
    }

    pub const fn stats(&self) -> TrueHdStats {
        self.stats
    }

    /// Bytes per audio sample on the 192 kHz, 8-channel carrier.
    pub const fn bytes_per_sample(&self) -> u64 {
        self.bytes_per_sample
    }

    /// Number of data bytes buffered for an incomplete access unit.
    pub fn pending_bytes(&self) -> usize {
        self.data.len()
    }

    /// Drop partial input and restart the relative timeline.
    pub fn reset(&mut self) {
        self.data.clear();
        self.chunks.clear();
        self.prev_timing = None;
        self.prev_carrier_offset = None;
        self.sample_offset = 0;
        self.stats.resets += 1;
    }

    /// Feed one MAT payload (word order already restored) and return complete
    /// access units.
    pub fn push_mat_frame(
        &mut self,
        payload: &[u8],
        payload_carrier_offset: u64,
        generation: u64,
    ) -> Vec<TrueHdAccessUnit> {
        if !self.validate_mat(payload) {
            self.stats.malformed_mat_frames += 1;
            self.reset();
            return Vec::new();
        }
        self.stats.mat_frames += 1;

        self.push_data(
            &payload[MAT_START_OFFSET + MAT_START_CODE.len()..MAT_MIDDLE_OFFSET],
            payload_carrier_offset + (MAT_START_OFFSET + MAT_START_CODE.len()) as u64,
        );
        self.push_data(
            &payload[MAT_MIDDLE_OFFSET + MAT_MIDDLE_CODE.len()..MAT_END_OFFSET],
            payload_carrier_offset + (MAT_MIDDLE_OFFSET + MAT_MIDDLE_CODE.len()) as u64,
        );

        if self.data.len() > MAX_PENDING_BYTES {
            self.stats.malformed_mat_frames += 1;
            self.reset();
            return Vec::new();
        }

        self.extract(generation)
    }

    fn validate_mat(&self, payload: &[u8]) -> bool {
        payload.len() == MAT_FRAME_BYTES
            && payload[MAT_START_OFFSET..MAT_START_OFFSET + MAT_START_CODE.len()] == MAT_START_CODE
            && payload[MAT_MIDDLE_OFFSET..MAT_MIDDLE_OFFSET + MAT_MIDDLE_CODE.len()]
                == MAT_MIDDLE_CODE
            && payload[MAT_END_OFFSET..MAT_END_OFFSET + MAT_END_CODE.len()] == MAT_END_CODE
    }

    fn push_data(&mut self, bytes: &[u8], carrier_offset: u64) {
        if bytes.is_empty() {
            return;
        }
        let start = self.data.len();
        if let Some(last) = self.chunks.last_mut()
            && last.start + last.len == start
            && last.carrier_offset + last.len as u64 == carrier_offset
        {
            last.len += bytes.len();
            self.data.extend_from_slice(bytes);
            return;
        }
        self.chunks.push(Chunk {
            start,
            len: bytes.len(),
            carrier_offset,
        });
        self.data.extend_from_slice(bytes);
    }

    fn extract(&mut self, generation: u64) -> Vec<TrueHdAccessUnit> {
        let mut units = Vec::new();

        loop {
            let unit = match self.next_unit() {
                NextUnit::Unit {
                    start,
                    len,
                    timing,
                    carrier_offset,
                } => (start, len, timing, carrier_offset),
                NextUnit::NeedMore => break,
                NextUnit::Malformed => {
                    self.stats.malformed_mat_frames += 1;
                    self.reset();
                    break;
                }
            };
            let (start, len, timing, carrier_offset) = unit;

            let sample_offset = match self.prev_timing {
                Some(previous) => {
                    let delta = timing.wrapping_sub(previous);
                    self.sample_offset = self.sample_offset.wrapping_add(u64::from(delta));
                    self.sample_offset
                }
                None => {
                    self.sample_offset = 0;
                    0
                }
            };

            units.push(TrueHdAccessUnit {
                generation,
                carrier_offset,
                input_timing: timing,
                sample_offset,
                bytes: self.data[start..start + len].to_vec(),
            });
            self.stats.access_units += 1;
            self.prev_timing = Some(timing);
            self.prev_carrier_offset = Some(carrier_offset);
            self.consume(start + len);
        }

        units
    }

    /// Locate the next access unit. Padding is a run of zero bytes; the MAT
    /// code bytes that the carrier removed are accounted for by the timing
    /// equation rather than by hard-coded padding counts.
    fn next_unit(&mut self) -> NextUnit {
        let Some(first_nonzero) = self.data.iter().position(|&byte| byte != 0) else {
            return NextUnit::NeedMore;
        };
        if self.data.len() < 4 {
            return NextUnit::NeedMore;
        }

        let mut candidates = Vec::with_capacity(2);
        if first_nonzero > 0 {
            candidates.push(first_nonzero - 1);
        }
        candidates.push(first_nonzero);

        let mut best: Option<(i64, usize, usize, u16, u64)> = None;
        let mut incomplete = false;
        for start in candidates {
            let Some((len, timing)) = self.parse_length(start) else {
                continue;
            };
            let Some(carrier_offset) = self.carrier_offset_at(start) else {
                continue;
            };

            if let (Some(previous_timing), Some(previous_offset)) =
                (self.prev_timing, self.prev_carrier_offset)
            {
                let delta = timing.wrapping_sub(previous_timing);
                let expected = i64::from(delta) * self.bytes_per_sample as i64;
                let actual = carrier_offset as i64 - previous_offset as i64;
                let skew = expected - actual;
                if !(-MAX_TIMING_SKEW_BYTES..=MAX_TIMING_SKEW_BYTES).contains(&skew) {
                    continue;
                }
                if start + len > self.data.len() {
                    incomplete = true;
                    continue;
                }
                let score = skew.abs();
                if best.is_none_or(|(best_score, ..)| score < best_score) {
                    best = Some((score, start, len, timing, carrier_offset));
                }
            } else if start + len > self.data.len() {
                incomplete = true;
            } else {
                best = Some((0, start, len, timing, carrier_offset));
                break;
            }
        }

        if let Some((_, start, len, timing, carrier_offset)) = best {
            if start > 0 {
                self.stats.padding_bytes += start as u64;
            }
            return NextUnit::Unit {
                start,
                len,
                timing,
                carrier_offset,
            };
        }

        if incomplete || self.data.len() < TRUEHD_MIN_AU_BYTES {
            NextUnit::NeedMore
        } else {
            NextUnit::Malformed
        }
    }

    fn parse_length(&self, start: usize) -> Option<(usize, u16)> {
        let header = self.data.get(start..start + 4)?;
        let length_word = u16::from_be_bytes([header[0], header[1]]);
        let len = usize::from(length_word & 0x0FFF) * 2;
        if !(TRUEHD_MIN_AU_BYTES..=TRUEHD_MAX_AU_BYTES).contains(&len) {
            return None;
        }
        let timing = u16::from_be_bytes([header[2], header[3]]);
        Some((len, timing))
    }

    fn carrier_offset_at(&self, index: usize) -> Option<u64> {
        self.chunks.iter().find_map(|chunk| {
            let end = chunk.start + chunk.len;
            (chunk.start <= index && index < end)
                .then_some(chunk.carrier_offset + (index - chunk.start) as u64)
        })
    }

    fn consume(&mut self, count: usize) {
        if count == 0 || count > self.data.len() {
            return;
        }
        self.data.drain(..count);

        let mut chunks = Vec::with_capacity(self.chunks.len());
        for chunk in self.chunks.drain(..) {
            let end = chunk.start + chunk.len;
            if end <= count {
                continue;
            }
            let start = chunk.start.saturating_sub(count);
            let carrier_offset = if chunk.start >= count {
                chunk.carrier_offset
            } else {
                chunk.carrier_offset + (count - chunk.start) as u64
            };
            chunks.push(Chunk {
                start,
                len: end - count - start,
                carrier_offset,
            });
        }
        self.chunks = chunks;

        if self.data.is_empty() {
            self.chunks.clear();
        }
    }
}

enum NextUnit {
    Unit {
        start: usize,
        len: usize,
        timing: u16,
        carrier_offset: u64,
    },
    NeedMore,
    Malformed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access_unit(length: usize, timing: u16) -> Vec<u8> {
        assert!(length >= TRUEHD_MIN_AU_BYTES && length.is_multiple_of(2));
        let mut out = vec![0u8; length];
        let words = length / 2;
        out[0] = ((words >> 8) & 0x0F) as u8;
        out[1] = (words & 0xFF) as u8;
        out[2..4].copy_from_slice(&timing.to_be_bytes());
        out[4..].fill((timing & 0xFF) as u8);
        out
    }

    /// Build a realistic data stream: each access unit starts on a 2560-byte
    /// slot so the timing equation matches the 64 carrier bytes per sample.
    fn timed_stream(count: usize, length: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut data = Vec::new();
        let mut units = Vec::new();
        for index in 0..count {
            let timing = (index as u16).wrapping_mul(40).wrapping_add(0xFFF0);
            let unit = access_unit(length + index % 3 * 2, timing);
            units.push(unit.clone());
            data.extend_from_slice(&unit);
            let next = (index + 1) * 2560;
            data.resize(next, 0);
        }
        (data, units)
    }

    /// A stream whose access unit 11 is long enough to cross the MAT middle
    /// code at data offset 30688.
    fn stream_with_long_unit() -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut data = Vec::new();
        let mut units = Vec::new();
        let mut timing = 0xFFF0u16;
        for index in 0..11 {
            let unit = access_unit(300, timing);
            units.push(unit.clone());
            data.extend_from_slice(&unit);
            data.resize((index + 1) * 2560, 0);
            timing = timing.wrapping_add(40);
        }
        let unit = access_unit(3000, timing);
        units.push(unit.clone());
        data.extend_from_slice(&unit);
        data.resize(31_232, 0);
        timing = timing.wrapping_add(48);
        for _ in 0..4 {
            let unit = access_unit(300, timing);
            units.push(unit.clone());
            data.extend_from_slice(&unit);
            let next = data.len().div_ceil(2560) * 2560;
            data.resize(next, 0);
            timing = timing.wrapping_add(40);
        }
        (data, units)
    }

    fn mat_frame(data: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; MAT_FRAME_BYTES];
        frame[..MAT_START_CODE.len()].copy_from_slice(&MAT_START_CODE);
        frame[MAT_MIDDLE_OFFSET..MAT_MIDDLE_OFFSET + MAT_MIDDLE_CODE.len()]
            .copy_from_slice(&MAT_MIDDLE_CODE);
        frame[MAT_END_OFFSET..].copy_from_slice(&MAT_END_CODE);

        let first_capacity = MAT_MIDDLE_OFFSET - MAT_START_CODE.len();
        let first = data.len().min(first_capacity);
        frame[MAT_START_CODE.len()..MAT_START_CODE.len() + first].copy_from_slice(&data[..first]);
        let second_start = MAT_MIDDLE_OFFSET + MAT_MIDDLE_CODE.len();
        frame[second_start..second_start + data.len() - first].copy_from_slice(&data[first..]);
        frame
    }

    #[test]
    fn recovers_access_units_across_codes() {
        let (data, units) = timed_stream(23, 288);
        let frame = mat_frame(&data);

        let mut depacketizer = TrueHdDepacketizer::new(48_000);
        let recovered = depacketizer.push_mat_frame(&frame, 8, 3);

        assert_eq!(recovered.len(), 23);
        for (actual, expected) in recovered.iter().zip(&units) {
            assert_eq!(&actual.bytes, expected);
        }
        assert_eq!(recovered[0].sample_offset, 0);
        assert_eq!(recovered[1].sample_offset, 40);
        assert_eq!(recovered[0].generation, 3);
    }

    #[test]
    fn reassembles_an_access_unit_split_by_the_mat_middle_code() {
        let (data, units) = stream_with_long_unit();
        assert!(data.len() > MAT_MIDDLE_OFFSET - MAT_START_CODE.len());
        let frame = mat_frame(&data);

        let mut depacketizer = TrueHdDepacketizer::new(48_000);
        let recovered = depacketizer.push_mat_frame(&frame, 8, 0);

        assert_eq!(recovered.len(), units.len());
        for (actual, expected) in recovered.iter().zip(&units) {
            assert_eq!(&actual.bytes, expected);
        }
    }

    #[test]
    fn skips_padding_and_reports_malformed_frames() {
        let first = access_unit(300, 0xFFF0);
        let second = access_unit(400, 0x0018);
        let mut data = first.clone();
        data.resize(2560, 0);
        data.extend_from_slice(&second);

        let frame = mat_frame(&data);
        let mut depacketizer = TrueHdDepacketizer::new(48_000);
        let units = depacketizer.push_mat_frame(&frame, 8, 0);
        assert_eq!(units.len(), 2);
        assert!(depacketizer.stats().padding_bytes > 0);

        let mut broken = frame;
        broken[0] = 0xFF;
        assert!(depacketizer.push_mat_frame(&broken, 0, 0).is_empty());
        assert_eq!(depacketizer.stats().malformed_mat_frames, 1);
    }
}
