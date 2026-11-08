//! RFC 6716 Appendix B self-delimiting framing for Opus multistream packets.
//!
//! Each substream except the last is stored in Opus self-delimited form: an
//! extra size for the last frame is inserted after the normal frame-length
//! headers and before the frame data. The last substream uses ordinary Opus
//! framing. This is the format produced by `opus_multistream_encode` and
//! consumed by `opus_multistream_decode`.

use crate::error::{Result, SpatialError};

/// Maximum elementary Opus packet size accepted by the framing layer.
pub const MAX_SUBSTREAM_BYTES: usize = 1275 * 48;

fn read_size(data: &[u8], position: &mut usize) -> Result<usize> {
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

fn write_size(out: &mut Vec<u8>, len: usize) -> Result<()> {
    if len > 1275 {
        return Err(SpatialError::InvalidValue(
            "Opus frame exceeds the self-delimiting size limit",
        ));
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

/// Parsed layout of one elementary Opus packet.
#[derive(Debug, Clone)]
struct StreamLayout {
    toc: u8,
    /// Bytes between the TOC and the first frame in ordinary framing.
    header: Vec<u8>,
    frame_sizes: Vec<usize>,
    padding_len: usize,
    /// Total bytes consumed from the input.
    consumed: usize,
    /// Offset of the first frame byte in the input.
    frame_start: usize,
}

impl StreamLayout {
    fn normal_packet(&self, data: &[u8]) -> Result<Vec<u8>> {
        let end = self.frame_start + self.frame_sizes.iter().sum::<usize>();
        let frame_data = data
            .get(self.frame_start..end)
            .ok_or(SpatialError::Truncated)?;
        let mut out =
            Vec::with_capacity(1 + self.header.len() + frame_data.len() + self.padding_len);
        out.push(self.toc);
        out.extend_from_slice(&self.header);
        out.extend_from_slice(frame_data);
        out.extend_from_slice(&data[end..end + self.padding_len]);
        Ok(out)
    }
}

/// Parse one elementary Opus packet, optionally in self-delimited form.
fn parse_stream(data: &[u8], self_delimited: bool) -> Result<StreamLayout> {
    if data.is_empty() {
        return Err(SpatialError::Truncated);
    }
    let toc = data[0];
    let code = toc & 0x03;
    let mut position = 1usize;
    let mut header = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    let mut padding_len = 0usize;
    let count: usize;
    let cbr: bool;

    match code {
        0 => {
            count = 1;
            cbr = true;
        }
        1 => {
            count = 2;
            cbr = true;
        }
        2 => {
            count = 2;
            cbr = false;
            let first = read_size(data, &mut position)?;
            write_size(&mut header, first)?;
            sizes.push(first);
        }
        _ => {
            let count_byte = *data.get(position).ok_or(SpatialError::Truncated)?;
            position += 1;
            header.push(count_byte);
            count = (count_byte & 0x3f) as usize;
            if count == 0 || count > 48 {
                return Err(SpatialError::InvalidValue("invalid Opus frame count"));
            }
            cbr = count_byte & 0x80 == 0;
            if count_byte & 0x40 != 0 {
                loop {
                    let byte = *data.get(position).ok_or(SpatialError::Truncated)?;
                    position += 1;
                    header.push(byte);
                    padding_len += if byte == 255 { 254 } else { byte as usize };
                    if byte != 255 {
                        break;
                    }
                }
            }
            if !cbr {
                for _ in 0..count - 1 {
                    let size = read_size(data, &mut position)?;
                    write_size(&mut header, size)?;
                    sizes.push(size);
                }
            }
        }
    }

    // Self-delimited framing carries an explicit size for the last frame.
    if self_delimited {
        let last = read_size(data, &mut position)?;
        sizes.push(last);
        if cbr {
            sizes = vec![last; count];
        }
    }

    let frame_start = position;
    let available = data
        .len()
        .checked_sub(frame_start)
        .ok_or(SpatialError::Truncated)?;
    let frame_bytes = if self_delimited {
        sizes.iter().sum::<usize>()
    } else {
        available
            .checked_sub(padding_len)
            .ok_or(SpatialError::Truncated)?
    };
    let consumed = frame_start
        .checked_add(frame_bytes)
        .and_then(|value| value.checked_add(padding_len))
        .ok_or(SpatialError::InvalidValue("Opus packet length overflow"))?;
    if consumed > data.len() {
        return Err(SpatialError::Truncated);
    }

    if !self_delimited {
        if cbr {
            if count == 0 || frame_bytes % count != 0 {
                return Err(SpatialError::Syntax("CBR Opus payload is not divisible"));
            }
            sizes = vec![frame_bytes / count; count];
        } else {
            let used = sizes.iter().sum::<usize>();
            if used > frame_bytes {
                return Err(SpatialError::Syntax("Opus frame sizes overrun the packet"));
            }
            sizes.push(frame_bytes - used);
        }
    }
    if sizes.len() != count || sizes.iter().any(|size| *size > 1275) {
        return Err(SpatialError::InvalidValue("invalid Opus frame layout"));
    }
    if sizes.iter().sum::<usize>() != frame_bytes {
        return Err(SpatialError::Syntax(
            "Opus frame sizes disagree with the packet",
        ));
    }

    Ok(StreamLayout {
        toc,
        header,
        frame_sizes: sizes,
        padding_len,
        consumed,
        frame_start,
    })
}

/// Convert one ordinary Opus packet to self-delimited form.
fn to_self_delimited(packet: &[u8]) -> Result<Vec<u8>> {
    let layout = parse_stream(packet, false)?;
    if layout.consumed != packet.len() {
        return Err(SpatialError::Syntax("trailing bytes in an Opus packet"));
    }
    let last = *layout
        .frame_sizes
        .last()
        .ok_or(SpatialError::InvalidValue("Opus packet has no frames"))?;
    let end = layout.frame_start + layout.frame_sizes.iter().sum::<usize>();
    let mut out = Vec::with_capacity(packet.len() + 2);
    out.push(layout.toc);
    out.extend_from_slice(&layout.header);
    write_size(&mut out, last)?;
    out.extend_from_slice(&packet[layout.frame_start..end]);
    out.extend_from_slice(&packet[end..end + layout.padding_len]);
    Ok(out)
}

/// Split a multistream packet into exactly `streams` ordinary Opus packets.
pub fn split(packet: &[u8], streams: usize) -> Result<Vec<Vec<u8>>> {
    if streams == 0 || streams > crate::constants::MAX_STREAM_COUNT {
        return Err(SpatialError::InvalidValue("invalid substream count"));
    }
    if packet.len() > streams.saturating_mul(MAX_SUBSTREAM_BYTES) {
        return Err(SpatialError::InvalidValue(
            "multistream packet is too large",
        ));
    }
    let mut position = 0usize;
    let mut out = Vec::with_capacity(streams);
    for index in 0..streams {
        let self_delimited = index + 1 != streams;
        let layout = parse_stream(&packet[position..], self_delimited)?;
        let end = position
            .checked_add(layout.consumed)
            .ok_or(SpatialError::InvalidValue("substream length overflow"))?;
        if end > packet.len() {
            return Err(SpatialError::Truncated);
        }
        out.push(layout.normal_packet(&packet[position..end])?);
        position = end;
    }
    if position != packet.len() {
        return Err(SpatialError::Syntax("trailing multistream bytes"));
    }
    Ok(out)
}

/// Concatenate elementary packets into one multistream packet.
///
/// Every packet except the last is converted to self-delimited framing.
pub fn pack(substreams: &[&[u8]]) -> Result<Vec<u8>> {
    if substreams.is_empty() || substreams.len() > crate::constants::MAX_STREAM_COUNT {
        return Err(SpatialError::InvalidValue("invalid substream count"));
    }
    let mut out = Vec::new();
    for (index, stream) in substreams.iter().enumerate() {
        if stream.is_empty() {
            return Err(SpatialError::InvalidValue("empty Opus substream"));
        }
        if index + 1 == substreams.len() {
            out.extend_from_slice(stream);
        } else {
            out.extend(to_self_delimited(stream)?);
        }
    }
    Ok(out)
}

/// Pack owned substreams.
pub fn pack_owned(substreams: &[Vec<u8>]) -> Result<Vec<u8>> {
    let slices: Vec<&[u8]> = substreams.iter().map(Vec::as_slice).collect();
    pack(&slices)
}

/// Replace one elementary packet in a multistream packet, preserving framing.
pub fn replace_substream(
    packet: &[u8],
    streams: usize,
    index: usize,
    data: &[u8],
) -> Result<Vec<u8>> {
    let mut parts = split(packet, streams)?;
    let slot = parts
        .get_mut(index)
        .ok_or(SpatialError::InvalidValue("substream index out of range"))?;
    *slot = data.to_vec();
    pack_owned(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus_packet(frame_len: usize, tag: u8) -> Vec<u8> {
        // Code 0 CELT fullband mono/stereo-compatible TOC.
        let mut packet = vec![(31 << 3) | 0];
        packet.extend(std::iter::repeat(tag).take(frame_len));
        packet
    }

    #[test]
    fn self_delimited_round_trip_variable_lengths() {
        let parts = vec![
            opus_packet(3, 1),
            opus_packet(300, 2),
            opus_packet(1, 3),
            opus_packet(1275, 4),
            opus_packet(17, 5),
        ];
        let packed = pack_owned(&parts).unwrap();
        assert_eq!(split(&packed, parts.len()).unwrap(), parts);
    }

    #[test]
    fn self_delimited_layout_matches_libopus_style() {
        // One mono code-0 frame of three bytes becomes TOC, size, frame.
        let part = opus_packet(3, 0xaa);
        let packed = pack(&[part.as_slice(), &opus_packet(3, 0xbb)]).unwrap();
        assert_eq!(&packed[..5], &[part[0], 3, 0xaa, 0xaa, 0xaa]);
    }

    #[test]
    fn truncated_self_delimited_frame_is_rejected() {
        let parts = vec![opus_packet(4, 1), opus_packet(4, 2)];
        let packed = pack_owned(&parts).unwrap();
        // TOC and last-frame size are present, but the frame data is not.
        assert!(split(&packed[..2], 2).is_err());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let parts = vec![opus_packet(1276, 1), opus_packet(1, 2)];
        assert!(pack_owned(&parts).is_err());
    }

    #[test]
    fn replace_keeps_other_substreams() {
        let parts = vec![opus_packet(4, 1), opus_packet(8, 2), opus_packet(2, 3)];
        let packed = pack_owned(&parts).unwrap();
        let replacement = opus_packet(5, 9);
        let replaced = replace_substream(&packed, 3, 1, &replacement).unwrap();
        let split = split(&replaced, 3).unwrap();
        assert_eq!(split[0], parts[0]);
        assert_eq!(split[1], replacement);
        assert_eq!(split[2], parts[2]);
    }
}
