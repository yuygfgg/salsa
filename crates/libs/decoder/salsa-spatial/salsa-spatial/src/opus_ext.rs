//! Experimental Opus padding-extension carriage for enhancement bytes.
//!
//! This implements the experimental extension mechanism from
//! `draft-ietf-mlcodec-opus-extension-06`.  The salsa payload is carried in an
//! experimental long extension (ID 120, which is one of the IDs reserved for
//! experiments).  The extension payload starts with a four-byte magic, a
//! format version, a flags byte, the original packet reconstruction data, and
//! a self-delimiting length for the opaque salsa bytes.
//!
//! Opus padding only exists in code 3 packets, so a code 0/1/2 packet is
//! repacketized into a code 3 packet with identical frames.  Enough information
//! about the original framing is stored in the extension to reconstruct the
//! original packet byte-for-byte, including any optional padding header.
//!
//! A non-extended Opus decoder ignores the non-zero padding as required by
//! RFC 6716 and decodes the same frames.

use crate::error::{Result, SpatialError};
use crate::{ENHANCEMENT_FORMAT_VERSION, OPUS_EXTENSION_EXPERIMENT_ID};

/// One of the extension IDs reserved for experimental use by the draft.
const EXTENSION_ID: u8 = 120;
const MAGIC: u32 = OPUS_EXTENSION_EXPERIMENT_ID;
const VERSION: u16 = ENHANCEMENT_FORMAT_VERSION;

const FLAG_ORIGINAL_CODE_MASK: u8 = 0x03;
const FLAG_ORIGINAL_VBR: u8 = 0x04;
const FLAG_ORIGINAL_PADDING_HEADER: u8 = 0x08;

/// Append a salsa enhancement extension to an Opus packet.
///
/// The returned packet decodes to exactly the same frames as the input when
/// passed to an ordinary Opus decoder.  [`strip_extension`] recovers both the
/// original packet bytes and the payload.
pub fn append_extension(packet: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    let original = parse_opus_packet(packet)?;
    if original.frames.len() > 63 {
        return Err(SpatialError::Syntax("too many Opus frames in packet"));
    }
    for frame in &original.frames {
        if frame.len() > 1275 {
            return Err(SpatialError::Syntax("Opus frame exceeds 1275 bytes"));
        }
    }

    let mut wrapper = Vec::new();
    wrapper.extend_from_slice(&MAGIC.to_be_bytes());
    wrapper.extend_from_slice(&VERSION.to_be_bytes());
    let mut flags = original.code & FLAG_ORIGINAL_CODE_MASK;
    if original.vbr {
        flags |= FLAG_ORIGINAL_VBR;
    }
    if !original.padding_header.is_empty() {
        flags |= FLAG_ORIGINAL_PADDING_HEADER;
    }
    wrapper.push(flags);
    let padding_header_len = u16::try_from(original.padding_header.len())
        .map_err(|_| SpatialError::Syntax("Opus padding header is too large"))?;
    wrapper.extend_from_slice(&padding_header_len.to_be_bytes());
    let padding_len = u32::try_from(original.padding.len())
        .map_err(|_| SpatialError::Syntax("Opus padding is too large"))?;
    wrapper.extend_from_slice(&padding_len.to_be_bytes());
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| SpatialError::Syntax("salsa enhancement payload is too large"))?;
    wrapper.extend_from_slice(&payload_len.to_be_bytes());
    wrapper.extend_from_slice(&original.padding_header);
    wrapper.extend_from_slice(&original.padding);
    wrapper.extend_from_slice(payload);

    // Extension instance: experimental ID 120, L=1, explicit length.
    let mut extension_block = Vec::new();
    extension_block.push((EXTENSION_ID << 1) | 1);
    write_extension_length(&mut extension_block, wrapper.len());
    extension_block.extend_from_slice(&wrapper);

    let frame_count = original.frames.len();
    let toc = (original.toc & 0xfc) | 3;
    let mut out = Vec::with_capacity(
        2 + extension_block.len()
            + original
                .frames
                .iter()
                .map(|frame| frame.len() + 2)
                .sum::<usize>(),
    );
    out.push(toc);
    // VBR=1, padding=1, M frames.
    out.push(0x80 | 0x40 | (frame_count as u8)); // v=1, p=1, M=frame_count
    write_padding_header(&mut out, extension_block.len());
    for frame in original.frames.iter().take(frame_count.saturating_sub(1)) {
        write_frame_length(&mut out, frame.len())?;
    }
    for frame in &original.frames {
        out.extend_from_slice(frame);
    }
    out.extend_from_slice(&extension_block);
    Ok(out)
}

