//! IAMF 1.1 framing for the salsa-spatial core and enhancement carriage.
//!
//! # Scope
//!
//! The stream is a single IAMF 1.1 IA sequence with one channel-based audio
//! element (a single 5.1.2 layer and five Opus substreams), one mix
//! presentation, and one temporal unit per 20 ms Opus packet group.  The
//! sequence header, codec config, audio element, mix presentation, temporal
//! delimiter and audio frame syntax follows the IAMF specification and the
//! AOMediaCodec `iamf-tools` reference grammar, so ordinary IAMF/Opus decoders
//! can decode the core without codec-specific changes.
//!
//! # Salsa-specific metadata
//!
//! The exact [`SalsaSpatialStreamConfig`] is carried once in a reserved IAMF
//! Metadata OBU.  Per-tile scene/parameter state is carried only in legal Opus
//! padding extensions on the center substream, following the experimental Opus
//! extension draft.  A normal Opus decoder ignores the padding and decodes the
//! same five substreams; the salsa parser recovers the extension before
//! handing the center packet to libopus.
//!
//! # Simplifications
//!
//! * No OBU trimming or extension headers are emitted.
//! * Mix gain values use Q7.8 dB as required; the headroom gain is also stored
//!   losslessly in the private salsa metadata.
//! * Loudness information in the mix presentation is written as zero values;
//!   the experiment does not perform loudness measurement.
//! * Reserved/unknown OBUs are ignored by the reader.
//!
//! The transport is fully self-delimiting and round-trips through
//! [`IamfReader`].

use std::ops::Range;

use objectaudio_ir::{ObjectEvent, ObjectState, Transition};
use salsa_bitstream::BitReader;

use crate::constants::{
    CORE_STREAM_COUNT, MAX_OBJECTS, OPUS_FRAME_SAMPLES, SAMPLE_RATE, TILE_PACKETS,
};
use crate::error::{Result, SpatialError};
use crate::frame::{CorePacket, EnhancementTile, SalsaSpatialFrame, SalsaSpatialStreamConfig};
use crate::opus_ext;
use crate::params::TrajectoryKnot;

const OBU_CODEC_CONFIG: u8 = 0;
const OBU_AUDIO_ELEMENT: u8 = 1;
const OBU_MIX_PRESENTATION: u8 = 2;
const OBU_PARAMETER_BLOCK: u8 = 3;
const OBU_TEMPORAL_DELIMITER: u8 = 4;
const OBU_AUDIO_FRAME: u8 = 5;
const OBU_AUDIO_FRAME_ID0: u8 = 6;
const OBU_AUDIO_FRAME_ID17: u8 = 23;
const OBU_METADATA: u8 = 24;
const OBU_SEQUENCE_HEADER: u8 = 31;

const SALSA_METADATA_TYPE: u64 = 3;
const SALSA_CONFIG_PARAM_TYPE: u64 = 10;
const SALSA_ENHANCEMENT_PARAM_TYPE: u64 = 11;
const SALSA_CONFIG_PARAM_ID: u64 = 0x5343_4647; // "SCFG"
const SALSA_ENHANCEMENT_PARAM_ID: u64 = 0x5341_4c45; // "SALE"
const SALSA_CONFIG_MAGIC: u32 = 0x5343_4647;
const SALSA_CONFIG_VERSION: u16 = 1;
const SALSA_PARAMETER_RATE: u64 = SAMPLE_RATE as u64;
const TILE_MAGIC: u32 = 0x5354_494c; // "STIL"
const TILE_VERSION: u16 = 1;

/// Whether the prefix starts with a complete IAMF sequence header.
///
/// This identifies the container. The reader validates its codec and scene
/// configuration separately. Reuse the OBU reader so header extensions and
/// variable-length sizes follow the same rules as decoding.
pub fn has_sequence_header(data: &[u8]) -> bool {
    matches!(
        probe_obu(data),
        Ok(Probe::Ready { obu_type: OBU_SEQUENCE_HEADER, payload, .. })
            if payload.len() >= 6 && data[payload.start..payload.end].starts_with(b"iamf")
    )
}

/// Writes one IAMF stream with a single channel-based 5.1.2 element.
#[derive(Debug, Default)]
pub struct IamfWriter {
    config: Option<SalsaSpatialStreamConfig>,
    buffer: Vec<u8>,
    finished: bool,
}

impl IamfWriter {
    /// Creates a writer and serializes the sequence-level OBUs immediately.
    pub fn new(config: &SalsaSpatialStreamConfig) -> Result<Self> {
        let header = build_header(config)?;
        Ok(Self {
            config: Some(config.clone()),
            buffer: header,
            finished: false,
        })
    }

