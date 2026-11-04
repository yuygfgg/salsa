//! E-AC-3 / AC-3 syncframe framing.
//!
//! This only splits the transport stream into syncframes; it never touches
//! EMDF, JOC or PCM.

use crate::error::{Eac3JocError, Result};

pub const SYNCWORD: u16 = 0x0B77;

const HEADER_BYTES_EAC3: usize = 4;
const HEADER_BYTES_AC3: usize = 6;

/// AC-3 syncframe length in 16-bit words, indexed by `fscod` then `frmsizecod`
/// (A/52 5.4.1.2).
const AC3_FRAME_WORDS: [[u16; 38]; 3] = [
    [
        64, 64, 80, 80, 96, 96, 112, 112, 128, 128, 160, 160, 192, 192, 224, 224, 256, 256, 320,
        320, 384, 384, 448, 448, 512, 512, 640, 640, 768, 768, 896, 896, 1024, 1024, 1152, 1152,
        1280, 1280,
    ],
    [
        69, 70, 87, 88, 104, 105, 121, 122, 139, 140, 174, 175, 208, 209, 243, 244, 278, 278, 348,
        348, 417, 418, 487, 487, 557, 557, 696, 696, 835, 835, 975, 975, 1114, 1114, 1253, 1253,
        1393, 1393,
    ],
    [
        96, 96, 120, 120, 144, 144, 168, 168, 192, 192, 240, 240, 288, 288, 336, 336, 384, 384,
        480, 480, 576, 576, 672, 672, 768, 768, 960, 960, 1152, 1152, 1344, 1344, 1536, 1536, 1728,
        1728, 1920, 1920,
    ],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Ac3,
    Eac3,
}

/// The L2 header of one syncframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncFrameHeader {
    pub codec: Codec,
    /// E-AC-3 `strmtyp`: 0 independent, 1 dependent, 2/3 reserved.
    pub stream_type: u8,
    pub substream_id: u8,
    pub acmod: u8,
    pub lfeon: bool,
    pub bsid: u8,
    pub sample_rate: u32,
    /// E-AC-3 `numblkscod` (0-3); always 3 (six blocks) in JOC streams.
    pub num_blocks_code: u8,
    pub chanmap: Option<u16>,
    /// Declared size of the whole syncframe in bytes.
    pub size: usize,
}

impl SyncFrameHeader {
    /// Substreams with the same key belong to the same audio frame.
    pub fn same_stream(&self, other: &Self) -> bool {
        self.codec == other.codec
            && self.stream_type == other.stream_type
            && self.substream_id == other.substream_id
    }

    /// Samples per syncframe; JOC uses six 256-sample blocks.
    pub fn samples(&self) -> usize {
        if self.codec == Codec::Ac3 || self.num_blocks_code == 3 {
            6 * 256
        } else {
            (usize::from(self.num_blocks_code) + 1) * 256
        }
    }
}

/// One syncframe with the payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFrame {
    pub header: SyncFrameHeader,
    /// Byte offset of the frame start in the fed stream.
    pub offset: usize,
    pub bytes: Vec<u8>,
}

fn read_bits(data: &[u8], start: usize, count: usize) -> u32 {
    salsa_bitstream::BitReader::at(data, start).read(count as u32)
}

