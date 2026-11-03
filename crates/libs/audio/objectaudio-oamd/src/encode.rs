//! OAMD ID11 writer for the salsa spatial payload.
//!
//! The writer emits the subset of ETSI TS 103 420 / OAMD version 0 that the
//! `truehdd/oamd` parser and the Dolby decoders accept: a dynamic-object-only
//! program with one LFE channel, one object element, and up to eight 32-sample
//! aligned object-info blocks.

use std::collections::BTreeMap;

use bitstream_io::{BigEndian, BitWrite, BitWriter};
use objectaudio_ir::{
    METADATA_BLOCK_SAMPLES, ProgramCalibration, Snapshot, SpatialPayload, TrimRecord,
    TrimRecordKind, align_sample,
};

use crate::OamdError;

const MAX_BLOCKS: usize = 8;
const MAX_BLOCK_OFFSET: u32 = 63;

/// Encode a `SpatialPayload` as an OAMD ID11 payload.
///
/// Every non-LFE object is written as a dynamic object. Bed semantics are not
/// reconstructed: this is the documented lossy direction of the Dolby adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingProfile {
    ObjectAudio,
    Eac3Joc,
}

pub fn encode(payload: &SpatialPayload) -> Result<Vec<u8>, OamdError> {
    encode_with_profile(payload, EncodingProfile::ObjectAudio)
}

/// Profiles share the same OAMD schema. E-AC-3 adds its default trim element.
pub fn encode_with_profile(
    payload: &SpatialPayload,
    profile: EncodingProfile,
) -> Result<Vec<u8>, OamdError> {
    payload
        .validate()
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    if payload
        .objects
        .iter()
        .chain(payload.events.iter().map(|event| &event.state))
        .any(|object| object.spread.is_some())
    {
        return Err(OamdError::Encode(
            "object spread is not supported by the OAMD writer".into(),
        ));
    }
    let object_count = payload.objects.len() + 1;
    if object_count > 159 {
        return Err(OamdError::TooManyObjects {
            count: object_count,
            limit: 159,
        });
    }

    let blocks = block_table(payload)?;
    let mut body = BitWriter::new(Vec::new());
    write_body(&mut body, &blocks)?;
    body.byte_align()
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    let body = body.into_writer();

    let mut out = BitWriter::new(Vec::new());
    let write = |writer: &mut BitWriter<Vec<u8>, BigEndian>, bits: u32, value: u32| {
        writer
            .write_var::<u32>(bits, value)
            .map_err(|error| OamdError::Encode(error.to_string()))
    };
    write(&mut out, 2, 0)?; // oamd_version
    write_object_count(&mut out, object_count)?;
    let has_trim = profile == EncodingProfile::Eac3Joc || payload.calibration.is_some();
    out.write_bit(true)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_dyn_object_only_program
    out.write_bit(true)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_lfe_present
    out.write_bit(false)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_alternate_object_data_present
    write(&mut out, 4, if has_trim { 2 } else { 1 })?;

    write(&mut out, 4, 1)?; // oa_element_id_idx = object
    write_variable(&mut out, (body.len() - 1) as u32)?;
    out.write_bytes(&body)
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    if has_trim {
        // E-AC-3 always carries a trim element; the default is warp 0 with
        // automatic trim and no bypass. The object-audio profile only adds one
        // when the payload has calibration to preserve.
        let default = ProgramCalibration::default();
        let calibration = payload.calibration.as_ref().unwrap_or(&default);
        write_trim_element(&mut out, calibration, object_count)?;
    }
    out.byte_align()
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    Ok(out.into_writer())
}