    /// Writes the temporal unit(s) for one 100 ms (or shorter final) tile.
    ///
    /// The first call also returns the sequence-level OBUs built by
    /// [`IamfWriter::new`].  Concatenating the outputs of successive calls and
    /// [`IamfWriter::finish`] produces the complete stream.
    pub fn write_frame(&mut self, frame: &SalsaSpatialFrame) -> Result<Vec<u8>> {
        if self.finished {
            return Err(SpatialError::Iamf("writer already finished"));
        }
        if self.config.is_none() {
            return Err(SpatialError::Iamf("writer has no configuration"));
        }
        let bytes = encode_frame(frame)?;
        self.buffer.extend_from_slice(&bytes);
        Ok(std::mem::take(&mut self.buffer))
    }

    /// Finalizes the stream and returns any bytes not yet returned.
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        self.finished = true;
        Ok(std::mem::take(&mut self.buffer))
    }
}

/// Reads IAMF bytes emitted by [`IamfWriter`].
#[derive(Debug)]
pub struct IamfReader {
    pending: Vec<u8>,
    config: Option<SalsaSpatialStreamConfig>,
    sample_rate: u32,
    next_sample: u64,
    current_packets: Vec<CorePacket>,
    pending_packet: CorePacket,
    pending_seen: [bool; CORE_STREAM_COUNT],
}

impl Default for IamfReader {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            config: None,
            sample_rate: SAMPLE_RATE,
            next_sample: 0,
            current_packets: Vec::new(),
            pending_packet: CorePacket::default(),
            pending_seen: [false; CORE_STREAM_COUNT],
        }
    }
}