/// Remove a salsa enhancement extension from an Opus packet, returning the
/// ordinary packet and the extracted payload (if present).
///
/// Packets without a salsa extension are returned unchanged.  A malformed
/// extension is reported as an error.
pub fn strip_extension(packet: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let parsed = parse_opus_packet(packet)?;
    let extension = match find_extension(&parsed.padding) {
        Some(extension) => extension,
        None => return Ok((packet.to_vec(), None)),
    };

    let mut reader = ByteReader::new(extension);
    let magic = reader.u32()?;
    if magic != MAGIC {
        return Err(SpatialError::Syntax("bad Opus extension magic"));
    }
    let version = reader.u16()?;
    if version != VERSION {
        return Err(SpatialError::UnsupportedVersion { version });
    }
    let flags = reader.u8()?;
    let padding_header_len = reader.u16()? as usize;
    let padding_len = reader.u32()? as usize;
    let payload_len = reader.u32()? as usize;
    let padding_header = reader.bytes(padding_header_len)?.to_vec();
    let padding = reader.bytes(padding_len)?.to_vec();
    let payload = reader.bytes(payload_len)?.to_vec();

    let original_code = flags & FLAG_ORIGINAL_CODE_MASK;
    let original_vbr = flags & FLAG_ORIGINAL_VBR != 0;
    let had_padding_header = flags & FLAG_ORIGINAL_PADDING_HEADER != 0;

    let mut out = Vec::new();
    out.push((parsed.toc & 0xfc) | original_code);
    match original_code {
        0 => {
            if parsed.frames.len() != 1 {
                return Err(SpatialError::Syntax("code 0 packet frame count mismatch"));
            }
            out.extend_from_slice(&parsed.frames[0]);
        }
        1 => {
            if parsed.frames.len() != 2 || parsed.frames[0].len() != parsed.frames[1].len() {
                return Err(SpatialError::Syntax("code 1 packet frame layout mismatch"));
            }
            out.extend_from_slice(&parsed.frames[0]);
            out.extend_from_slice(&parsed.frames[1]);
        }
        2 => {
            if parsed.frames.len() != 2 {
                return Err(SpatialError::Syntax("code 2 packet frame count mismatch"));
            }
            write_frame_length(&mut out, parsed.frames[0].len())?;
            out.extend_from_slice(&parsed.frames[0]);
            out.extend_from_slice(&parsed.frames[1]);
        }
        3 => {
            let frame_count = u8::try_from(parsed.frames.len())
                .map_err(|_| SpatialError::Syntax("too many Opus frames"))?;
            let mut count_byte = frame_count;
            if had_padding_header {
                count_byte |= 0x40;
            }
            if original_vbr {
                count_byte |= 0x80;
            }
            out.push(count_byte);
            if had_padding_header {
                out.extend_from_slice(&padding_header);
            }
            if original_vbr {
                for frame in parsed
                    .frames
                    .iter()
                    .take(parsed.frames.len().saturating_sub(1))
                {
                    write_frame_length(&mut out, frame.len())?;
                }
            } else if parsed
                .frames
                .windows(2)
                .any(|pair| pair[0].len() != pair[1].len())
            {
                return Err(SpatialError::Syntax("CBR packet has unequal frame sizes"));
            }
            for frame in &parsed.frames {
                out.extend_from_slice(frame);
            }
            if had_padding_header {
                out.extend_from_slice(&padding);
            }
        }
        _ => return Err(SpatialError::Syntax("invalid original packet code")),
    }

    Ok((out, Some(payload)))
}

#[derive(Debug)]
struct ParsedOpusPacket {
    toc: u8,
    code: u8,
    vbr: bool,
    frames: Vec<Vec<u8>>,
    padding_header: Vec<u8>,
    padding: Vec<u8>,
}