fn write_trim_element(
    out: &mut BitWriter<Vec<u8>, BigEndian>,
    calibration: &ProgramCalibration,
    object_count: usize,
) -> Result<(), OamdError> {
    let mut body = BitWriter::new(Vec::new());
    body.write_bit(false)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_discard_unknown_element
    write_bits(&mut body, 2, u32::from(calibration.warp_mode))?;
    write_bits(&mut body, 2, 0)?; // reserved
    write_bits(&mut body, 2, u32::from(calibration.global_trim_mode))?;
    if calibration.global_trim_mode == 2 {
        for record in &calibration.configs {
            write_trim_record(&mut body, record)?;
        }
    }
    let bypass_any = calibration.bypass.iter().any(|bypass| *bypass);
    body.write_bit(bypass_any)
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    if bypass_any {
        // Slot 0 is the LFE; IR object 0 is OAMD slot 1.
        body.write_bit(false)
            .map_err(|error| OamdError::Encode(error.to_string()))?;
        for object in 0..object_count.saturating_sub(1) {
            let bypass = calibration.bypass.get(object).copied().unwrap_or(false);
            body.write_bit(bypass)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
        }
    }
    body.byte_align()
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    let body = body.into_writer();
    write_bits(out, 4, 2)?; // trim element
    write_variable(out, (body.len() - 1) as u32)?;
    out.write_bytes(&body)
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    Ok(())
}

fn write_trim_record(
    out: &mut BitWriter<Vec<u8>, BigEndian>,
    record: &TrimRecord,
) -> Result<(), OamdError> {
    match record.kind {
        TrimRecordKind::Absent | TrimRecordKind::Default => {
            out.write_bit(true)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
        }
        TrimRecordKind::Disabled => {
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
            out.write_bit(true)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
        }
        TrimRecordKind::Explicit => {
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
            let present = [
                record.centre_db.is_some(),
                record.surround_db.is_some(),
                record.height_db.is_some(),
                record.overhead_balance.is_some(),
                record.listener_balance.is_some(),
            ];
            let mut presence = 0u32;
            for (index, present) in present.iter().enumerate() {
                if *present {
                    presence |= 1 << index;
                }
            }
            write_bits(out, 5, presence)?;
            if present[0] {
                write_bits(out, 4, trim_code(record.centre_db()))?;
            }
            if present[1] {
                write_bits(out, 4, trim_code(record.surround_db()))?;
            }
            if present[2] {
                write_bits(out, 4, trim_code(record.height_db()))?;
            }
            if present[3] {
                write_balance(out, record.overhead_balance())?;
            }
            if present[4] {
                write_balance(out, record.listener_balance())?;
            }
        }
    }
    Ok(())
}

fn write_balance(out: &mut BitWriter<Vec<u8>, BigEndian>, balance: f64) -> Result<(), OamdError> {
    out.write_bit(balance >= 0.0)
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    let code = (balance.abs() * 16.0).round_ties_even().clamp(1.0, 16.0) as u32 - 1;
    write_bits(out, 4, code)
}

/// Nearest OAMD trim level code for a value from outside the OAMD parser.
fn trim_code(value: f64) -> u32 {
    let mut best = 0usize;
    let mut best_distance = f64::INFINITY;
    for (index, level) in crate::payload::TRIM_LUT.iter().enumerate() {
        let distance = (value - level).abs();
        if distance < best_distance {
            best = index;
            best_distance = distance;
        }
    }
    best as u32
}

fn write_bits(
    out: &mut BitWriter<Vec<u8>, BigEndian>,
    bits: u32,
    value: u32,
) -> Result<(), OamdError> {
    out.write_var::<u32>(bits, value)
        .map_err(|error| OamdError::Encode(error.to_string()))
}

fn write_object_count(
    out: &mut BitWriter<Vec<u8>, BigEndian>,
    count: usize,
) -> Result<(), OamdError> {
    let value = (count - 1) as u32;
    if value < 31 {
        out.write::<5, u32>(value)
            .map_err(|error| OamdError::Encode(error.to_string()))
    } else {
        out.write::<5, u32>(31)
            .map_err(|error| OamdError::Encode(error.to_string()))?;
        out.write::<7, u32>(value - 31)
            .map_err(|error| OamdError::Encode(error.to_string()))
    }
}

/// `get_variable_bits_max(4, 4)` inverse.
fn write_variable(out: &mut BitWriter<Vec<u8>, BigEndian>, value: u32) -> Result<(), OamdError> {
    let mut groups = Vec::new();
    let mut value = value;
    loop {
        groups.push((value & 0xF) as u8);
        value >>= 4;
        if value == 0 {
            break;
        }
        value -= 1;
    }
    if groups.len() > 4 {
        return Err(OamdError::Encode(format!(
            "OAMD element is too large ({value} groups)"
        )));
    }
    for (index, group) in groups.iter().rev().enumerate() {
        out.write::<4, u8>(*group)
            .map_err(|error| OamdError::Encode(error.to_string()))?;
        let more = index + 1 != groups.len();
        out.write_bit(more)
            .map_err(|error| OamdError::Encode(error.to_string()))?;
    }
    Ok(())
}