impl IamfReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn config(&self) -> Option<&SalsaSpatialStreamConfig> {
        self.config.as_ref()
    }

    /// Appends bytes and returns every complete tile that became available.
    pub fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<SalsaSpatialFrame>> {
        self.pending.extend_from_slice(data);
        let mut frames = Vec::new();
        self.process_available(&mut frames)?;
        Ok(frames)
    }

    /// Processes any remaining complete OBUs and returns the final partial tile.
    pub fn flush(&mut self) -> Result<Vec<SalsaSpatialFrame>> {
        let mut frames = Vec::new();
        self.process_available(&mut frames)?;
        if !self.pending.is_empty() {
            return Err(SpatialError::Truncated);
        }
        if self.pending_seen.iter().any(|seen| *seen) {
            return Err(SpatialError::Truncated);
        }
        if !self.current_packets.is_empty() {
            self.emit_tile(&mut frames)?;
        }
        Ok(frames)
    }

    fn process_available(&mut self, frames: &mut Vec<SalsaSpatialFrame>) -> Result<()> {
        while let Some((obu_type, payload)) = self.drain_one_obu()? {
            self.process_obu(obu_type, &payload, frames)?;
        }
        Ok(())
    }

    fn drain_one_obu(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        match probe_obu(&self.pending)? {
            Probe::NeedMore => Ok(None),
            Probe::Ready {
                obu_type,
                payload,
                next,
            } => {
                let payload = self.pending[payload].to_vec();
                self.pending.drain(..next);
                Ok(Some((obu_type, payload)))
            }
        }
    }

    fn process_obu(
        &mut self,
        obu_type: u8,
        payload: &[u8],
        frames: &mut Vec<SalsaSpatialFrame>,
    ) -> Result<()> {
        match obu_type {
            OBU_SEQUENCE_HEADER => self.parse_sequence_header(payload),
            OBU_CODEC_CONFIG => self.parse_codec_config(payload),
            OBU_AUDIO_ELEMENT => self.parse_audio_element(payload),
            OBU_PARAMETER_BLOCK => Ok(()),
            OBU_TEMPORAL_DELIMITER => self.parse_temporal_delimiter(),
            OBU_AUDIO_FRAME..=OBU_AUDIO_FRAME_ID17 => {
                self.parse_audio_frame(obu_type, payload, frames)
            }
            OBU_METADATA => self.parse_metadata(payload),
            OBU_MIX_PRESENTATION | 25..=30 => Ok(()),
            _ => Ok(()),
        }
    }

    fn parse_sequence_header(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 6 {
            return Err(SpatialError::Truncated);
        }
        if &payload[..4] != b"iamf" {
            return Err(SpatialError::Iamf("missing iamf sequence code"));
        }
        Ok(())
    }

    fn parse_metadata(&mut self, payload: &[u8]) -> Result<()> {
        let mut reader = R::new(payload);
        let metadata_type = reader.uleb()?;
        let size = reader.uleb()? as usize;
        let bytes = reader.bytes(size)?;
        if metadata_type == SALSA_METADATA_TYPE {
            self.config = Some(decode_config_payload(bytes)?);
        }
        Ok(())
    }

    fn parse_codec_config(&mut self, payload: &[u8]) -> Result<()> {
        let mut reader = R::new(payload);
        let _codec_config_id = reader.uleb()?;
        let codec_id = reader.u32()?;
        if codec_id != 0x4f70_7573 {
            return Err(SpatialError::Iamf("unsupported codec in stream"));
        }
        let _samples_per_frame = reader.uleb()?;
        let _audio_roll_distance = reader.i16()?;
        let _version = reader.u8()?;
        let _output_channel_count = reader.u8()?;
        let _pre_skip = reader.u16()?;
        let input_sample_rate = reader.u32()?;
        if input_sample_rate != 0 {
            self.sample_rate = input_sample_rate;
        }
        let _output_gain = reader.i16()?;
        let _mapping_family = reader.u8()?;
        Ok(())
    }

    fn parse_audio_element(&mut self, payload: &[u8]) -> Result<()> {
        let mut reader = BitReader::new(payload);
        let _audio_element_id = br_uleb(&mut reader)?;
        let element_type = reader.try_read(3)?;
        let _reserved = reader.try_read(5)?;
        if element_type != 0 {
            return Err(SpatialError::Iamf("audio element is not channel based"));
        }
        let _codec_config_id = br_uleb(&mut reader)?;
        let num_substreams = br_uleb(&mut reader)?;
        if num_substreams != CORE_STREAM_COUNT as u64 {
            return Err(SpatialError::Iamf("unexpected substream count"));
        }
        for _ in 0..num_substreams {
            let _substream_id = br_uleb(&mut reader)?;
        }

        let num_parameters = br_uleb(&mut reader)?;
        if num_parameters > 64 {
            return Err(SpatialError::Iamf("too many audio element parameters"));
        }
        for _ in 0..num_parameters {
            let parameter_type = br_uleb(&mut reader)?;
            match parameter_type {
                SALSA_CONFIG_PARAM_TYPE => {
                    let size = br_uleb(&mut reader)? as usize;
                    let bytes = read_bit_bytes(&mut reader, size)?;
                    self.config = Some(parse_config_definition(&bytes)?);
                }
                SALSA_ENHANCEMENT_PARAM_TYPE => {
                    let size = br_uleb(&mut reader)? as usize;
                    let bytes = read_bit_bytes(&mut reader, size)?;
                    parse_enhancement_definition(&bytes)?;
                }
                _ => {
                    return Err(SpatialError::Iamf(
                        "unsupported audio element parameter type",
                    ));
                }
            }
        }

        let num_layers = reader.try_read(3)?;
        let _layout_reserved = reader.try_read(5)?;
        if num_layers != 1 {
            return Err(SpatialError::Iamf("expected one channel layer"));
        }
        let loudspeaker_layout = reader.try_read(4)?;
        let output_gain_is_present = reader.try_read(1)? != 0;
        let _recon_gain_is_present = reader.try_read(1)?;
        let _layer_reserved = reader.try_read(2)?;
        let substream_count = reader.try_read(8)?;
        let coupled_substream_count = reader.try_read(8)?;
        if loudspeaker_layout != 3 || substream_count != CORE_STREAM_COUNT as u32 {
            return Err(SpatialError::Iamf("expected a single 5.1.2 layer"));
        }
        if coupled_substream_count != 3 {
            return Err(SpatialError::Iamf("unexpected coupled substream count"));
        }
        if output_gain_is_present {
            let _gain_flag = reader.try_read(6)?;
            let _gain_reserved = reader.try_read(2)?;
            let _output_gain = reader.try_read(16)?;
        }
        if loudspeaker_layout == 15 {
            let _expanded = reader.try_read(8)?;
        }
        Ok(())
    }

    fn parse_temporal_delimiter(&mut self) -> Result<()> {
        if self.pending_seen.iter().any(|seen| *seen) {
            return Err(SpatialError::Iamf(
                "incomplete packet group before temporal delimiter",
            ));
        }
        Ok(())
    }

    fn parse_audio_frame(
        &mut self,
        obu_type: u8,
        payload: &[u8],
        frames: &mut Vec<SalsaSpatialFrame>,
    ) -> Result<()> {
        let (substream_id, packet) = if obu_type == OBU_AUDIO_FRAME {
            let mut reader = R::new(payload);
            let id = reader.uleb()? as usize;
            (id, &payload[reader.position..])
        } else {
            ((obu_type - OBU_AUDIO_FRAME_ID0) as usize, payload)
        };
        if substream_id >= CORE_STREAM_COUNT {
            return Err(SpatialError::Iamf("audio frame substream out of range"));
        }
        if self.pending_seen[substream_id] {
            return Err(SpatialError::Iamf("duplicate audio frame substream"));
        }
        match substream_id {
            0 => self.pending_packet.front = packet.to_vec(),
            1 => self.pending_packet.surround = packet.to_vec(),
            2 => self.pending_packet.height = packet.to_vec(),
            3 => self.pending_packet.center = packet.to_vec(),
            4 => self.pending_packet.lfe = packet.to_vec(),
            _ => unreachable!(),
        }
        self.pending_seen[substream_id] = true;
        if self.pending_seen.iter().all(|seen| *seen) {
            let packet = std::mem::take(&mut self.pending_packet);
            self.pending_seen = [false; CORE_STREAM_COUNT];
            self.current_packets.push(packet);
            self.maybe_emit(frames)?;
        }
        Ok(())
    }

    fn maybe_emit(&mut self, frames: &mut Vec<SalsaSpatialFrame>) -> Result<()> {
        if self.current_packets.len() >= TILE_PACKETS {
            self.emit_tile(frames)?;
        }
        Ok(())
    }

    fn emit_tile(&mut self, frames: &mut Vec<SalsaSpatialFrame>) -> Result<()> {
        let mut packets = std::mem::take(&mut self.current_packets);
        if packets.is_empty() {
            return Ok(());
        }
        let tile_payload = strip_center_extensions(&mut packets);
        let fallback_frames = packets.len() * OPUS_FRAME_SAMPLES;
        let sample_rate = self.sample_rate;
        let start_sample;
        let frame_count;
        let enhancement;
        let objects;
        let events;
        let snapshot;
        match tile_payload {
            Some(bytes) => {
                let params = parse_tile_payload(&bytes)?;
                start_sample = params.frame_start_sample;
                frame_count = if params.frame_frames > 0 {
                    params.frame_frames as usize
                } else {
                    fallback_frames
                };
                enhancement = params.enhancement;
                objects = params.objects;
                events = params.events;
                snapshot = params.snapshot;
            }
            None => {
                start_sample = self.next_sample;
                frame_count = fallback_frames;
                enhancement = EnhancementTile {
                    start_sample,
                    frames: fallback_frames,
                    bytes: Vec::new(),
                    snapshot: false,
                };
                objects = Vec::new();
                events = Vec::new();
                snapshot = false;
            }
        }
        self.next_sample = start_sample.saturating_add(frame_count as u64);
        frames.push(SalsaSpatialFrame {
            sample_rate,
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

struct TileParams {
    frame_start_sample: u64,
    frame_frames: u32,
    enhancement: EnhancementTile,
    objects: Vec<ObjectState>,
    events: Vec<ObjectEvent>,
    snapshot: bool,
}

fn build_header(config: &SalsaSpatialStreamConfig) -> Result<Vec<u8>> {
    if !config.headroom_gain.is_finite() || config.headroom_gain <= 0.0 {
        return Err(SpatialError::InvalidValue("headroom gain must be positive"));
    }
    if config.object_count > MAX_OBJECTS {
        return Err(SpatialError::TooManyObjects(config.object_count));
    }

    let mut out = Vec::new();
    out.extend_from_slice(&write_obu(OBU_SEQUENCE_HEADER, &encode_sequence_header()));
    out.extend_from_slice(&write_obu(OBU_CODEC_CONFIG, &encode_codec_config()));
    out.extend_from_slice(&write_obu(
        OBU_AUDIO_ELEMENT,
        &encode_audio_element(config)?,
    ));
    out.extend_from_slice(&write_obu(
        OBU_MIX_PRESENTATION,
        &encode_mix_presentation(config),
    ));
    // The salsa configuration is private metadata. Standard IAMF decoders
    // ignore reserved metadata types, so the ordinary Opus core remains
    // decodable without any codec-specific changes.
    out.extend_from_slice(&write_obu(OBU_METADATA, &encode_metadata(config)));
    Ok(out)
}

fn encode_metadata(config: &SalsaSpatialStreamConfig) -> Vec<u8> {
    let payload = encode_config_payload(config);
    let mut writer = W::new();
    writer.uleb(SALSA_METADATA_TYPE);
    writer.uleb(payload.len() as u64);
    writer.bytes(&payload);
    writer.out
}

fn encode_sequence_header() -> Vec<u8> {
    let mut writer = W::new();
    writer.u32(0x6961_6d66); // "iamf"
    writer.u8(1); // primary profile: base
    writer.u8(1); // additional profile: base
    writer.out
}

fn encode_codec_config() -> Vec<u8> {
    let mut writer = W::new();
    writer.uleb(0); // codec_config_id
    writer.u32(0x4f70_7573); // "Opus"
    writer.uleb(OPUS_FRAME_SAMPLES as u64);
    // R = ceil(3840 / num_samples_per_frame); audio_roll_distance = -R.
    writer.i16(-4);
    writer.u8(1); // Opus version
    writer.u8(2); // IAMF default output_channel_count
    writer.u16(120); // pre-skip (the encoder's restricted-low-delay lookahead)
    writer.u32(SAMPLE_RATE);
    writer.i16(0); // output_gain
    writer.u8(0); // mapping_family 0
    writer.out
}

fn encode_audio_element(_config: &SalsaSpatialStreamConfig) -> Result<Vec<u8>> {
    let mut writer = W::new();
    writer.uleb(0); // audio_element_id
    // audio_element_type = channel_based (3 bits, MSB first) + 5 reserved bits.
    writer.u8(0);
    writer.uleb(0); // codec_config_id
    writer.uleb(CORE_STREAM_COUNT as u64);
    for substream_id in 0..CORE_STREAM_COUNT {
        writer.uleb(substream_id as u64);
    }
    writer.uleb(0); // num_parameters: the core is standard channel-based IAMF

    // One scalable channel layer: 5.1.2, five substreams, three coupled.
    writer.u8(0x20); // num_layers=1 (3 bits, MSB first) + reserved (5 bits)
    writer.u8(0x30); // layout=3 (4 bits; 5.1.2) + flags (2) + reserved (2)
    writer.u8(CORE_STREAM_COUNT as u8); // substream_count
    writer.u8(3); // coupled_substream_count
    Ok(writer.out)
}

fn encode_mix_presentation(_config: &SalsaSpatialStreamConfig) -> Vec<u8> {
    let mut writer = W::new();
    writer.uleb(0); // mix_presentation_id
    writer.uleb(0); // count_label: no localized annotations
    writer.uleb(1); // num_sub_mixes
    writer.uleb(1); // num_audio_elements in the sub-mix
    writer.uleb(0); // audio_element_id
    // rendering_config: headphones stereo (2), no element gain offset (1),
    // binaural filter profile ambient (2), reserved (3).
    writer.u8(0x00);
    writer.uleb(0); // rendering_config_extension_size
    write_mix_gain(&mut writer, 0, 0.0); // element mix gain
    write_mix_gain(&mut writer, 1, 0.0); // sub-mix output gain
    writer.uleb(1); // num_layouts: stereo loudness layout
    write_loudness_layout(&mut writer, 0); // sound system A (stereo)
    writer.out
}

fn write_mix_gain(writer: &mut W, parameter_id: u64, gain_db: f64) {
    writer.uleb(parameter_id);
    writer.uleb(SALSA_PARAMETER_RATE);
    // param_definition_mode = 1 (schedule in the parameter block; no schedule
    // is sent, so the default gain applies), reserved = 0.
    writer.u8(0x80);
    let q = (gain_db * 256.0).round();
    writer.i16(q.clamp(i16::MIN as f64, i16::MAX as f64) as i16);
}

fn write_loudness_layout(writer: &mut W, sound_system: u8) {
    // layout_type = 2 (loudspeakers SS convention), 4-bit sound system,
    // 2 reserved bits, all MSB first.
    writer.u8(0x80 | (sound_system << 2));
    writer.u8(0); // loudness info_type
    writer.i16(0); // integrated loudness (Q7.8 dB)
    writer.i16(0); // digital peak (Q7.8 dB)
}

fn encode_frame(frame: &SalsaSpatialFrame) -> Result<Vec<u8>> {
    let packet_count = frame.packets.len();
    let payload = encode_tile_payload(frame);
    let fragments = split_payload(&payload, packet_count);
    let mut out = Vec::new();

    if packet_count == 0 {
        out.extend_from_slice(&write_obu(OBU_TEMPORAL_DELIMITER, &[]));
        return Ok(out);
    }

    for (index, packet) in frame.packets.iter().enumerate() {
        out.extend_from_slice(&write_obu(OBU_TEMPORAL_DELIMITER, &[]));
        for substream_id in 0..CORE_STREAM_COUNT {
            let data = if substream_id == 3 && !fragments[index].is_empty() {
                opus_ext::append_extension(&packet.center, &fragments[index])?
            } else {
                packet.stream(substream_id).unwrap_or_default().to_vec()
            };
            out.extend_from_slice(&write_obu(OBU_AUDIO_FRAME_ID0 + substream_id as u8, &data));
        }
    }
    Ok(out)
}

fn encode_tile_payload(frame: &SalsaSpatialFrame) -> Vec<u8> {
    let mut writer = W::new();
    writer.u32(TILE_MAGIC);
    writer.u16(TILE_VERSION);
    writer.u64(frame.start_sample);
    writer.u32(frame.frames as u32);
    let mut flags = 0u8;
    if frame.enhancement.snapshot {
        flags |= 1;
    }
    if frame.snapshot {
        flags |= 2;
    }
    writer.u8(flags);
    writer.u64(frame.enhancement.start_sample);
    writer.u32(frame.enhancement.frames as u32);
    writer.u32(frame.enhancement.bytes.len() as u32);
    writer.bytes(&frame.enhancement.bytes);

    let include_scene = frame.snapshot || frame.start_sample == 0;
    if include_scene {
        writer.u32(frame.objects.len() as u32);
        for object in &frame.objects {
            encode_object(&mut writer, object);
        }
        writer.u32(frame.events.len() as u32);
        for event in &frame.events {
            writer.i64(event.offset);
            match event.transition {
                Transition::Step => writer.u8(0),
                Transition::Position(samples) => {
                    writer.u8(1);
                    writer.u32(samples);
                }
                Transition::All(samples) => {
                    writer.u8(2);
                    writer.u32(samples);
                }
            }
            encode_object(&mut writer, &event.state);
        }
    } else {
        writer.u32(0);
        writer.u32(0);
    }
    writer.out
}

fn parse_tile_payload(bytes: &[u8]) -> Result<TileParams> {
    let mut reader = R::new(bytes);
    if reader.u32()? != TILE_MAGIC {
        return Err(SpatialError::Iamf("bad salsa tile magic"));
    }
    let version = reader.u16()?;
    if version != TILE_VERSION {
        return Err(SpatialError::UnsupportedVersion { version });
    }
    let frame_start_sample = reader.u64()?;
    let frame_frames = reader.u32()?;
    let flags = reader.u8()?;
    let enhancement_start_sample = reader.u64()?;
    let enhancement_frames = reader.u32()?;
    let bytes_len = reader.u32()? as usize;
    let enhancement_bytes = reader.bytes(bytes_len)?.to_vec();
    let object_count = bounded_count(&mut reader, 1)?;
    let mut objects = Vec::with_capacity(object_count);
    for _ in 0..object_count {
        objects.push(decode_object(&mut reader)?);
    }
    let event_count = bounded_count(&mut reader, 16)?;
    let mut events = Vec::with_capacity(event_count);
    for _ in 0..event_count {
        let offset = reader.i64()?;
        let transition = match reader.u8()? {
            0 => Transition::Step,
            1 => Transition::Position(reader.u32()?),
            2 => Transition::All(reader.u32()?),
            _ => return Err(SpatialError::Iamf("invalid scene transition")),
        };
        let state = decode_object(&mut reader)?;
        events.push(ObjectEvent {
            offset,
            transition,
            state,
        });
    }
    Ok(TileParams {
        frame_start_sample,
        frame_frames,
        enhancement: EnhancementTile {
            start_sample: enhancement_start_sample,
            frames: enhancement_frames as usize,
            bytes: enhancement_bytes,
            snapshot: flags & 1 != 0,
        },
        objects,
        events,
        snapshot: flags & 2 != 0,
    })
}

fn split_payload(bytes: &[u8], parts: usize) -> Vec<Vec<u8>> {
    if parts == 0 {
        return Vec::new();
    }
    if bytes.is_empty() {
        return vec![Vec::new(); parts];
    }
    let mut fragments = Vec::with_capacity(parts);
    for index in 0..parts {
        let start = index * bytes.len() / parts;
        let end = (index + 1) * bytes.len() / parts;
        let mut fragment = Vec::with_capacity(6 + end - start);
        fragment.push(index as u8);
        fragment.push(parts as u8);
        fragment.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        fragment.extend_from_slice(&bytes[start..end]);
        fragments.push(fragment);
    }
    fragments
}

/// Removes salsa padding extensions from center packets and reassembles the
/// enhancement bytes when possible.  The returned packets are the original
/// core packets, so a parser consumer sees the same packet bytes the writer was
/// given while a core decoder's later `strip_extension` call is a harmless
/// no-op.
fn strip_center_extensions(packets: &mut [CorePacket]) -> Option<Vec<u8>> {
    let mut parts: Vec<(u8, u8, usize, Vec<u8>)> = Vec::new();
    for packet in packets.iter_mut() {
        let Ok((stripped, payload)) = opus_ext::strip_extension(&packet.center) else {
            continue;
        };
        packet.center = stripped;
        let Some(payload) = payload else {
            continue;
        };
        if payload.len() < 6 {
            continue;
        }
        let part = payload[0];
        let total = payload[1];
        let total_len =
            u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]) as usize;
        parts.push((part, total, total_len, payload[6..].to_vec()));
    }
    if parts.is_empty() {
        return None;
    }
    let total = parts[0].1;
    let total_len = parts[0].2;
    if total as usize != parts.len() {
        return None;
    }
    parts.sort_by_key(|part| part.0);
    let mut bytes = Vec::with_capacity(total_len);
    for (index, (part, part_total, part_len, data)) in parts.into_iter().enumerate() {
        if part as usize != index || part_total != total || part_len != total_len {
            return None;
        }
        bytes.extend_from_slice(&data);
    }
    bytes.truncate(total_len);
    Some(bytes)
}

fn write_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + payload.len());
    out.push(obu_type << 3);
    write_uleb(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out
}

