//! Versioned, length-delimited Salsa records carried in the center substream.
//!
//! Records travel inside the experimental Opus padding extension on the mono
//! center packet. Fragment records add an unambiguous configuration id, record
//! type, tile start sample, fragment index/count and total length before any
//! byte is accepted. Unknown optional records can be skipped by length.

use objectaudio_ir::{ObjectEvent, ObjectState, Transition};

use crate::constants::{MAX_OBJECTS, MAX_RECORD_BYTES, MAX_RECORD_FRAGMENTS};
use crate::descriptor::{
    BaseFold, CodecBinding, InputDomain, ModelAlgorithm, ModelRow, ObservationGroup, Projection,
    ReconstructionStage, SignalModel,
};
use crate::error::{Result, SpatialError};
use crate::frame::{EnhancementTile, SalsaSpatialStreamConfig};
use crate::params::TrajectoryKnot;

/// Magic for the outer fragment wrapper.
pub const RECORD_MAGIC: u32 = 0x534C_5352; // "SLSR"
/// Version of the record schema.
pub const RECORD_VERSION: u16 = 1;
/// Magic for a typed record payload header.
pub const RECORD_ENTRY_MAGIC: u32 = 0x534C_454E; // "SLEN"
/// Configuration payload magic.
pub const CONFIG_MAGIC: u32 = 0x5343_4647; // "SCFG"
/// Tile payload magic.
pub const TILE_MAGIC: u32 = 0x5354_494C; // "STIL"

pub const RECORD_CONFIG: u8 = 1;
pub const RECORD_TILE: u8 = 2;
pub const RECORD_GROUP_ACTIVITY: u8 = 3;

/// One typed record inside a reassembled bundle.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Configuration(SalsaSpatialStreamConfig),
    Tile(TileRecord),
    GroupActivity(GroupActivity),
}

/// One 100 ms tile record.
#[derive(Debug, Clone, PartialEq)]
pub struct TileRecord {
    pub start_sample: u64,
    pub frames: u32,
    pub snapshot: bool,
    pub enhancement: EnhancementTile,
    pub objects: Vec<ObjectState>,
    pub events: Vec<ObjectEvent>,
}

/// Per-tile activity within fixed group capacity.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GroupActivity {
    pub start_sample: u64,
    pub active_groups: Vec<u32>,
    pub active_rank: u32,
}

/// Serialize one typed record.
pub fn write_record(record: &Record) -> Result<Vec<u8>> {
    let (tag, flags, payload) = match record {
        Record::Configuration(config) => {
            config.validate()?;
            (RECORD_CONFIG, 0u8, encode_configuration(config)?)
        }
        Record::Tile(tile) => (
            RECORD_TILE,
            if tile.snapshot { 1 } else { 0 },
            encode_tile(tile)?,
        ),
        Record::GroupActivity(activity) => {
            let mut writer = Writer::new();
            writer.u64(activity.start_sample);
            writer.ulen(activity.active_groups.len() as u32)?;
            for group in &activity.active_groups {
                writer.u32(*group);
            }
            writer.u32(activity.active_rank);
            (RECORD_GROUP_ACTIVITY, 0, writer.out)
        }
    };
    let mut writer = Writer::new();
    writer.u32(RECORD_ENTRY_MAGIC);
    writer.u16(RECORD_VERSION);
    writer.u8(tag);
    writer.u8(flags);
    writer.u32(payload.len() as u32);
    writer.bytes(&payload);
    Ok(writer.out)
}