/// Parse the syncframe header at the start of `data`.
pub fn parse_syncframe_header(data: &[u8]) -> Option<SyncFrameHeader> {
    if data.len() < HEADER_BYTES_AC3 {
        return None;
    }
    let syncword = (u16::from(data[0]) << 8) | u16::from(data[1]);
    if syncword != SYNCWORD {
        return None;
    }
    let bsid = read_bits(data, 40, 5) as u8;
    if bsid <= 8 {
        let fscod = read_bits(data, 32, 2);
        let frmsizecod = read_bits(data, 34, 6);
        if fscod > 2 || frmsizecod >= 38 {
            return None;
        }
        let size = usize::from(AC3_FRAME_WORDS[fscod as usize][frmsizecod as usize]) * 2;
        let acmod = read_bits(data, 48, 3) as u8;
        let mut bit = 51usize;
        if acmod & 1 != 0 && acmod != 1 {
            bit += 2; // cmixlev
        }
        if acmod & 4 != 0 {
            bit += 2; // surmixlev
        }
        if acmod == 2 {
            bit += 2; // dsurmod
        }
        let lfeon = read_bits(data, bit, 1) != 0;
        let sample_rate = match fscod {
            0 => 48_000,
            1 => 44_100,
            _ => 32_000,
        };
        return Some(SyncFrameHeader {
            codec: Codec::Ac3,
            stream_type: 0,
            substream_id: 0,
            acmod,
            lfeon,
            bsid,
            sample_rate,
            num_blocks_code: 3,
            chanmap: None,
            size,
        });
    }

    let stream_type = read_bits(data, 16, 2) as u8;
    let substream_id = read_bits(data, 18, 3) as u8;
    let frmsiz = read_bits(data, 21, 11);
    let size = (frmsiz as usize + 1) * 2;
    let fscod = read_bits(data, 32, 2);
    let (sample_rate, num_blocks_code) = if fscod == 3 {
        let fscod2 = read_bits(data, 34, 2);
        let rate = match fscod2 {
            0 => 24_000,
            1 => 22_050,
            2 => 16_000,
            _ => return None,
        };
        (rate, 3)
    } else {
        let rate = match fscod {
            0 => 48_000,
            1 => 44_100,
            _ => 32_000,
        };
        (rate, read_bits(data, 34, 2) as u8)
    };
    // fscod==3 also consumes two bits, so acmod always starts at bit 36.
    let acmod = read_bits(data, 36, 3) as u8;
    let lfeon = read_bits(data, 39, 1) != 0;
    let mut bit = 45 + 5; // bsid, then dialnorm
    let compre = read_bits(data, bit, 1);
    bit += 1;
    if compre != 0 {
        bit += 8;
    }
    if acmod == 0 {
        bit += 5; // dialnorm2
        let compr2 = read_bits(data, bit, 1);
        bit += 1;
        if compr2 != 0 {
            bit += 8;
        }
    }
    let mut chanmap = None;
    if stream_type == 1 && read_bits(data, bit, 1) != 0 {
        chanmap = Some(read_bits(data, bit + 1, 16) as u16);
    }
    Some(SyncFrameHeader {
        codec: Codec::Eac3,
        stream_type,
        substream_id,
        acmod,
        lfeon,
        bsid,
        sample_rate,
        num_blocks_code,
        chanmap,
        size,
    })
}

/// Incremental syncframe splitter.
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
    consumed: usize,
    base_offset: usize,
    finished: bool,
    frames_emitted: u64,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.consumed > 0 {
            self.buffer.drain(..self.consumed);
            self.base_offset += self.consumed;
            self.consumed = 0;
        }
        self.buffer.extend_from_slice(data);
    }

    pub fn finish(&mut self) {
        self.finished = true;
    }

    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted
    }

    /// Number of bytes already turned into frames.
    pub fn stream_offset(&self) -> usize {
        self.base_offset + self.consumed
    }

    /// Next complete syncframe, or `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<SyncFrame>> {
        let available = self.buffer.len() - self.consumed;
        if available == 0 {
            return Ok(None);
        }
        let start = self.consumed;
        if available < HEADER_BYTES_EAC3 {
            if self.finished {
                return Err(Eac3JocError::TruncatedHeader);
            }
            return Ok(None);
        }
        let header = match parse_syncframe_header(&self.buffer[start..]) {
            Some(header) if header.size > 0 => header,
            _ => {
                if available < HEADER_BYTES_AC3 && !self.finished {
                    return Ok(None);
                }
                return Err(Eac3JocError::InvalidSyncframe {
                    offset: self.base_offset + start,
                });
            }
        };
        if header.size > available {
            if !self.finished {
                return Ok(None);
            }
            return Err(Eac3JocError::TruncatedFrame {
                declared: header.size,
                remaining: available,
            });
        }
        let bytes = self.buffer[start..start + header.size].to_vec();
        let offset = self.base_offset + start;
        self.consumed += header.size;
        self.frames_emitted += 1;
        Ok(Some(SyncFrame {
            header,
            offset,
            bytes,
        }))
    }

    /// Declared size of the syncframe starting `offset` bytes into `data`.
    pub fn frame_bytes(data: &[u8], offset: usize) -> Result<usize> {
        let header = parse_syncframe_header(&data[offset..])
            .ok_or(Eac3JocError::InvalidSyncframe { offset })?;
        if offset + header.size > data.len() {
            return Err(Eac3JocError::TruncatedFrame {
                declared: header.size,
                remaining: data.len() - offset,
            });
        }
        Ok(header.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_syncword() {
        assert!(parse_syncframe_header(&[0, 0, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn reads_an_ac3_header() {
        // 0x0B77, crc, fscod=0/frmsizecod=0, bsid=8, bsmod/acmod=2, lfeon=0.
        let mut data = vec![0u8; 8];
        data[0] = 0x0B;
        data[1] = 0x77;
        // bsid bits 40..45 = 8
        data[5] = 8 << 3;
        let header = parse_syncframe_header(&data).unwrap();
        assert_eq!(header.codec, Codec::Ac3);
        assert_eq!(header.size, 128);
        assert_eq!(header.sample_rate, 48_000);
    }

    #[test]
    fn incremental_reader_needs_more_bytes() {
        let mut reader = FrameReader::new();
        reader.push(&[0x0B]);
        assert!(reader.next_frame().unwrap().is_none());
        reader.finish();
        assert!(reader.next_frame().is_err());
    }
}
