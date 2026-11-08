//! Salsa over one standard Opus multistream track.
//!
//! [`MultistreamWriter`] attaches private records to the mono center substream.
//! [`MultistreamReader`] splits standard multistream packets, recovers hidden
//! coded channels into a core-first logical buffer and reassembles tiles.
//! [`SalsaSpatialParser`] adds Ogg Opus framing for the reference writer.

use objectaudio_ir::SpatialParser;

use crate::constants::{OPUS_FRAME_SAMPLES, SAMPLE_RATE, TILE_PACKETS};
use crate::descriptor::CodecBinding;
use crate::error::{Result, SpatialError};
use crate::frame::{
    EnhancementTile, MultistreamPacket, SalsaSpatialFrame, SalsaSpatialStreamConfig,
};
use crate::multistream;
use crate::ogg::OggOpusReader;
use crate::opus_ext;
use crate::record::{
    Fragment, GroupActivity, Reassembler, Record, TileRecord, parse_records, write_record,
};

/// Attaches versioned Salsa records to the center substream of assembled
/// multistream packets.
#[derive(Debug, Clone)]
pub struct MultistreamWriter {
    config: SalsaSpatialStreamConfig,
    last_config_sample: Option<u64>,
}

impl MultistreamWriter {
    pub fn new(config: SalsaSpatialStreamConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            last_config_sample: None,
        })
    }

    pub fn config(&self) -> &SalsaSpatialStreamConfig {
        &self.config
    }

    /// Attach configuration and tile records to one 100 ms tile.
    ///
    /// `frame.packets` must already contain one elementary packet per
    /// substream. The returned packets have the metadata extension appended to
    /// the center substream and are ready for container framing.
    pub fn write_frame(&mut self, frame: &SalsaSpatialFrame) -> Result<Vec<MultistreamPacket>> {
        if frame.packets.is_empty() {
            return Ok(Vec::new());
        }
        let streams = self.config.codec.stream_count;
        for packet in &frame.packets {
            if packet.len() != streams {
                return Err(SpatialError::InvalidValue(
                    "packet substream count disagrees with the configuration",
                ));
            }
        }

        let resend = match self.last_config_sample {
            None => true,
            Some(sample) => {
                frame.snapshot
                    && frame.start_sample.saturating_sub(sample) >= u64::from(SAMPLE_RATE)
            }
        };
        let mut bundle = Vec::new();
        if resend {
            bundle.extend(write_record(&Record::Configuration(self.config.clone()))?);
            self.last_config_sample = Some(frame.start_sample);
        }
        bundle.extend(write_record(&Record::Tile(TileRecord {
            start_sample: frame.start_sample,
            frames: frame.frames as u32,
            snapshot: frame.snapshot,
            enhancement: frame.enhancement.clone(),
            objects: frame.objects.clone(),
            events: frame.events.clone(),
        }))?);
        if !frame.objects.is_empty() {
            let active_groups: Vec<u32> = self
                .config
                .observation_groups
                .iter()
                .map(|group| group.group_id)
                .collect();
            bundle.extend(write_record(&Record::GroupActivity(GroupActivity {
                start_sample: frame.start_sample,
                active_groups,
                active_rank: frame
                    .enhancement
                    .bytes
                    .first()
                    .map_or(0, |value| u32::from(*value)),
            }))?);
        }

        let center = self.config.codec.center_substream_index()?;
        let fragments = Fragment::split(
            self.config.config_id,
            frame.start_sample,
            &bundle,
            frame.packets.len(),
        )?;
        let mut out = Vec::with_capacity(frame.packets.len());
        for (packet, fragment) in frame.packets.iter().zip(fragments) {
            let mut packet = packet.clone();
            let ordinary = packet
                .substreams
                .get(center)
                .ok_or(SpatialError::InvalidValue("missing center substream"))?;
            let extended = opus_ext::append_extension(ordinary, &fragment)?;
            // The extension lives in Opus padding, so it does not count toward
            // the 1275-byte per-frame limit.  Only the total Opus packet size is
            // bounded.  The center substream is not the last substream, so it
            // is carried in self-delimited framing and may use all of it.
            if extended.len() > multistream::MAX_SUBSTREAM_BYTES {
                return Err(SpatialError::InvalidValue(
                    "center packet plus metadata exceeds the Opus packet limit",
                ));
            }
            packet.substreams[center] = extended;
            out.push(packet);
        }
        Ok(out)
    }
}