/// Parse every typed record in a reassembled bundle.
pub fn parse_records(bytes: &[u8]) -> Result<Vec<Record>> {
    let mut reader = Reader::new(bytes);
    let mut out = Vec::new();
    while reader.remaining() > 0 {
        if reader.remaining() < 12 {
            return Err(SpatialError::Truncated);
        }
        if reader.u32()? != RECORD_ENTRY_MAGIC {
            return Err(SpatialError::Syntax("bad record magic"));
        }
        let version = reader.u16()?;
        if version != RECORD_VERSION {
            return Err(SpatialError::UnsupportedVersion { version });
        }
        let tag = reader.u8()?;
        let flags = reader.u8()?;
        let length = reader.u32()? as usize;
        if length > MAX_RECORD_BYTES {
            return Err(SpatialError::InvalidValue("record payload is too large"));
        }
        let payload = reader.bytes(length)?;
        let record = match tag {
            RECORD_CONFIG => Record::Configuration(decode_configuration(payload)?),
            RECORD_TILE => Record::Tile(decode_tile(payload, flags & 1 != 0)?),
            RECORD_GROUP_ACTIVITY => {
                let mut reader = Reader::new(payload);
                let start_sample = reader.u64()?;
                let count = reader.len()?;
                if count > 256 {
                    return Err(SpatialError::InvalidValue("too many active groups"));
                }
                let mut active_groups = Vec::with_capacity(count);
                for _ in 0..count {
                    active_groups.push(reader.u32()?);
                }
                let active_rank = reader.u32()?;
                Record::GroupActivity(GroupActivity {
                    start_sample,
                    active_groups,
                    active_rank,
                })
            }
            // Unknown optional record types are skipped by length.
            _ => continue,
        };
        out.push(record);
    }
    Ok(out)
}

/// Serialize the complete configuration record payload.
pub fn encode_configuration(config: &SalsaSpatialStreamConfig) -> Result<Vec<u8>> {
    config.validate()?;
    let mut writer = Writer::new();
    writer.u32(CONFIG_MAGIC);
    writer.u16(config.format_version);
    writer.u8(config.profile);
    writer.u32(config.config_id);
    writer.u32(config.object_count as u32);
    writer.f64(config.headroom_gain);
    for bitrate in config.core_bitrates {
        writer.u32(bitrate);
    }
    writer.f64(config.beta);
    encode_binding(&mut writer, &config.codec);
    writer.u32(config.signal_models.len() as u32);
    for model in &config.signal_models {
        writer.u32(model.model_ref);
        writer.u8(model.algorithm.tag());
        writer.u16(model.algorithm_version);
    }
    writer.u32(config.observation_groups.len() as u32);
    for group in &config.observation_groups {
        writer.u32(group.group_id);
        writer.u32(group.logical_channels.len() as u32);
        for logical in &group.logical_channels {
            writer.u32(*logical as u32);
        }
        match group.input {
            InputDomain::SceneObjects => writer.u8(1),
            InputDomain::ObjectResidual { stage_id } => {
                writer.u8(2);
                writer.u8(stage_id);
            }
        }
        match &group.projection {
            Projection::ModelRows { model_ref, rows } => {
                writer.u8(1);
                writer.u32(*model_ref);
                writer.u32(rows.len() as u32);
                for row in rows {
                    writer.u8(row.tag());
                }
            }
        }
    }
    match &config.base_fold {
        Some(fold) => {
            writer.u8(1);
            writer.u8(fold.preset);
            for row in fold.matrix {
                for value in row {
                    writer.f64(value);
                }
            }
        }
        None => writer.u8(0),
    }
    writer.u32(config.reconstruction.len() as u32);
    for stage in &config.reconstruction {
        writer.u8(stage.stage_id);
        writer.u32(stage.base_logical.len() as u32);
        for logical in &stage.base_logical {
            writer.u32(*logical as u32);
        }
        writer.u32(stage.required_groups.len() as u32);
        for group in &stage.required_groups {
            writer.u32(*group);
        }
        writer.u32(stage.observation_logical.len() as u32);
        for logical in &stage.observation_logical {
            writer.u32(*logical as u32);
        }
    }
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
    Ok(writer.out)
}

