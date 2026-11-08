//! Ogg page scanner used only to find tile boundaries for `GstBaseParse`.
//!
//! The Salsa parser owns all payload interpretation. This scanner counts Ogg
//! pages so the element can hand complete five-packet (100 ms) tiles to the
//! codec parser. A page carries one Opus multistream packet in the reference
//! writer.

const OGG_HEADER: &[u8; 4] = b"OggS";
const OPUS_HEAD: &[u8; 8] = b"OpusHead";
const OPUS_TAGS: &[u8; 8] = b"OpusTags";
const TILE_PACKETS: usize = 5;

struct Page {
    next: usize,
    header: bool,
    has_payload: bool,
}

fn page_at(data: &[u8], offset: usize) -> Option<Page> {
    let end_header = offset.checked_add(27)?;
    if data.len() < end_header {
        return None;
    }
    if &data[offset..offset + 4] != OGG_HEADER {
        return None;
    }
    let segments = data[offset + 26] as usize;
    let lacing_end = end_header.checked_add(segments)?;
    if data.len() < lacing_end {
        return None;
    }
    let payload_len: usize = data[end_header..lacing_end]
        .iter()
        .map(|value| *value as usize)
        .sum();
    let payload_end = lacing_end.checked_add(payload_len)?;
    if data.len() < payload_end {
        return None;
    }
    let header = data[lacing_end..payload_end].starts_with(OPUS_HEAD)
        || data[lacing_end..payload_end].starts_with(OPUS_TAGS);
    Some(Page {
        next: payload_end,
        header,
        has_payload: payload_len > 0,
    })
}

/// Whether the prefix contains at least one non-header Ogg page.
pub fn has_audio_page(data: &[u8]) -> bool {
    let mut position = 0usize;
    while position < data.len() {
        let Some(page) = page_at(data, position) else {
            return false;
        };
        if !page.header && page.has_payload {
            return true;
        }
        position = page.next;
    }
    false
}

/// Return the byte length of the first complete five-packet tile.
///
/// Header pages (`OpusHead`, `OpusTags`) are included with the first tile.
/// The final tile can be shorter and is emitted by the codec parser's `flush`.
pub fn tile_bytes(data: &[u8]) -> Option<usize> {
    if data.len() < 4 || &data[..4] != OGG_HEADER {
        return None;
    }
    let mut position = 0usize;
    let mut audio_pages = 0usize;
    let mut saw_header = false;
    while position < data.len() {
        let page = page_at(data, position)?;
        if page.header {
            saw_header = true;
        } else if page.has_payload {
            audio_pages += 1;
            if audio_pages == TILE_PACKETS {
                return Some(page.next);
            }
        }
        position = page.next;
    }
    let _ = saw_header;
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use salsa_spatial::{MultistreamPacket, OggSalsaWriter, SalsaSpatialFrame};

    fn elementary(tag: u8) -> Vec<u8> {
        let mut packet = vec![(31 << 3) | 0];
        packet.extend_from_slice(&[tag; 7]);
        packet
    }

    fn frame(start: u64, count: usize) -> SalsaSpatialFrame {
        let packets = (0..count)
            .map(|index| {
                MultistreamPacket::new(
                    (0..6u8)
                        .map(|stream| elementary(index as u8 + stream))
                        .collect(),
                )
            })
            .collect();
        SalsaSpatialFrame {
            sample_rate: 48_000,
            start_sample: start,
            frames: count * 960,
            packets,
            enhancement: salsa_spatial::EnhancementTile {
                start_sample: start,
                frames: count * 960,
                bytes: Vec::new(),
                snapshot: false,
            },
            objects: Vec::new(),
            events: Vec::new(),
            snapshot: false,
            parameters: None,
        }
    }

    #[test]
    fn finds_the_second_tile_marker() {
        let (mut writer, mut data) =
            OggSalsaWriter::new(salsa_spatial::SalsaSpatialStreamConfig::default(), 1, 120)
                .unwrap();
        data.extend(writer.write_frame(&frame(0, 5)).unwrap());
        let first_tile = data.len();
        data.extend(writer.write_frame(&frame(4800, 5)).unwrap());
        assert_eq!(tile_bytes(&data), Some(first_tile));
    }

    #[test]
    fn headers_alone_do_not_form_a_tile() {
        let (_, headers) =
            OggSalsaWriter::new(salsa_spatial::SalsaSpatialStreamConfig::default(), 1, 120)
                .unwrap();
        assert!(!headers.is_empty());
        assert_eq!(tile_bytes(&headers), None);
        let (mut writer, mut full) =
            OggSalsaWriter::new(salsa_spatial::SalsaSpatialStreamConfig::default(), 1, 120)
                .unwrap();
        let header_len = full.len();
        full.extend(writer.write_frame(&frame(0, 5)).unwrap());
        let mut truncated = headers;
        truncated.extend_from_slice(&full[header_len..full.len() - 10]);
        assert_eq!(tile_bytes(&truncated), None);
    }

    #[test]
    fn waits_for_a_truncated_page() {
        let (mut writer, mut data) =
            OggSalsaWriter::new(salsa_spatial::SalsaSpatialStreamConfig::default(), 1, 120)
                .unwrap();
        data.extend(writer.write_frame(&frame(0, 5)).unwrap());
        data.pop();
        assert_eq!(tile_bytes(&data), None);
    }
}