/// Parses standard multistream packets into complete Salsa tiles.
#[derive(Debug, Default)]
pub struct MultistreamReader {
    binding: Option<CodecBinding>,
    config: Option<SalsaSpatialStreamConfig>,
    reassembler: Reassembler,
    packets: Vec<MultistreamPacket>,
    tile: Option<TileRecord>,
    next_sample: u64,
}

impl MultistreamReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_binding(binding: CodecBinding) -> Self {
        Self {
            binding: Some(binding),
            ..Self::default()
        }
    }

    pub fn set_binding(&mut self, binding: CodecBinding) {
        self.binding = Some(binding);
    }

    pub fn binding(&self) -> Option<&CodecBinding> {
        self.binding.as_ref()
    }

    pub fn config(&self) -> Option<&SalsaSpatialStreamConfig> {
        self.config.as_ref()
    }

    /// Push one packed Opus multistream packet.
    pub fn push_packet_bytes(&mut self, data: &[u8]) -> Result<Vec<SalsaSpatialFrame>> {
        let binding = self
            .binding
            .as_ref()
            .ok_or(SpatialError::InvalidValue("no Opus codec binding"))?;
        let parts = multistream::split(data, binding.stream_count)?;
        self.push_packet(MultistreamPacket::new(parts))
    }

    /// Push one already split multistream packet.
    pub fn push_packet(&mut self, mut packet: MultistreamPacket) -> Result<Vec<SalsaSpatialFrame>> {
        let binding = self
            .binding
            .as_ref()
            .ok_or(SpatialError::InvalidValue("no Opus codec binding"))?
            .clone();
        if packet.len() != binding.stream_count {
            return Err(SpatialError::InvalidValue(
                "packet substream count disagrees with the binding",
            ));
        }
        let center = binding.center_substream_index()?;
        let mut complete = None;
        if let Some(substream) = packet.substreams.get(center) {
            let (ordinary, payload) = opus_ext::strip_extension(substream)?;
            packet.substreams[center] = ordinary;
            if let Some(payload) = payload {
                let fragment = Fragment::parse(&payload)?;
                if let Some(bundle) = self.reassembler.push(fragment)? {
                    complete = Some(bundle);
                }
            }
        }
        if let Some(bundle) = complete {
            self.consume_bundle(&bundle)?;
        }
        self.packets.push(packet);

        let mut frames = Vec::new();
        if self.packets.len() >= TILE_PACKETS {
            self.emit_tile(&mut frames)?;
        }
        Ok(frames)
    }

    /// Emit a partial final tile, if any.
    pub fn flush(&mut self) -> Result<Vec<SalsaSpatialFrame>> {
        let mut frames = Vec::new();
        if !self.packets.is_empty() {
            self.emit_tile(&mut frames)?;
        }
        Ok(frames)
    }

    pub fn reset(&mut self) {
        self.reassembler.reset();
        self.packets.clear();
        self.tile = None;
        self.next_sample = 0;
    }

    fn consume_bundle(&mut self, bundle: &[u8]) -> Result<()> {
        for record in parse_records(bundle)? {
            match record {
                Record::Configuration(config) => {
                    if let Some(binding) = &self.binding {
                        // The private configuration must agree with the
                        // standard Opus header before enhanced decoding.
                        if binding.stream_count != config.codec.stream_count
                            || binding.coupled_count != config.codec.coupled_count
                            || binding.output_channels != config.codec.output_channels
                            || binding.mapping_family != config.codec.mapping_family
                            || binding.channel_mapping != config.codec.channel_mapping
                        {
                            return Err(SpatialError::InvalidValue(
                                "private configuration disagrees with OpusHeader",
                            ));
                        }
                    }
                    self.config = Some(config);
                }
                Record::Tile(tile) => self.tile = Some(tile),
                Record::GroupActivity(_) => {}
            }
        }
        Ok(())
    }

    fn emit_tile(&mut self, frames: &mut Vec<SalsaSpatialFrame>) -> Result<()> {
        let packets = std::mem::take(&mut self.packets);
        let streams = self
            .binding
            .as_ref()
            .map_or(0, |binding| binding.stream_count);
        let fallback_frames = packets.len() * OPUS_FRAME_SAMPLES;
        let tile = self.tile.take();
        let (start_sample, frame_count, enhancement, objects, events, snapshot) = match tile {
            Some(tile) => {
                let frames = if tile.frames > 0 {
                    tile.frames as usize
                } else {
                    fallback_frames
                };
                (
                    tile.start_sample,
                    frames,
                    tile.enhancement,
                    tile.objects,
                    tile.events,
                    tile.snapshot,
                )
            }
            None => (
                self.next_sample,
                fallback_frames,
                EnhancementTile {
                    start_sample: self.next_sample,
                    frames: fallback_frames,
                    bytes: Vec::new(),
                    snapshot: false,
                },
                Vec::new(),
                Vec::new(),
                false,
            ),
        };
        let _ = streams;
        self.next_sample = start_sample.saturating_add(frame_count as u64);
        frames.push(SalsaSpatialFrame {
            sample_rate: SAMPLE_RATE,
            start_sample,
            frames: frame_count,
            packets,
            enhancement,
            objects,
            events,
            snapshot,
            parameters: None,
        });
        Ok(())
    }
}