fn parse_opus_packet(packet: &[u8]) -> Result<ParsedOpusPacket> {
    let toc = *packet.first().ok_or(SpatialError::Truncated)?;
    let code = toc & 0x03;
    if code < 3 {
        let payload = &packet[1..];
        let frames = match code {
            0 => vec![payload.to_vec()],
            1 => {
                if !payload.len().is_multiple_of(2) {
                    return Err(SpatialError::Syntax("code 1 packet has odd payload length"));
                }
                let half = payload.len() / 2;
                vec![payload[..half].to_vec(), payload[half..].to_vec()]
            }
            2 => {
                let mut position = 0usize;
                let first_len = read_frame_length(payload, &mut position)?;
                if first_len > payload.len().saturating_sub(position) {
                    return Err(SpatialError::Syntax("code 2 first frame overruns packet"));
                }
                vec![
                    payload[position..position + first_len].to_vec(),
                    payload[position + first_len..].to_vec(),
                ]
            }
            _ => unreachable!(),
        };
        return Ok(ParsedOpusPacket {
            toc,
            code,
            vbr: false,
            frames,
            padding_header: Vec::new(),
            padding: Vec::new(),
        });
    }

    if packet.len() < 2 {
        return Err(SpatialError::Truncated);
    }
    let count_byte = packet[1];
    let frame_count = (count_byte & 0x3f) as usize;
    let has_padding = count_byte & 0x40 != 0;
    let vbr = count_byte & 0x80 != 0;
    if frame_count == 0 {
        return Err(SpatialError::Syntax("code 3 packet has no frames"));
    }

    let mut position = 2usize;
    let mut padding_header = Vec::new();
    let mut padding_len = 0usize;
    if has_padding {
        loop {
            let byte = *packet.get(position).ok_or(SpatialError::Truncated)?;
            position += 1;
            padding_header.push(byte);
            if byte == 255 {
                padding_len = padding_len.saturating_add(254);
            } else {
                padding_len = padding_len.saturating_add(byte as usize);
                break;
            }
        }
    }
    if position.saturating_add(padding_len) > packet.len() {
        return Err(SpatialError::Syntax("Opus padding overruns packet"));
    }
    let data_end = packet.len() - padding_len;

    let mut lengths = Vec::with_capacity(frame_count);
    if vbr {
        let mut used = 0usize;
        for _ in 0..frame_count - 1 {
            let len = read_frame_length(&packet[..data_end], &mut position)?;
            used = used.saturating_add(len);
            lengths.push(len);
        }
        let remaining = data_end.saturating_sub(position);
        if used > remaining {
            return Err(SpatialError::Syntax("Opus frame lengths overrun packet"));
        }
        lengths.push(remaining - used);
    } else {
        let remaining = data_end.saturating_sub(position);
        if !remaining.is_multiple_of(frame_count) {
            return Err(SpatialError::Syntax("CBR code 3 payload is not divisible"));
        }
        lengths.resize(frame_count, remaining / frame_count);
    }

    let mut frames = Vec::with_capacity(frame_count);
    for len in lengths {
        if position.saturating_add(len) > data_end {
            return Err(SpatialError::Syntax("Opus frame overruns packet"));
        }
        frames.push(packet[position..position + len].to_vec());
        position += len;
    }
    if position != data_end {
        return Err(SpatialError::Syntax("Opus packet has trailing frame data"));
    }

    Ok(ParsedOpusPacket {
        toc,
        code,
        vbr,
        frames,
        padding_header,
        padding: packet[data_end..].to_vec(),
    })
}

fn read_frame_length(data: &[u8], position: &mut usize) -> Result<usize> {
    let first = *data.get(*position).ok_or(SpatialError::Truncated)?;
    *position += 1;
    if first < 252 {
        Ok(first as usize)
    } else {
        let second = *data.get(*position).ok_or(SpatialError::Truncated)?;
        *position += 1;
        Ok(second as usize * 4 + first as usize)
    }
}

fn write_frame_length(out: &mut Vec<u8>, len: usize) -> Result<()> {
    if len > 1275 {
        return Err(SpatialError::Syntax("Opus frame exceeds 1275 bytes"));
    }
    if len < 252 {
        out.push(len as u8);
    } else {
        let first = 252 + (len & 0x03);
        let second = (len - first) / 4;
        out.push(first as u8);
        out.push(second as u8);
    }
    Ok(())
}

/// RFC 6716 padding length coding: 255 means 254 plus the next value.
fn write_padding_header(out: &mut Vec<u8>, mut len: usize) {
    loop {
        if len < 255 {
            out.push(len as u8);
            return;
        }
        out.push(255);
        len -= 254;
    }
}

/// Opus extension length coding from the draft: 255 means 255 plus the next
/// value.
fn write_extension_length(out: &mut Vec<u8>, mut len: usize) {
    while len >= 255 {
        out.push(255);
        len -= 255;
    }
    out.push(len as u8);
}

fn read_extension_length(data: &[u8], position: &mut usize) -> Result<usize> {
    let mut len = 0usize;
    loop {
        let byte = *data.get(*position).ok_or(SpatialError::Truncated)?;
        *position += 1;
        if byte == 255 {
            len = len.saturating_add(255);
        } else {
            return Ok(len.saturating_add(byte as usize));
        }
    }
}