fn write_uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn br_uleb(reader: &mut BitReader<'_>) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for _ in 0..5 {
        let byte = reader.try_read(8).map_err(|_| SpatialError::Truncated)? as u64;
        value |= (byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(SpatialError::Iamf("uleb128 overflow"))
}

fn read_bit_bytes(reader: &mut BitReader<'_>, count: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(reader.try_read(8).map_err(|_| SpatialError::Truncated)? as u8);
    }
    Ok(out)
}

fn parse_config_definition(bytes: &[u8]) -> Result<SalsaSpatialStreamConfig> {
    let mut reader = R::new(bytes);
    let parameter_id = reader.uleb()?;
    if parameter_id != SALSA_CONFIG_PARAM_ID {
        return Err(SpatialError::Iamf("unexpected salsa config parameter id"));
    }
    let _parameter_rate = reader.uleb()?;
    decode_config_payload(reader.rest())
}

fn parse_enhancement_definition(bytes: &[u8]) -> Result<()> {
    let mut reader = R::new(bytes);
    let parameter_id = reader.uleb()?;
    if parameter_id != SALSA_ENHANCEMENT_PARAM_ID {
        return Err(SpatialError::Iamf(
            "unexpected salsa enhancement parameter id",
        ));
    }
    let _parameter_rate = reader.uleb()?;
    Ok(())
}

