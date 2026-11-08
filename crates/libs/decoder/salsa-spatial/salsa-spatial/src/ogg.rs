//! Minimal Ogg Opus container support for the first reference writer/reader.
//!
//! The payload in every Ogg packet is one standard Opus multistream packet.
//! `OpusHead` uses mapping family 1 and the public 7.1 speaker map. The private
//! Salsa configuration remains inside the center substream's Opus padding.

use crate::descriptor::CodecBinding;
use crate::error::{Result, SpatialError};

const OGG_HEADER: &[u8; 4] = b"OggS";
const OPUS_HEAD: &[u8; 8] = b"OpusHead";
const OPUS_TAGS: &[u8; 8] = b"OpusTags";
const BOS: u8 = 0x02;
const EOS: u8 = 0x04;

/// Decoded `OpusHead`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    pub version: u8,
    pub channels: usize,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
    pub mapping_family: u8,
    pub stream_count: usize,
    pub coupled_count: usize,
    pub channel_mapping: Vec<u8>,
}

impl OpusHead {
    pub fn from_binding(binding: &CodecBinding, pre_skip: u16) -> Self {
        Self {
            version: 1,
            channels: binding.output_channels,
            pre_skip,
            input_sample_rate: crate::constants::SAMPLE_RATE,
            output_gain: 0,
            mapping_family: binding.mapping_family,
            stream_count: binding.stream_count,
            coupled_count: binding.coupled_count,
            channel_mapping: binding.channel_mapping.clone(),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(19 + self.channel_mapping.len());
        out.extend_from_slice(OPUS_HEAD);
        out.push(self.version);
        out.push(
            u8::try_from(self.channels)
                .map_err(|_| SpatialError::InvalidValue("too many Opus channels"))?,
        );
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain.to_le_bytes());
        out.push(self.mapping_family);
        if self.mapping_family != 0 {
            out.push(
                u8::try_from(self.stream_count)
                    .map_err(|_| SpatialError::InvalidValue("too many Opus streams"))?,
            );
            out.push(
                u8::try_from(self.coupled_count)
                    .map_err(|_| SpatialError::InvalidValue("too many coupled streams"))?,
            );
            if self.channel_mapping.len() != self.channels {
                return Err(SpatialError::InvalidValue(
                    "OpusHead channel map length mismatch",
                ));
            }
            out.extend_from_slice(&self.channel_mapping);
        }
        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 19 || &data[..8] != OPUS_HEAD {
            return Err(SpatialError::Syntax("bad OpusHead"));
        }
        let version = data[8];
        let channels = data[9] as usize;
        let pre_skip = u16::from_le_bytes([data[10], data[11]]);
        let input_sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let output_gain = i16::from_le_bytes([data[16], data[17]]);
        let mapping_family = data[18];
        let mut stream_count = 0;
        let mut coupled_count = 0;
        let mut channel_mapping = Vec::new();
        if mapping_family != 0 {
            if data.len() < 21 + channels {
                return Err(SpatialError::Truncated);
            }
            stream_count = data[19] as usize;
            coupled_count = data[20] as usize;
            channel_mapping = data[21..21 + channels].to_vec();
        }
        Ok(Self {
            version,
            channels,
            pre_skip,
            input_sample_rate,
            output_gain,
            mapping_family,
            stream_count,
            coupled_count,
            channel_mapping,
        })
    }

    /// Public codec binding. `logical_to_coded` is recovered later from the
    /// private configuration record.
    pub fn public_binding(&self) -> CodecBinding {
        CodecBinding {
            output_channels: self.channels,
            mapping_family: self.mapping_family,
            stream_count: self.stream_count,
            coupled_count: self.coupled_count,
            channel_mapping: self.channel_mapping.clone(),
            logical_to_coded: Vec::new(),
        }
    }
}

/// One demuxed Ogg packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggPacket {
    pub data: Vec<u8>,
    pub granule_position: u64,
    pub eos: bool,
}

/// Streaming Ogg Opus multiplexer.
#[derive(Debug)]
pub struct OggOpusWriter {
    serial: u32,
    sequence: u32,
    pre_skip: u16,
    total_samples: u64,
    header_sent: bool,
    eos_sent: bool,
}

impl OggOpusWriter {
    pub fn new(serial: u32, pre_skip: u16) -> Self {
        Self {
            serial,
            sequence: 0,
            pre_skip,
            total_samples: 0,
            header_sent: false,
            eos_sent: false,
        }
    }

    /// Emit the BOS `OpusHead` page and the `OpusTags` page.
    pub fn headers(&mut self, binding: &CodecBinding, vendor: &str) -> Result<Vec<u8>> {
        let head = OpusHead::from_binding(binding, self.pre_skip).encode()?;
        let mut tags = Vec::new();
        tags.extend_from_slice(OPUS_TAGS);
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor.as_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        let mut out = self.page(&head, BOS, 0);
        out.extend(self.page(&tags, 0, 0));
        self.header_sent = true;
        Ok(out)
    }

