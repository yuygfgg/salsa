//! EMDF container parsing.

use crate::bit::BitReader;
use crate::error::{Eac3JocError, Result};

pub const EMDF_SYNCWORD: u16 = 0x5838;
pub const ID_OAMD: u8 = objectaudio_oamd::OAMD_PAYLOAD_ID;
pub const ID_JOC: u8 = 14;
pub const MAX_PAYLOADS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmdfPayload {
    pub id: u8,
    /// `sample_offset` in samples (the 12-bit field shifted right by one).
    pub sample_offset: u16,
    /// MSB-first bit position of the payload bytes.
    pub bit_offset: usize,
    pub size: usize,
    pub group_id: u8,
    pub has_group_id: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmdfContainer {
    pub start_bit: usize,
    /// Declared container size in bytes, syncword included.
    pub raw_size: usize,
    pub payloads: Vec<EmdfPayload>,
}

impl EmdfContainer {
    pub fn find(&self, id: u8) -> Option<&EmdfPayload> {
        self.payloads.iter().find(|payload| payload.id == id)
    }
}

fn syntax(message: impl Into<String>) -> Eac3JocError {
    Eac3JocError::EmdfSyntax(message.into())
}

/// Parse the EMDF container that starts at `start_bit`.
pub fn parse_at(data: &[u8], start_bit: usize) -> Result<EmdfContainer> {
    let mut reader = BitReader::at(data, start_bit);
    if reader.read(16) != u32::from(EMDF_SYNCWORD) {
        return Err(syntax(format!("EMDF syncword mismatch at bit {start_bit}")));
    }
    let length = reader.read(16);
    let body_start = reader.position();
    let body_end = body_start + length as usize * 8;
    if body_end > reader.limit() {
        return Err(syntax(format!(
            "EMDF container length {length} exceeds buffer at bit {start_bit}"
        )));
    }
    reader.set_limit_bits(body_end);

    let mut version = reader.read(2);
    if version == 3 {
        version += reader.variable_bits(2, 8).map_err(|error| match error {
            Eac3JocError::BitstreamTruncated => Eac3JocError::BitstreamTruncated,
            other => other,
        })?;
    }
    let mut key_id = reader.read(3);
    if key_id == 7 {
        key_id += reader.variable_bits(3, 8)?;
    }
    if reader.is_failed() {
        return Err(Eac3JocError::BitstreamTruncated);
    }
    if version != 0 || key_id != 0 {
        return Err(syntax(format!(
            "unsupported EMDF version/key_id {version}/{key_id}"
        )));
    }

    let mut container = EmdfContainer {
        start_bit,
        raw_size: 0,
        payloads: Vec::new(),
    };
    let mut terminated = false;
    while reader.position() + 5 <= body_end {
        let mut payload_id = reader.read(5);
        if reader.is_failed() {
            return Err(Eac3JocError::BitstreamTruncated);
        }
        if payload_id == 0 {
            terminated = true;
            break;
        }
        if payload_id == 0x1f {
            payload_id += reader.variable_bits(5, 8)?;
        }
        if container
            .payloads
            .iter()
            .any(|payload| u32::from(payload.id) == payload_id)
        {
            return Err(syntax(format!("duplicate EMDF payload id {payload_id}")));
        }
        if container.payloads.len() >= MAX_PAYLOADS {
            return Err(syntax(format!("EMDF payload count exceeds {MAX_PAYLOADS}")));
        }

        let has_sample_offset = reader.read(1) != 0;
        let mut sample_offset = 0u16;
        if has_sample_offset {
            sample_offset = (reader.read(12) >> 1) as u16;
        }
        if reader.read(1) != 0 {
            let _ = reader.variable_bits(11, 8)?;
        }
        let mut group_id = 0u8;
        let mut has_group_id = false;
        if reader.read(1) != 0 {
            group_id = reader.variable_bits(2, 8)? as u8;
            has_group_id = true;
        }
        if reader.read(1) != 0 && !reader.skip(8) {
            return Err(Eac3JocError::BitstreamTruncated);
        }
        if reader.read(1) == 0 {
            let mut frame_aligned = false;
            if !has_sample_offset {
                frame_aligned = reader.read(1) != 0;
                if frame_aligned && !reader.skip(2) {
                    return Err(Eac3JocError::BitstreamTruncated);
                }
            }
            if (has_sample_offset || frame_aligned) && !reader.skip(7) {
                return Err(Eac3JocError::BitstreamTruncated);
            }
        }
        if reader.is_failed() {
            return Err(Eac3JocError::BitstreamTruncated);
        }

        let payload_size = reader.variable_bits(8, 8)?;
        let payload_bits = payload_size as usize * 8;
        if reader.position() + payload_bits > body_end {
            return Err(syntax(format!(
                "EMDF payload id {payload_id} extends past container body (size {payload_size} at bit {})",
                reader.position()
            )));
        }
        let bit_offset = reader.position();
        if !reader.skip(payload_bits) {
            return Err(Eac3JocError::BitstreamTruncated);
        }
        container.payloads.push(EmdfPayload {
            id: payload_id as u8,
            sample_offset,
            bit_offset,
            size: payload_size as usize,
            group_id,
            has_group_id,
        });
    }

    if !terminated {
        return Err(syntax("EMDF container has no payload id 0 terminator"));
    }
    container.raw_size = 4 + length as usize;
    Ok(container)
}

/// All candidate `0x5838` bit offsets over the eight alignments, ascending.
pub fn marker_offsets(data: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    if data.len() < 4 {
        return offsets;
    }
    for shift in 0..8usize {
        let aligned_len = if shift == 0 {
            data.len()
        } else {
            data.len() - 1
        };
        if aligned_len < 2 {
            continue;
        }
        let aligned_byte = |index: usize| -> u8 {
            if shift == 0 {
                data[index]
            } else {
                let high = u16::from(data[index]) << shift;
                let low = u16::from(data[index + 1]) >> (8 - shift);
                (high | low) as u8
            }
        };
        for index in 0..aligned_len - 1 {
            if aligned_byte(index) == 0x58 && aligned_byte(index + 1) == 0x38 {
                offsets.push(index * 8 + shift);
            }
        }
    }
    offsets.sort_unstable();
    offsets
}

/// Score a container as the frame's JOC metadata. Higher is better.
pub type ContainerScore<'a> = &'a dyn Fn(&EmdfContainer) -> i32;

/// Find the single top-level container that carries ID11+ID14.
pub fn find_joc_emdf(data: &[u8], score: Option<ContainerScore<'_>>) -> Result<EmdfContainer> {
    let offsets = marker_offsets(data);
    let marker_count = offsets.len();
    let mut matches = Vec::new();
    let mut parse_errors = 0usize;
    let mut first_error = String::new();
    for start_bit in offsets {
        match parse_at(data, start_bit) {
            Ok(candidate) => {
                if candidate.find(ID_OAMD).is_some() && candidate.find(ID_JOC).is_some() {
                    matches.push(candidate);
                }
            }
            Err(error) => {
                parse_errors += 1;
                if first_error.is_empty() {
                    first_error = format!("@bit{start_bit}: {error}");
                }
            }
        }
    }
    if matches.is_empty() {
        let mut message = format!(
            "no contiguous EMDF container carrying ID11+ID14 in this syncframe (markers={}, parse_failures={parse_errors})",
            marker_count
        );
        if !first_error.is_empty() {
            message.push_str(&format!("; first candidate error {first_error}"));
        }
        return Err(Eac3JocError::EmdfTransport(message));
    }

    matches.sort_by_key(|container| container.start_bit);
    let mut top_level: Vec<EmdfContainer> = Vec::new();
    for candidate in matches {
        let nested = top_level.iter().any(|parent| {
            parent.start_bit < candidate.start_bit
                && candidate.start_bit < parent.start_bit + parent.raw_size * 8
        });
        if !nested {
            top_level.push(candidate);
        }
    }

    if top_level.len() == 1 {
        return Ok(top_level.remove(0));
    }

    if let Some(score) = score {
        let mut best = 0;
        let mut winner: Option<usize> = None;
        let mut winners = 0;
        for (index, candidate) in top_level.iter().enumerate() {
            let value = score(candidate);
            if value > best {
                best = value;
                winner = Some(index);
                winners = 1;
            } else if value == best && value > 0 {
                winners += 1;
            }
        }
        if best > 0 && winners == 1 {
            return Ok(top_level.remove(winner.expect("winner index")));
        }
    }
    Err(Eac3JocError::EmdfTransport(format!(
        "multiple top-level JOC EMDF containers ({}); automatic selection is not defined",
        top_level.len()
    )))
}

/// Like [`find_joc_emdf`], but a frame without any EMDF marker is a normal
/// "absent" result.
pub fn find_joc_emdf_optional(
    data: &[u8],
    score: Option<ContainerScore<'_>>,
) -> Result<Option<EmdfContainer>> {
    match find_joc_emdf(data, score) {
        Ok(container) => Ok(Some(container)),
        Err(Eac3JocError::EmdfTransport(message)) if !message.starts_with("multiple top-level") => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Extract a payload exactly as the MSB-first reader sees it.
pub fn extract_payload_bytes(data: &[u8], payload: &EmdfPayload) -> Result<Vec<u8>> {
    let mut reader = BitReader::at(data, payload.bit_offset);
    reader
        .read_bytes(payload.size)
        .ok_or(Eac3JocError::BitstreamTruncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_syncword_only() {
        assert!(parse_at(&[0x58, 0x38, 0, 0], 0).is_err());
    }

    #[test]
    fn marker_offsets_find_shifted_markers() {
        // 0x58 0x38 at bit 4.
        let data = [0x05, 0x83, 0x80, 0x00];
        assert_eq!(marker_offsets(&data), vec![4]);
    }

    #[test]
    fn variable_bit_error_is_reported() {
        let mut reader = crate::bit::BitReader::new(&[0u8; 1]);
        assert!(reader.variable_bits(8, 8).is_err());
    }
}