fn encode_config_payload(config: &SalsaSpatialStreamConfig) -> Vec<u8> {
    let mut writer = W::new();
    writer.u32(SALSA_CONFIG_MAGIC);
    writer.u16(SALSA_CONFIG_VERSION);
    writer.u32(config.object_count as u32);
    writer.f64(config.headroom_gain);
    for bitrate in config.core_bitrates {
        writer.u32(bitrate);
    }
    writer.f64(config.beta);
    writer.u32(config.initial_objects.len() as u32);
    for object in &config.initial_objects {
        encode_object(&mut writer, object);
    }
    writer.u32(config.initial_trajectories.len() as u32);
    for trajectory in &config.initial_trajectories {
        writer.u32(trajectory.len() as u32);
        for knot in trajectory {
            writer.u64(knot.sample);
            for coordinate in knot.position {
                writer.f64(coordinate);
            }
        }
    }
    writer.out
}

fn decode_config_payload(bytes: &[u8]) -> Result<SalsaSpatialStreamConfig> {
    let mut reader = R::new(bytes);
    if reader.u32()? != SALSA_CONFIG_MAGIC {
        return Err(SpatialError::Iamf("bad salsa config magic"));
    }
    let version = reader.u16()?;
    if version != SALSA_CONFIG_VERSION {
        return Err(SpatialError::UnsupportedVersion { version });
    }
    let object_count = reader.u32()? as usize;
    if object_count > MAX_OBJECTS {
        return Err(SpatialError::TooManyObjects(object_count));
    }
    let headroom_gain = reader.f64()?;
    let mut core_bitrates = [0u32; 5];
    for bitrate in &mut core_bitrates {
        *bitrate = reader.u32()?;
    }
    let beta = reader.f64()?;
    let initial_objects_count = bounded_count(&mut reader, 1)?;
    let mut initial_objects = Vec::with_capacity(initial_objects_count);
    for _ in 0..initial_objects_count {
        initial_objects.push(decode_object(&mut reader)?);
    }
    let trajectory_count = bounded_count(&mut reader, 4)?;
    let mut initial_trajectories = Vec::with_capacity(trajectory_count);
    for _ in 0..trajectory_count {
        let knot_count = bounded_count(&mut reader, 32)?;
        let mut trajectory = Vec::with_capacity(knot_count);
        for _ in 0..knot_count {
            let sample = reader.u64()?;
            let position = [reader.f64()?, reader.f64()?, reader.f64()?];
            trajectory.push(TrajectoryKnot { sample, position });
        }
        initial_trajectories.push(trajectory);
    }
    Ok(SalsaSpatialStreamConfig {
        object_count,
        headroom_gain,
        initial_objects,
        initial_trajectories,
        core_bitrates,
        beta,
    })
}