    /// Append one multistream packet, advancing the granule position.
    ///
    /// Audio-page granules count decoded samples without the pre-skip. Only the
    /// final page adds the pre-skip and acts as the end trim.
    pub fn packet(&mut self, packet: &[u8], samples: usize) -> Result<Vec<u8>> {
        if !self.header_sent {
            return Err(SpatialError::InvalidValue("Ogg headers were not written"));
        }
        if self.eos_sent {
            return Err(SpatialError::InvalidValue("Ogg stream already ended"));
        }
        self.total_samples = self.total_samples.saturating_add(samples as u64);
        Ok(self.page(packet, 0, self.total_samples))
    }

    /// Append one tail packet and mark it as the final EOS page.
    ///
    /// The tail audio is not counted in the programme granule; the final
    /// granule (`pre_skip + programme samples`) trims it at playback.
    pub fn tail_packet(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        if !self.header_sent {
            return Err(SpatialError::InvalidValue("Ogg headers were not written"));
        }
        if self.eos_sent {
            return Err(SpatialError::InvalidValue("Ogg stream already ended"));
        }
        self.eos_sent = true;
        let granule = u64::from(self.pre_skip).saturating_add(self.total_samples);
        Ok(self.page(packet, EOS, granule))
    }

    /// Emit the end-of-stream page with the final granule position.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.eos_sent {
            return Vec::new();
        }
        self.eos_sent = true;
        let granule = u64::from(self.pre_skip).saturating_add(self.total_samples);
        self.page(&[], EOS, granule)
    }

    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    fn page(&mut self, packet: &[u8], flags: u8, granule: u64) -> Vec<u8> {
        let mut lacing = Vec::new();
        let mut remaining = packet.len();
        loop {
            let chunk = remaining.min(255);
            lacing.push(chunk as u8);
            remaining -= chunk;
            if remaining == 0 {
                // Terminate with a zero lacing value only when the payload is
                // an exact multiple of 255 and is non-empty.
                if chunk == 255 {
                    lacing.push(0);
                }
                break;
            }
        }
        if packet.is_empty() {
            lacing.clear();
        }

        let mut out = Vec::with_capacity(27 + lacing.len() + packet.len());
        out.extend_from_slice(OGG_HEADER);
        out.push(0);
        out.push(flags);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // checksum placeholder
        out.push(lacing.len() as u8);
        out.extend_from_slice(&lacing);
        out.extend_from_slice(packet);
        let checksum = ogg_crc32(&out);
        out[22..26].copy_from_slice(&checksum.to_le_bytes());
        self.sequence = self.sequence.wrapping_add(1);
        out
    }
}

/// Streaming Ogg Opus demultiplexer.
#[derive(Debug, Default)]
pub struct OggOpusReader {
    pending: Vec<u8>,
    partial: Vec<u8>,
    head: Option<OpusHead>,
    saw_tags: bool,
    serial: Option<u32>,
}

impl OggOpusReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn head(&self) -> Option<&OpusHead> {
        self.head.as_ref()
    }

    pub fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<OggPacket>> {
        self.pending.extend_from_slice(data);
        let mut packets = Vec::new();
        loop {
            let Some(page) = parse_page(&self.pending)? else {
                break;
            };
            self.pending.drain(..page.next);
            if let Some(expected) = self.serial {
                if expected != page.serial {
                    return Err(SpatialError::InvalidValue("Ogg serial number changed"));
                }
            } else {
                self.serial = Some(page.serial);
            }

            let continued = page.header_type & 0x01 != 0;
            if continued != !self.partial.is_empty() {
                return Err(SpatialError::Syntax("Ogg packet continuation mismatch"));
            }

            let mut segment_start = 0usize;
            let mut emitted = Vec::new();
            for value in &page.lacing {
                let length = *value as usize;
                let end = segment_start
                    .checked_add(length)
                    .ok_or(SpatialError::Truncated)?;
                if end > page.payload.len() {
                    return Err(SpatialError::Truncated);
                }
                self.partial
                    .extend_from_slice(&page.payload[segment_start..end]);
                segment_start = end;
                if length < 255 {
                    emitted.push(std::mem::take(&mut self.partial));
                }
            }
            if segment_start != page.payload.len() {
                return Err(SpatialError::Syntax("Ogg lacing does not cover payload"));
            }

            let eos = page.header_type & EOS != 0;
            for data in emitted {
                if data.starts_with(OPUS_HEAD) {
                    self.head = Some(OpusHead::decode(&data)?);
                } else if !self.saw_tags && data.starts_with(OPUS_TAGS) {
                    self.saw_tags = true;
                } else if !data.is_empty() {
                    // A midstream join or seek starts on an audio page without
                    // OpusHead/OpusTags. The caller supplies the binding.
                    packets.push(OggPacket {
                        data,
                        granule_position: page.granule,
                        eos,
                    });
                }
            }
        }
        Ok(packets)
    }

    pub fn flush(&mut self) -> Result<Vec<OggPacket>> {
        if !self.pending.is_empty() || !self.partial.is_empty() {
            return Err(SpatialError::Truncated);
        }
        Ok(Vec::new())
    }
}