/// Ogg-backed streaming parser for the reference container.
#[derive(Debug, Default)]
pub struct SalsaSpatialParser {
    ogg: OggOpusReader,
    reader: MultistreamReader,
}

impl SalsaSpatialParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn config(&self) -> Option<&SalsaSpatialStreamConfig> {
        self.reader.config()
    }

    pub fn reader(&self) -> &MultistreamReader {
        &self.reader
    }

    /// Push one pre-demultiplexed multistream packet.
    pub fn push_packet(&mut self, packet: MultistreamPacket) -> Result<Vec<SalsaSpatialFrame>> {
        self.reader.push_packet(packet)
    }

    /// Set the binding recovered by a container adapter.
    pub fn set_binding(&mut self, binding: CodecBinding) {
        self.reader.set_binding(binding);
    }
}

impl SpatialParser for SalsaSpatialParser {
    type Frame = SalsaSpatialFrame;
    type Error = SpatialError;

    fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<Self::Frame>> {
        let packets = self.ogg.push_bytes(data)?;
        if let Some(head) = self.ogg.head()
            && self.reader.binding().is_none()
        {
            self.reader.set_binding(head.public_binding());
        }
        let mut frames = Vec::new();
        for packet in packets {
            frames.extend(self.reader.push_packet_bytes(&packet.data)?);
        }
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<Self::Frame>> {
        self.ogg.flush()?;
        self.reader.flush()
    }
}

/// Ogg Opus reference writer. It muxes one multistream packet per 20 ms.
#[derive(Debug)]
pub struct OggSalsaWriter {
    config: SalsaSpatialStreamConfig,
    writer: MultistreamWriter,
    ogg: crate::ogg::OggOpusWriter,
}

impl OggSalsaWriter {
    pub fn new(
        config: SalsaSpatialStreamConfig,
        serial: u32,
        pre_skip: u16,
    ) -> Result<(Self, Vec<u8>)> {
        config.validate()?;
        let writer = MultistreamWriter::new(config.clone())?;
        let mut ogg = crate::ogg::OggOpusWriter::new(serial, pre_skip);
        let header = ogg.headers(&config.codec, "salsa-spatial")?;
        Ok((
            Self {
                config,
                writer,
                ogg,
            },
            header,
        ))
    }

    pub fn config(&self) -> &SalsaSpatialStreamConfig {
        &self.config
    }

    /// Encode one tile and return the container bytes.
    pub fn write_frame(&mut self, frame: &SalsaSpatialFrame) -> Result<Vec<u8>> {
        let packets = self.writer.write_frame(frame)?;
        let mut out = Vec::new();
        for packet in packets {
            let packed = multistream::pack_owned(&packet.substreams)?;
            out.extend(self.ogg.packet(&packed, OPUS_FRAME_SAMPLES)?);
        }
        Ok(out)
    }

    /// Transport one tail packet without extending the programme granule.
    /// The container's final granule position performs the end trim.
    pub fn write_tail(&mut self, packet: &MultistreamPacket) -> Result<Vec<u8>> {
        let packed = multistream::pack_owned(&packet.substreams)?;
        self.ogg.tail_packet(&packed)
    }

    pub fn finish(&mut self) -> Vec<u8> {
        self.ogg.finish()
    }