fn encode_object(writer: &mut W, object: &ObjectState) {
    writer.u32(object.id);
    for coordinate in object.position {
        writer.f32(coordinate);
    }
    writer.f32(object.gain);
    writer.u8(object.enabled as u8);
    match object.size {
        Some(size) => {
            writer.u8(1);
            writer.f32(size);
        }
        None => writer.u8(0),
    }
    match object.spread {
        Some(spread) => {
            writer.u8(1);
            writer.f32(spread);
        }
        None => writer.u8(0),
    }
}

fn decode_object(reader: &mut R) -> Result<ObjectState> {
    let id = reader.u32()?;
    let position = [reader.f32()?, reader.f32()?, reader.f32()?];
    let gain = reader.f32()?;
    let enabled = reader.u8()? != 0;
    let size = if reader.u8()? != 0 {
        Some(reader.f32()?)
    } else {
        None
    };
    let spread = if reader.u8()? != 0 {
        Some(reader.f32()?)
    } else {
        None
    };
    Ok(ObjectState {
        id,
        position,
        gain,
        enabled,
        size,
        spread,
    })
}

fn bounded_count(reader: &mut R<'_>, minimum_size: usize) -> Result<usize> {
    let count = reader.u32()? as usize;
    if count > MAX_OBJECTS * 1024 || count > reader.remaining() / minimum_size {
        return Err(SpatialError::Truncated);
    }
    Ok(count)
}