struct BlockTable {
    offsets: Vec<u32>,
    ramps: Vec<u16>,
    /// `state[slot - 1][block]`, where slot 0 is LFE.
    states: Vec<Vec<objectaudio_ir::ObjectState>>,
}

fn block_table(payload: &SpatialPayload) -> Result<BlockTable, OamdError> {
    let block_samples = METADATA_BLOCK_SAMPLES as u32;
    let block_at = |sample: i64| -> Result<u32, OamdError> {
        let aligned = align_sample(sample, block_samples);
        if sample < 0 || aligned > i64::from(MAX_BLOCK_OFFSET * block_samples) {
            return Err(OamdError::Encode(format!(
                "sample offset {sample} is outside the OAMD block range; resolve or split the scene first"
            )));
        }
        Ok((aligned / i64::from(block_samples)) as u32)
    };
    let snapshot_ramp = match payload.snapshot {
        Snapshot::State => 0,
        Snapshot::Continue => {
            return Err(OamdError::Encode(
                "resolve stream history before encoding a continuation".into(),
            ));
        }
        Snapshot::Target(transition) => transition.samples(),
    };
    let mut by_block = BTreeMap::new();
    by_block.insert(
        block_at(payload.snapshot_offset)?,
        matches!(payload.snapshot, Snapshot::Target(_)).then_some(snapshot_ramp),
    );
    let mut events = payload
        .events
        .iter()
        .map(|event| Ok((block_at(event.offset)?, event)))
        .collect::<Result<Vec<_>, OamdError>>()?;
    events.sort_by_key(|(_, event)| event.offset);
    for (block, event) in &events {
        let ramp = event.transition.samples();
        if let Some(Some(previous)) = by_block.insert(*block, Some(ramp))
            && previous != ramp
        {
            return Err(OamdError::Encode(
                "different ramp lengths share one OAMD block; resample the scene before encoding"
                    .into(),
            ));
        }
    }
    if by_block.len() > MAX_BLOCKS {
        return Err(OamdError::TooManyBlocks {
            count: by_block.len(),
            limit: MAX_BLOCKS,
        });
    }
    if by_block.values().any(|ramp| ramp.unwrap_or(0) > 2047) {
        return Err(OamdError::Encode(
            "ramp exceeds the OAMD duration range".into(),
        ));
    }
    let offsets: Vec<u32> = by_block.keys().copied().collect();
    let ramps = by_block
        .values()
        .map(|ramp| ramp.unwrap_or(0) as u16)
        .collect();
    let states = payload
        .objects
        .iter()
        .map(|object| {
            offsets
                .iter()
                .map(|block| {
                    events
                        .iter()
                        .rfind(|(at, event)| *at <= *block && event.state.id == object.id)
                        .map_or(*object, |(_, event)| event.state)
                })
                .collect()
        })
        .collect();
    Ok(BlockTable {
        offsets,
        ramps,
        states,
    })
}

fn write_body(
    out: &mut BitWriter<Vec<u8>, BigEndian>,
    table: &BlockTable,
) -> Result<(), OamdError> {
    let write = |writer: &mut BitWriter<Vec<u8>, BigEndian>, bits: u32, value: u32| {
        writer
            .write_var::<u32>(bits, value)
            .map_err(|error| OamdError::Encode(error.to_string()))
    };
    out.write_bit(false)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_discard_unknown_element
    write(out, 2, 0)?; // sample_offset_code 0
    write(out, 3, table.offsets.len() as u32 - 1)?;
    for (block, ramp) in table.offsets.iter().zip(&table.ramps) {
        write(out, 6, *block)?;
        write_ramp(out, *ramp)?;
    }
    out.write_bit(true)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_reserved_data_not_present

    // Slot 0: the LFE bed object.
    for block in 0..table.offsets.len() {
        out.write_bit(false)
            .map_err(|error| OamdError::Encode(error.to_string()))?; // active
        if block == 0 {
            write_basic(out, 1.0)?;
        } else {
            write(out, 2, 2)?; // repeat previous basic info
        }
        out.write_bit(false)
            .map_err(|error| OamdError::Encode(error.to_string()))?; // no additional table data
    }

    for object_states in &table.states {
        for (block, state) in object_states.iter().enumerate() {
            let active = state.enabled && state.gain > 0.0;
            out.write_bit(!active)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
            if !active {
                out.write_bit(false)
                    .map_err(|error| OamdError::Encode(error.to_string()))?;
                continue;
            }
            if block == 0 {
                write_basic(out, state.gain)?;
            } else {
                write(out, 2, 1)?; // all basic fields
                write_basic(out, state.gain)?;
            }
            if block > 0 {
                write(out, 2, 1)?; // all render fields
                out.write_bit(false)
                    .map_err(|error| OamdError::Encode(error.to_string()))?; // absolute position
            }
            write_position(out, state.position)?;
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?; // no distance
            write(out, 3, 0)?; // zone constraints
            out.write_bit(true)
                .map_err(|error| OamdError::Encode(error.to_string()))?; // elevation enabled
            write_size(out, state.size)?;
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?; // no screen ref
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?; // no snap
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?; // no additional table data
        }
    }
    Ok(())
}