struct Page {
    header_type: u8,
    granule: u64,
    serial: u32,
    lacing: Vec<u8>,
    payload: Vec<u8>,
    next: usize,
}

fn parse_page(data: &[u8]) -> Result<Option<Page>> {
    if data.len() < 27 {
        return Ok(None);
    }
    if &data[..4] != OGG_HEADER {
        return Err(SpatialError::Syntax("bad Ogg capture pattern"));
    }
    let header_type = data[5];
    let granule = u64::from_le_bytes([
        data[6], data[7], data[8], data[9], data[10], data[11], data[12], data[13],
    ]);
    let serial = u32::from_le_bytes([data[14], data[15], data[16], data[17]]);
    let segments = data[26] as usize;
    let lacing_end = 27 + segments;
    if data.len() < lacing_end {
        return Ok(None);
    }
    let lacing = data[27..lacing_end].to_vec();
    let payload_len: usize = lacing.iter().map(|value| *value as usize).sum();
    let end = lacing_end + payload_len;
    if data.len() < end {
        return Ok(None);
    }
    let expected = u32::from_le_bytes([data[22], data[23], data[24], data[25]]);
    let mut copy = data[..end].to_vec();
    copy[22..26].fill(0);
    if ogg_crc32(&copy) != expected {
        return Err(SpatialError::Syntax("bad Ogg CRC"));
    }
    Ok(Some(Page {
        header_type,
        granule,
        serial,
        lacing,
        payload: data[lacing_end..end].to_vec(),
        next: end,
    }))
}

/// Detect a Salsa Ogg Opus stream from its `OpusTags` vendor string.
pub fn has_salsa_stream(data: &[u8]) -> bool {
    if data.len() < 4 || &data[..4] != OGG_HEADER {
        return false;
    }
    // The private configuration record is carried in the first center packet.
    // Remuxers may replace the OpusTags vendor, so the record magic is the
    // reliable signal once the file has been copied through another container.
    if data.windows(4).any(|window| window == b"SCFG") {
        return true;
    }
    let Some(position) = data
        .windows(OPUS_TAGS.len())
        .position(|window| window == OPUS_TAGS)
    else {
        return false;
    };
    let vendor_len_start = position + OPUS_TAGS.len();
    let Some(length_bytes) = data.get(vendor_len_start..vendor_len_start + 4) else {
        return false;
    };
    let length = u32::from_le_bytes([
        length_bytes[0],
        length_bytes[1],
        length_bytes[2],
        length_bytes[3],
    ]) as usize;
    data.get(vendor_len_start + 4..vendor_len_start + 4 + length)
        .is_some_and(|vendor| vendor.starts_with(b"salsa-spatial"))
}

/// Ogg CRC-32 (polynomial 0x04c11db7, no input/output reflection, init 0).
pub fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for byte in data {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_known_ogg_vector() {
        // The Ogg CRC of the ASCII string "OggS" with the high-bit-first
        // polynomial.
        assert_eq!(ogg_crc32(b"OggS"), 0x5fb0_a94f);
    }

    #[test]
    fn salsa_detection_accepts_config_magic_after_remux() {
        assert!(has_salsa_stream(b"OggS........SCFG................"));
        assert!(!has_salsa_stream(b"OggS........OpusTags................"));
        assert!(!has_salsa_stream(b"not-ogg SCFG"));
    }

    #[test]
    fn head_round_trips() {
        let binding = CodecBinding::initial();
        let head = OpusHead::from_binding(&binding, 120);
        assert_eq!(OpusHead::decode(&head.encode().unwrap()).unwrap(), head);
    }

    #[test]
    fn muxer_and_demuxer_round_trip_packets() {
        let binding = CodecBinding::initial();
        let mut writer = OggOpusWriter::new(0x1234, 120);
        let mut bytes = writer.headers(&binding, "salsa").unwrap();
        let packets = [vec![1u8; 30], vec![2u8; 300], vec![3u8; 7]];
        for packet in &packets {
            bytes.extend(writer.packet(packet, 960).unwrap());
        }
        bytes.extend(writer.finish());

        let mut reader = OggOpusReader::new();
        let parsed = reader.push_bytes(&bytes).unwrap();
        assert_eq!(parsed.len(), packets.len());
        assert_eq!(reader.head().unwrap().channels, binding.output_channels);
        for (index, (packet, parsed)) in packets.iter().zip(parsed).enumerate() {
            assert_eq!(&parsed.data, packet);
            assert_eq!(parsed.granule_position, 960 * (index as u64 + 1));
        }
    }
}