/// Parse a complete configuration record payload.
pub fn decode_configuration(bytes: &[u8]) -> Result<SalsaSpatialStreamConfig> {
    let mut reader = Reader::new(bytes);
    if reader.u32()? != CONFIG_MAGIC {
        return Err(SpatialError::Syntax("bad configuration magic"));
    }
    let format_version = reader.u16()?;
    if format_version != 1 {
        return Err(SpatialError::UnsupportedVersion {
            version: format_version,
        });
    }
    let profile = reader.u8()?;
    let config_id = reader.u32()?;
    let object_count = reader.u32()? as usize;
    if object_count > MAX_OBJECTS {
        return Err(SpatialError::TooManyObjects(object_count));
    }
    let headroom_gain = reader.f64()?;
    if !headroom_gain.is_finite() || headroom_gain <= 0.0 {
        return Err(SpatialError::InvalidValue("headroom gain must be positive"));
    }
    let mut core_bitrates = [0u32; 5];
    for bitrate in &mut core_bitrates {
        *bitrate = reader.u32()?;
    }
    let beta = reader.f64()?;
    let codec = decode_binding(&mut reader)?;
    let model_count = reader.bounded_count(1)?;
    let mut signal_models = Vec::with_capacity(model_count);
    for _ in 0..model_count {
        let model_ref = reader.u32()?;
        let tag = reader.u8()?;
        let algorithm = ModelAlgorithm::from_tag(tag)
            .ok_or(SpatialError::InvalidValue("unsupported model algorithm"))?;
        let algorithm_version = reader.u16()?;
        signal_models.push(SignalModel {
            model_ref,
            algorithm,
            algorithm_version,
        });
    }
    let group_count = reader.bounded_count(1)?;
    let mut observation_groups = Vec::with_capacity(group_count);
    for _ in 0..group_count {
        let group_id = reader.u32()?;
        let channel_count = reader.bounded_count(1)?;
        let mut logical_channels = Vec::with_capacity(channel_count);
        for _ in 0..channel_count {
            logical_channels.push(reader.u32()? as usize);
        }
        let input = match reader.u8()? {
            1 => InputDomain::SceneObjects,
            2 => InputDomain::ObjectResidual {
                stage_id: reader.u8()?,
            },
            _ => return Err(SpatialError::InvalidValue("unsupported input domain")),
        };
        let projection = match reader.u8()? {
            1 => {
                let model_ref = reader.u32()?;
                let row_count = reader.bounded_count(1)?;
                let mut rows = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    let tag = reader.u8()?;
                    rows.push(
                        ModelRow::from_tag(tag)
                            .ok_or(SpatialError::InvalidValue("unsupported model row"))?,
                    );
                }
                Projection::ModelRows { model_ref, rows }
            }
            _ => return Err(SpatialError::InvalidValue("unsupported projection")),
        };
        observation_groups.push(ObservationGroup {
            group_id,
            logical_channels,
            input,
            projection,
        });
    }
    let base_fold = if reader.u8()? != 0 {
        let preset = reader.u8()?;
        let mut matrix = [[0.0f64; 2]; 5];
        for row in &mut matrix {
            for value in row {
                *value = reader.f64()?;
            }
        }
        Some(BaseFold { preset, matrix })
    } else {
        None
    };
    let stage_count = reader.bounded_count(1)?;
    let mut reconstruction = Vec::with_capacity(stage_count);
    for _ in 0..stage_count {
        let stage_id = reader.u8()?;
        let base_count = reader.bounded_count(1)?;
        let mut base_logical = Vec::with_capacity(base_count);
        for _ in 0..base_count {
            base_logical.push(reader.u32()? as usize);
        }
        let required_count = reader.bounded_count(1)?;
        let mut required_groups = Vec::with_capacity(required_count);
        for _ in 0..required_count {
            required_groups.push(reader.u32()?);
        }
        let observation_count = reader.bounded_count(1)?;
        let mut observation_logical = Vec::with_capacity(observation_count);
        for _ in 0..observation_count {
            observation_logical.push(reader.u32()? as usize);
        }
        reconstruction.push(ReconstructionStage {
            stage_id,
            base_logical,
            required_groups,
            observation_logical,
        });
    }
    let object_total = reader.bounded_count(1)?;
    let mut initial_objects = Vec::with_capacity(object_total);
    for _ in 0..object_total {
        initial_objects.push(decode_object(&mut reader)?);
    }
    let trajectory_count = reader.bounded_count(1)?;
    let mut initial_trajectories = Vec::with_capacity(trajectory_count);
    for _ in 0..trajectory_count {
        let knot_count = reader.bounded_count(1)?;
        let mut trajectory = Vec::with_capacity(knot_count);
        for _ in 0..knot_count {
            let sample = reader.u64()?;
            let position = [reader.f64()?, reader.f64()?, reader.f64()?];
            trajectory.push(TrajectoryKnot { sample, position });
        }
        initial_trajectories.push(trajectory);
    }
    let config = SalsaSpatialStreamConfig {
        format_version,
        profile,
        config_id,
        object_count,
        headroom_gain,
        initial_objects,
        initial_trajectories,
        core_bitrates,
        beta,
        codec,
        signal_models,
        observation_groups,
        base_fold,
        reconstruction,
    };
    config.validate()?;
    Ok(config)
}