fn write_ramp(out: &mut BitWriter<Vec<u8>, BigEndian>, ramp: u16) -> Result<(), OamdError> {
    let write = |writer: &mut BitWriter<Vec<u8>, BigEndian>, bits: u32, value: u32| {
        writer
            .write_var::<u32>(bits, value)
            .map_err(|error| OamdError::Encode(error.to_string()))
    };
    match ramp {
        0 => write(out, 2, 0),
        512 => write(out, 2, 1),
        1536 => write(out, 2, 2),
        _ => {
            write(out, 2, 3)?;
            out.write_bit(false)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
            write(out, 11, u32::from(ramp.min(2047)))
        }
    }
}

fn write_basic(out: &mut BitWriter<Vec<u8>, BigEndian>, gain: f32) -> Result<(), OamdError> {
    let write = |writer: &mut BitWriter<Vec<u8>, BigEndian>, bits: u32, value: u32| {
        writer
            .write_var::<u32>(bits, value)
            .map_err(|error| OamdError::Encode(error.to_string()))
    };
    if gain <= 0.0 || !gain.is_finite() {
        write(out, 2, 1)?; // -inf
    } else {
        let db = (20.0 * gain.log10()).round() as i32;
        if db == 0 {
            write(out, 2, 0)?;
        } else {
            let index = if db > 0 { 15 - db } else { 14 - db };
            write(out, 2, 2)?;
            write(out, 6, index.clamp(0, 63) as u32)?;
        }
    }
    out.write_bit(true)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // default priority
    Ok(())
}

fn write_position(
    out: &mut BitWriter<Vec<u8>, BigEndian>,
    position: [f32; 3],
) -> Result<(), OamdError> {
    let write = |writer: &mut BitWriter<Vec<u8>, BigEndian>, bits: u32, value: u32| {
        writer
            .write_var::<u32>(bits, value)
            .map_err(|error| OamdError::Encode(error.to_string()))
    };
    let x = (position[0].clamp(-1.0, 1.0) + 1.0) * 0.5;
    let y = (1.0 - position[1].clamp(-1.0, 1.0)) * 0.5;
    let z = position[2].clamp(-1.0, 1.0);
    write(out, 6, (x * 62.0).round().clamp(0.0, 62.0) as u32)?;
    write(out, 6, (y * 62.0).round().clamp(0.0, 62.0) as u32)?;
    out.write_bit(z >= 0.0)
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    write(out, 4, (z.abs() * 15.0).round().clamp(0.0, 15.0) as u32)
}

fn write_size(out: &mut BitWriter<Vec<u8>, BigEndian>, size: Option<f32>) -> Result<(), OamdError> {
    let write = |writer: &mut BitWriter<Vec<u8>, BigEndian>, bits: u32, value: u32| {
        writer
            .write_var::<u32>(bits, value)
            .map_err(|error| OamdError::Encode(error.to_string()))
    };
    match size.filter(|size| *size > 0.0) {
        Some(size) => {
            write(out, 2, 1)?;
            write(out, 5, (size.clamp(0.0, 1.0) * 31.0).round() as u32)?;
        }
        None => write(out, 2, 0)?,
    }
    Ok(())
}