    pub fn total_samples(&self) -> u64 {
        self.ogg.total_samples()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SalsaSpatialStreamConfig {
        SalsaSpatialStreamConfig::default()
    }

    fn elementary(tag: u8) -> Vec<u8> {
        // Code 0 packet with a CELT fullband 20 ms configuration.
        let mut packet = vec![(31 << 3) | 0];
        packet.extend_from_slice(&[tag; 7]);
        packet
    }

    fn packet(tag: u8) -> MultistreamPacket {
        MultistreamPacket::new(
            (0..6u8)
                .map(|index| elementary(tag.wrapping_add(index)))
                .collect(),
        )
    }

    fn frame(start_sample: u64, snapshot: bool) -> SalsaSpatialFrame {
        SalsaSpatialFrame {
            sample_rate: SAMPLE_RATE,
            start_sample,
            frames: TILE_PACKETS * OPUS_FRAME_SAMPLES,
            packets: (0..TILE_PACKETS as u8).map(packet).collect(),
            enhancement: EnhancementTile {
                start_sample,
                frames: TILE_PACKETS * OPUS_FRAME_SAMPLES,
                bytes: vec![1, 2, 3],
                snapshot,
            },
            objects: Vec::new(),
            events: Vec::new(),
            snapshot,
            parameters: None,
        }
    }

    #[test]
    fn writer_reader_round_trip() {
        let mut writer = MultistreamWriter::new(config()).unwrap();
        let mut reader = MultistreamReader::with_binding(config().codec);
        let mut parsed = Vec::new();
        for index in 0..3 {
            let frame = frame(index * 4800, index == 0);
            for packet in writer.write_frame(&frame).unwrap() {
                let packed = multistream::pack_owned(&packet.substreams).unwrap();
                parsed.extend(reader.push_packet_bytes(&packed).unwrap());
            }
        }
        parsed.extend(reader.flush().unwrap());
        assert_eq!(parsed.len(), 3);
        assert!(reader.config().is_some());
        for (index, frame) in parsed.iter().enumerate() {
            assert_eq!(frame.start_sample, index as u64 * 4800);
            assert_eq!(frame.enhancement.bytes, vec![1, 2, 3]);
        }
    }

    #[test]
    fn large_metadata_uses_opus_padding_capacity() {
        // 10 kB of enhancement bytes becomes ~2 kB per center packet, well past
        // the 1275-byte per-frame limit but still a valid Opus packet because
        // it is carried as padding.
        let mut writer = MultistreamWriter::new(config()).unwrap();
        let mut reader = MultistreamReader::with_binding(config().codec);
        let mut tile = frame(0, true);
        tile.enhancement.bytes = vec![7; 10_000];

        let packets = writer.write_frame(&tile).unwrap();
        assert!(
            packets
                .iter()
                .any(|packet| packet.substreams[3].len() > 1275)
        );
        let mut parsed = Vec::new();
        for packet in packets {
            let packed = multistream::pack_owned(&packet.substreams).unwrap();
            parsed.extend(reader.push_packet_bytes(&packed).unwrap());
        }
        parsed.extend(reader.flush().unwrap());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].enhancement.bytes, tile.enhancement.bytes);
    }

    #[test]
    fn ogg_round_trip_preserves_tiles() {
        let config = config();
        let (mut writer, mut bytes) = OggSalsaWriter::new(config.clone(), 1, 120).unwrap();
        let frames: Vec<_> = (0..2)
            .map(|index| frame(index * 4800, index == 0))
            .collect();
        for frame in &frames {
            bytes.extend(writer.write_frame(frame).unwrap());
        }
        bytes.extend(writer.finish());

        let mut parser = SalsaSpatialParser::new();
        let mut parsed = parser.push_bytes(&bytes).unwrap();
        parsed.extend(parser.flush().unwrap());
        assert_eq!(parsed.len(), frames.len());
        assert!(parser.config().is_some());
        let head = parser.ogg.head().unwrap();
        assert_eq!(head.channel_mapping, config.codec.channel_mapping);
    }

    #[test]
    fn invalid_stream_count_is_rejected() {
        let mut reader = MultistreamReader::with_binding(config().codec);
        let bad = MultistreamPacket::new(vec![vec![1]]);
        assert!(reader.push_packet(bad).is_err());
    }
}