fn find_extension(padding: &[u8]) -> Option<&[u8]> {
    let mut position = 0usize;
    while position < padding.len() {
        let id_byte = padding[position];
        position += 1;
        let id = id_byte >> 1;
        let length_flag = id_byte & 1;
        if id == 0 {
            if length_flag == 0 {
                return None;
            }
            continue;
        }
        if id == 1 {
            if length_flag == 1 {
                position += 1;
            }
            continue;
        }
        if id == 2 {
            // This implementation never emits RTE.  The repeat mechanism
            // cannot introduce a salsa extension, so stop scanning safely.
            return None;
        }
        if id < 32 {
            if length_flag == 1 {
                position += 1;
            }
            continue;
        }

        let data_len = if length_flag == 0 {
            padding.len().saturating_sub(position)
        } else {
            match read_extension_length(padding, &mut position) {
                Ok(len) => len,
                Err(_) => return None,
            }
        };
        if position.saturating_add(data_len) > padding.len() {
            return None;
        }
        let data = &padding[position..position + data_len];
        position += data_len;
        if id == EXTENSION_ID {
            return Some(data);
        }
    }
    None
}

struct ByteReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ByteReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn u8(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.position)
            .ok_or(SpatialError::Truncated)?;
        self.position += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn bytes(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(SpatialError::Truncated)?;
        let slice = self
            .data
            .get(self.position..end)
            .ok_or(SpatialError::Truncated)?;
        self.position = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_packet() -> Vec<u8> {
        // Code 0, 20 ms CELT fullband mono (config 31), followed by fake bytes.
        let mut packet = vec![(31 << 3) | 0b000];
        packet.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        packet
    }

    #[test]
    fn append_and_strip_round_trip_code_zero() {
        let packet = synthetic_packet();
        let payload = b"salsa enhancement payload".to_vec();
        let framed = append_extension(&packet, &payload).unwrap();
        assert_ne!(framed, packet);
        let (stripped, extracted) = strip_extension(&framed).unwrap();
        assert_eq!(stripped, packet);
        assert_eq!(extracted, Some(payload));
    }

    #[test]
    fn packets_without_extension_are_unchanged() {
        let packet = synthetic_packet();
        let (stripped, extracted) = strip_extension(&packet).unwrap();
        assert_eq!(stripped, packet);
        assert_eq!(extracted, None);
    }

    #[test]
    fn append_and_strip_round_trip_code_three_existing_padding() {
        // Code 3, one VBR frame, existing RFC 6716 padding of 3 zero bytes.
        let mut packet = vec![(20 << 3) | 3, 0x40 | 1];
        packet.push(3); // padding length: 3 bytes
        packet.extend_from_slice(&[9, 8, 7]);
        packet.extend_from_slice(&[0, 0, 0]);
        let payload = vec![0xaa, 0xbb, 0xcc];
        let framed = append_extension(&packet, &payload).unwrap();
        let (stripped, extracted) = strip_extension(&framed).unwrap();
        assert_eq!(stripped, packet);
        assert_eq!(extracted, Some(payload));
    }

    #[test]
    fn large_lengths_round_trip() {
        let packet = synthetic_packet();
        let payload = vec![0x5a; 900];
        let framed = append_extension(&packet, &payload).unwrap();
        let (stripped, extracted) = strip_extension(&framed).unwrap();
        assert_eq!(stripped, packet);
        assert_eq!(extracted, Some(payload));
    }

    #[test]
    fn round_trip_code_one_and_code_two() {
        let payload = b"payload".to_vec();
        // Code 1: two equal-size frames.
        let mut code1 = vec![(20 << 3) | 0b01];
        code1.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let framed = append_extension(&code1, &payload).unwrap();
        assert_eq!(
            strip_extension(&framed).unwrap(),
            (code1, Some(payload.clone()))
        );

        // Code 2: two frames, first length signaled.
        let mut code2 = vec![(20 << 3) | 0b10, 3];
        code2.extend_from_slice(&[7, 8, 9, 10, 11]);
        let framed = append_extension(&code2, &payload).unwrap();
        assert_eq!(strip_extension(&framed).unwrap(), (code2, Some(payload)));
    }

    #[test]
    fn round_trip_code_three_vbr() {
        let payload = b"vbr payload".to_vec();
        // Code 3, VBR, two frames of lengths 3 and 2, no padding.
        let mut packet = vec![(16 << 3) | 0b11, 0x80 | 2, 3];
        packet.extend_from_slice(&[1, 2, 3, 4, 5]);
        let framed = append_extension(&packet, &payload).unwrap();
        assert_eq!(strip_extension(&framed).unwrap(), (packet, Some(payload)));
    }
}