/// Encode one tile record payload.
pub fn encode_tile(tile: &TileRecord) -> Result<Vec<u8>> {
    let mut writer = Writer::new();
    writer.u32(TILE_MAGIC);
    writer.u64(tile.start_sample);
    writer.u32(tile.frames);
    writer.u64(tile.enhancement.start_sample);
    writer.u32(tile.enhancement.frames as u32);
    writer.u32(tile.enhancement.bytes.len() as u32);
    writer.bytes(&tile.enhancement.bytes);
    writer.u32(tile.objects.len() as u32);
    for object in &tile.objects {
        encode_object(&mut writer, object);
    }
    writer.u32(tile.events.len() as u32);
    for event in &tile.events {
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
    Ok(writer.out)
}

/// Decode one tile record payload.
pub fn decode_tile(bytes: &[u8], snapshot: bool) -> Result<TileRecord> {
    let mut reader = Reader::new(bytes);
    if reader.u32()? != TILE_MAGIC {
        return Err(SpatialError::Syntax("bad tile magic"));
    }
    let start_sample = reader.u64()?;
    let frames = reader.u32()?;
    let enhancement_start_sample = reader.u64()?;
    let enhancement_frames = reader.u32()?;
    let byte_count = reader.u32()? as usize;
    if byte_count > MAX_RECORD_BYTES {
        return Err(SpatialError::InvalidValue(
            "enhancement payload is too large",
        ));
    }
    let enhancement_bytes = reader.bytes(byte_count)?.to_vec();
    let object_count = reader.bounded_count(1)?;
    let mut objects = Vec::with_capacity(object_count);
    for _ in 0..object_count {
        objects.push(decode_object(&mut reader)?);
    }
    let event_count = reader.bounded_count(1)?;
    let mut events = Vec::with_capacity(event_count);
    for _ in 0..event_count {
        let offset = reader.i64()?;
        let transition = match reader.u8()? {
            0 => Transition::Step,
            1 => Transition::Position(reader.u32()?),
            2 => Transition::All(reader.u32()?),
            _ => return Err(SpatialError::InvalidValue("invalid scene transition")),
        };
        let state = decode_object(&mut reader)?;
        events.push(ObjectEvent {
            offset,
            transition,
            state,
        });
    }
    Ok(TileRecord {
        start_sample,
        frames,
        snapshot,
        enhancement: EnhancementTile {
            start_sample: enhancement_start_sample,
            frames: enhancement_frames as usize,
            bytes: enhancement_bytes,
            snapshot,
        },
        objects,
        events,
    })
}

/// One fragment of a record bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    pub config_id: u32,
    pub tile_start_sample: u64,
    pub index: u8,
    pub count: u8,
    pub total_len: usize,
    pub payload: Vec<u8>,
}