enum Probe {
    NeedMore,
    Ready {
        obu_type: u8,
        payload: Range<usize>,
        next: usize,
    },
}

fn probe_obu(data: &[u8]) -> Result<Probe> {
    if data.is_empty() {
        return Ok(Probe::NeedMore);
    }
    let first = data[0];
    let obu_type = first >> 3;
    let type_specific = first & 0x02 != 0;
    let extension_flag = first & 0x01 != 0;

    let mut position = 1usize;
    let Some(obu_size) = probe_uleb(data, &mut position)? else {
        return Ok(Probe::NeedMore);
    };
    let size_start = position;

    if type_specific && is_audio_frame_type(obu_type) {
        let Some(_) = probe_uleb(data, &mut position)? else {
            return Ok(Probe::NeedMore);
        };
        let Some(_) = probe_uleb(data, &mut position)? else {
            return Ok(Probe::NeedMore);
        };
    }
    if extension_flag {
        let Some(extension_size) = probe_uleb(data, &mut position)? else {
            return Ok(Probe::NeedMore);
        };
        let extension_size = usize::try_from(extension_size)
            .map_err(|_| SpatialError::Iamf("extension header too large"))?;
        let end = position
            .checked_add(extension_size)
            .ok_or(SpatialError::Iamf("extension header overflow"))?;
        if end > data.len() {
            return Ok(Probe::NeedMore);
        }
        position = end;
    }

    let fields_len = position - size_start;
    if obu_size < fields_len as u64 {
        return Err(SpatialError::Iamf("OBU size smaller than its header"));
    }
    let payload_size = usize::try_from(obu_size - fields_len as u64)
        .map_err(|_| SpatialError::Iamf("OBU payload too large"))?;
    let payload_end = position
        .checked_add(payload_size)
        .ok_or(SpatialError::Iamf("OBU size overflow"))?;
    if payload_end > data.len() {
        return Ok(Probe::NeedMore);
    }
    Ok(Probe::Ready {
        obu_type,
        payload: position..payload_end,
        next: payload_end,
    })
}

