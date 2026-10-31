//! OAMD ID11 writer for the salsa spatial payload.
//!
//! The writer emits the subset of ETSI TS 103 420 / OAMD version 0 that the
//! `truehdd/oamd` parser and the Dolby decoders accept: a dynamic-object-only
//! program with one LFE channel, one object element, and up to eight 32-sample
//! aligned object-info blocks.

use std::collections::BTreeMap;

use bitstream_io::{BigEndian, BitWrite, BitWriter};
use objectaudio_ir::{METADATA_BLOCK_SAMPLES, SpatialPayload};

use crate::OamdError;

const MAX_BLOCKS: usize = 8;
const MAX_BLOCK_OFFSET: u32 = 63;

/// Encode a `SpatialPayload` as an OAMD ID11 payload.
///
/// Every non-LFE object is written as a dynamic object. Bed semantics are not
/// reconstructed: this is the documented lossy direction of the Dolby adapter.
pub fn encode(payload: &SpatialPayload) -> Result<Vec<u8>, OamdError> {
    let object_count = payload.objects.len() + 1;
    if object_count > 159 {
        return Err(OamdError::TooManyObjects {
            count: object_count,
            limit: 159,
        });
    }

    let blocks = block_table(payload);
    let mut body = BitWriter::new(Vec::new());
    write_body(&mut body, payload, &blocks)?;
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
    out.write_bit(true)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_dyn_object_only_program
    out.write_bit(true)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_lfe_present
    out.write_bit(false)
        .map_err(|error| OamdError::Encode(error.to_string()))?; // b_alternate_object_data_present
    write(&mut out, 4, 1)?; // one OA element

    write(&mut out, 4, 1)?; // oa_element_id_idx = object
    write_variable(&mut out, (body.len() - 1) as u32)?;
    out.write_bytes(&body)
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    out.byte_align()
        .map_err(|error| OamdError::Encode(error.to_string()))?;
    Ok(out.into_writer())
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
    states: Vec<Vec<Option<objectaudio_ir::ObjectState>>>,
}

fn block_table(payload: &SpatialPayload) -> BlockTable {
    let mut by_block: BTreeMap<u32, u16> = BTreeMap::new();
    by_block.insert(0, 0);
    let half_block = METADATA_BLOCK_SAMPLES as u32 / 2;
    let block_samples = METADATA_BLOCK_SAMPLES as u32;
    for event in &payload.events {
        let block = (event.offset.saturating_add(half_block) / block_samples).min(MAX_BLOCK_OFFSET);
        by_block
            .entry(block)
            .and_modify(|ramp| *ramp = (*ramp).max(event.ramp_samples.min(u16::MAX as u32) as u16))
            .or_insert(event.ramp_samples.min(u16::MAX as u32) as u16);
    }
    let mut offsets: Vec<u32> = by_block.keys().copied().collect();
    offsets.truncate(MAX_BLOCKS);
    let ramps: Vec<u16> = offsets
        .iter()
        .map(|block| by_block.get(block).copied().unwrap_or(0))
        .collect();

    let states = payload
        .objects
        .iter()
        .map(|object| {
            offsets
                .iter()
                .map(|block| {
                    let block_offset = block * block_samples;
                    payload
                        .events
                        .iter()
                        .rfind(|event| {
                            event.object == object.id
                                && (event.offset.saturating_add(half_block) / block_samples)
                                    .min(MAX_BLOCK_OFFSET)
                                    <= *block
                                && event.offset <= block_offset
                        })
                        .map(|event| event.state.clone())
                        .or_else(|| Some(object.clone()))
                })
                .collect()
        })
        .collect();
    BlockTable {
        offsets,
        ramps,
        states,
    }
}

fn write_body(
    out: &mut BitWriter<Vec<u8>, BigEndian>,
    _payload: &SpatialPayload,
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
            let state = state.as_ref();
            let active = state.is_some_and(|state| state.enabled && state.gain > 0.0);
            out.write_bit(!active)
                .map_err(|error| OamdError::Encode(error.to_string()))?;
            if !active {
                out.write_bit(false)
                    .map_err(|error| OamdError::Encode(error.to_string()))?;
                continue;
            }
            let state = state.expect("active state");
            if block == 0 {
                write_basic(out, state.gain)?;
            } else {
                write(out, 2, 3)?; // full basic info
                write_basic(out, state.gain)?;
            }
            if block > 0 {
                write(out, 2, 3)?; // full render info
                out.write_bit(false)
                    .map_err(|error| OamdError::Encode(error.to_string()))?; // absolute position
            }
            write_position(out, state.position, block > 0)?;
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
    _block: bool,
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