impl Fragment {
    /// Split a bundle into at most `count` fragments. The fragment count is
    /// bounded by [`MAX_RECORD_FRAGMENTS`].
    pub fn split(
        config_id: u32,
        tile_start_sample: u64,
        bytes: &[u8],
        count: usize,
    ) -> Result<Vec<Vec<u8>>> {
        if count == 0 || count > MAX_RECORD_FRAGMENTS || count > u8::MAX as usize {
            return Err(SpatialError::InvalidValue("invalid fragment count"));
        }
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(SpatialError::InvalidValue("record bundle is too large"));
        }
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            let start = index * bytes.len() / count;
            let end = (index + 1) * bytes.len() / count;
            let mut writer = Writer::new();
            writer.u32(RECORD_MAGIC);
            writer.u16(RECORD_VERSION);
            writer.u8(0); // fragment wrapper
            writer.u8(0);
            writer.u32(config_id);
            writer.u64(tile_start_sample);
            writer.u8(index as u8);
            writer.u8(count as u8);
            writer.u32(bytes.len() as u32);
            writer.u32((end - start) as u32);
            writer.bytes(&bytes[start..end]);
            out.push(writer.out);
        }
        Ok(out)
    }

    /// Parse one fragment wrapper.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.u32()? != RECORD_MAGIC {
            return Err(SpatialError::Syntax("bad fragment magic"));
        }
        let version = reader.u16()?;
        if version != RECORD_VERSION {
            return Err(SpatialError::UnsupportedVersion { version });
        }
        let _wrapper = reader.u8()?;
        let _reserved = reader.u8()?;
        let config_id = reader.u32()?;
        let tile_start_sample = reader.u64()?;
        let index = reader.u8()?;
        let count = reader.u8()?;
        let total_len = reader.u32()? as usize;
        let fragment_len = reader.u32()? as usize;
        if count == 0 || count as usize > MAX_RECORD_FRAGMENTS || index >= count {
            return Err(SpatialError::InvalidValue("invalid fragment index"));
        }
        if total_len > MAX_RECORD_BYTES {
            return Err(SpatialError::InvalidValue("record bundle is too large"));
        }
        if fragment_len > reader.remaining() {
            return Err(SpatialError::Truncated);
        }
        let payload = reader.bytes(fragment_len)?.to_vec();
        Ok(Self {
            config_id,
            tile_start_sample,
            index,
            count,
            total_len,
            payload,
        })
    }
}

/// Bounded reassembly of fragments for a single record bundle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reassembler {
    key: Option<(u32, u64)>,
    count: u8,
    total_len: usize,
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Accept one parsed fragment. Returns the complete bundle when every
    /// fragment is present.
    pub fn push(&mut self, fragment: Fragment) -> Result<Option<Vec<u8>>> {
        let key = (fragment.config_id, fragment.tile_start_sample);
        if self.key != Some(key) {
            self.key = Some(key);
            self.count = fragment.count;
            self.total_len = fragment.total_len;
            self.fragments = vec![None; fragment.count as usize];
            self.received = 0;
        }
        if self.count != fragment.count || self.total_len != fragment.total_len {
            self.reset();
            return Err(SpatialError::InvalidValue("inconsistent fragment framing"));
        }
        let index = fragment.index as usize;
        if self.fragments[index].is_some() {
            return Err(SpatialError::InvalidValue("duplicate fragment"));
        }
        self.fragments[index] = Some(fragment.payload);
        self.received += 1;
        if self.received != self.count as usize {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(self.total_len);
        for part in &self.fragments {
            let part = part.as_ref().ok_or(SpatialError::Truncated)?;
            out.extend_from_slice(part);
        }
        out.truncate(self.total_len);
        self.reset();
        Ok(Some(out))
    }
}

fn encode_binding(writer: &mut Writer, binding: &CodecBinding) {
    writer.u32(binding.output_channels as u32);
    writer.u8(binding.mapping_family);
    writer.u32(binding.stream_count as u32);
    writer.u32(binding.coupled_count as u32);
    writer.u32(binding.channel_mapping.len() as u32);
    writer.bytes(&binding.channel_mapping);
    writer.u32(binding.logical_to_coded.len() as u32);
    writer.bytes(&binding.logical_to_coded);
}

fn decode_binding(reader: &mut Reader) -> Result<CodecBinding> {
    let output_channels = reader.u32()? as usize;
    let mapping_family = reader.u8()?;
    let stream_count = reader.u32()? as usize;
    let coupled_count = reader.u32()? as usize;
    let map_len = reader.bounded_count(1)?;
    let channel_mapping = reader.bytes(map_len)?.to_vec();
    let logical_len = reader.bounded_count(1)?;
    let logical_to_coded = reader.bytes(logical_len)?.to_vec();
    Ok(CodecBinding {
        output_channels,
        mapping_family,
        stream_count,
        coupled_count,
        channel_mapping,
        logical_to_coded,
    })
}