fn probe_uleb(data: &[u8], position: &mut usize) -> Result<Option<u64>> {
    let start = *position;
    let mut value = 0u64;
    let mut shift = 0u32;
    for _ in 0..5 {
        let Some(byte) = data.get(*position).copied() else {
            *position = start;
            return Ok(None);
        };
        *position += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(Some(value));
        }
        shift += 7;
    }
    *position = start;
    Err(SpatialError::Iamf("uleb128 overflow in OBU header"))
}

fn is_audio_frame_type(obu_type: u8) -> bool {
    (OBU_AUDIO_FRAME_ID0..=OBU_AUDIO_FRAME_ID17).contains(&obu_type) || obu_type == OBU_AUDIO_FRAME
}

struct W {
    out: Vec<u8>,
}

impl W {
    fn new() -> Self {
        Self { out: Vec::new() }
    }

    fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn i16(&mut self, value: i16) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn f32(&mut self, value: f32) {
        self.u32(value.to_bits());
    }

    fn f64(&mut self, value: f64) {
        self.u64(value.to_bits());
    }

    fn uleb(&mut self, value: u64) {
        write_uleb(&mut self.out, value);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.out.extend_from_slice(value);
    }
}

struct R<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> R<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }

    fn rest(&self) -> &'a [u8] {
        &self.data[self.position..]
    }

    fn u8(&mut self) -> Result<u8> {
        let value = *self
            .data
            .get(self.position)
            .ok_or(SpatialError::Truncated)?;
        self.position += 1;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.bytes(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn uleb(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        for _ in 0..5 {
            let byte = self.u8()?;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
        Err(SpatialError::Iamf("uleb128 overflow"))
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