fn encode_object(writer: &mut Writer, object: &ObjectState) {
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

fn decode_object(reader: &mut Reader) -> Result<ObjectState> {
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

struct Writer {
    out: Vec<u8>,
}

impl Writer {
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

    fn i64(&mut self, value: i64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn f32(&mut self, value: f32) {
        self.u32(value.to_bits());
    }

    fn f64(&mut self, value: f64) {
        self.u64(value.to_bits());
    }

    fn ulen(&mut self, value: u32) -> Result<()> {
        if value as usize > MAX_OBJECTS * 1024 {
            return Err(SpatialError::InvalidValue("length prefix is too large"));
        }
        self.u32(value);
        Ok(())
    }

    fn bytes(&mut self, value: &[u8]) {
        self.out.extend_from_slice(value);
    }
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }

    fn bounded_count(&mut self, minimum_size: usize) -> Result<usize> {
        let count = self.u32()? as usize;
        if count > MAX_OBJECTS * 1024 || count > self.remaining() / minimum_size.max(1) {
            return Err(SpatialError::Truncated);
        }
        Ok(count)
    }

    fn len(&mut self) -> Result<usize> {
        self.bounded_count(1)
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

    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
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

    fn sample_config() -> SalsaSpatialStreamConfig {
        SalsaSpatialStreamConfig {
            object_count: 2,
            headroom_gain: 0.75,
            initial_objects: vec![
                ObjectState::new(1, [0.1, 0.2, 0.3]),
                ObjectState::new(2, [-0.4, 0.5, 0.0]),
            ],
            initial_trajectories: vec![
                vec![TrajectoryKnot {
                    sample: 0,
                    position: [0.1, 0.2, 0.3],
                }],
                vec![],
            ],
            ..Default::default()
        }
    }

    #[test]
    fn configuration_round_trips() {
        let config = sample_config();
        let bytes = encode_configuration(&config).unwrap();
        assert_eq!(decode_configuration(&bytes).unwrap(), config);
    }

    #[test]
    fn record_round_trips() {
        let config = sample_config();
        let records = vec![
            Record::Configuration(config.clone()),
            Record::Tile(TileRecord {
                start_sample: 0,
                frames: 4800,
                snapshot: true,
                enhancement: EnhancementTile {
                    start_sample: 0,
                    frames: 4800,
                    bytes: vec![1, 2, 3],
                    snapshot: true,
                },
                objects: config.initial_objects.clone(),
                events: vec![],
            }),
            Record::GroupActivity(GroupActivity {
                start_sample: 0,
                active_groups: vec![1],
                active_rank: 2,
            }),
        ];
        let mut bundle = Vec::new();
        for record in &records {
            bundle.extend(write_record(record).unwrap());
        }
        assert_eq!(parse_records(&bundle).unwrap(), records);
    }

    #[test]
    fn fragment_reassembly_is_bounded_and_ordered() {
        let bundle: Vec<u8> = (0..1000u32).map(|value| value as u8).collect();
        let fragments = Fragment::split(7, 960, &bundle, 5).unwrap();
        let mut reassembler = Reassembler::new();
        let mut result = None;
        for bytes in fragments.iter().rev() {
            let fragment = Fragment::parse(bytes).unwrap();
            result = reassembler.push(fragment).unwrap().or(result);
        }
        assert_eq!(result.unwrap(), bundle);
    }

    #[test]
    fn duplicate_fragment_is_rejected() {
        let fragments = Fragment::split(1, 0, &[9u8; 20], 2).unwrap();
        let first = Fragment::parse(&fragments[0]).unwrap();
        let mut reassembler = Reassembler::new();
        assert!(reassembler.push(first.clone()).unwrap().is_none());
        assert!(reassembler.push(first).is_err());
    }
}
