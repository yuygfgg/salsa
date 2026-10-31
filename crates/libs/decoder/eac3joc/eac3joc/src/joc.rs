//! JOC (EMDF ID14) payload parser.
//!
//! Output matrices are the dequantized `dq` values consumed by the DSP kernel.

use crate::bit::BitReader;
use crate::emdf::{self, EmdfContainer};
use crate::error::{Eac3JocError, Result};
use crate::huffman_tables::*;

pub const MAX_OBJECTS: usize = 15;
pub const MAX_DPOINTS: usize = 2;
pub const CORE_CHANNELS: usize = 5;
pub const MAX_CORE_CHANNELS: usize = 7;
pub const MAX_PARAMETER_BANDS: usize = 23;
pub const FRAME_SAMPLES: usize = 1536;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JocObject {
    pub present: bool,
    pub num_bands_idx: u8,
    pub n_bands: usize,
    pub sparse: bool,
    pub quant_idx: u8,
    pub slope_idx: u8,
    pub n_dpoints: usize,
    pub offset_ts: [u8; MAX_DPOINTS],
    /// `dq[data_point][core_channel][parameter_band]`.
    pub dq: [[[f64; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS],
}

impl Default for JocObject {
    fn default() -> Self {
        Self {
            present: false,
            num_bands_idx: 0,
            n_bands: 1,
            sparse: false,
            quant_idx: 0,
            slope_idx: 0,
            n_dpoints: 1,
            offset_ts: [0; MAX_DPOINTS],
            dq: [[[0.0; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JocParams {
    pub dmx_config_idx: u8,
    pub num_objects_bits: u8,
    pub ext_config_idx: u8,
    pub n_channels: u8,
    pub n_objects: u8,
    pub objects_clamped: bool,
    pub clipgain_x_bits: u8,
    pub clipgain_y_bits: u8,
    pub seq_count: u16,
    pub clipgain: f64,
    pub objects: [JocObject; MAX_OBJECTS],
    pub present_mask: u16,
    pub data_end_bits: u32,
    pub trailing_bits: u32,
}

impl JocParams {
    /// Heap-allocated [`JocParams::silent`].
    pub fn silent_boxed() -> Box<Self> {
        Box::new(Self::silent())
    }

    /// Parameters for an audio frame that carries no JOC payload: every object
    /// present with an all-zero matrix, so the kernel advances its analysis and
    /// synthesis state while rendering silence.
    pub fn silent() -> Self {
        let mut objects = [JocObject::default(); MAX_OBJECTS];
        for object in &mut objects {
            object.present = true;
            object.n_bands = 1;
            object.n_dpoints = 1;
        }
        Self {
            dmx_config_idx: 0,
            num_objects_bits: 0,
            ext_config_idx: 0,
            n_channels: CORE_CHANNELS as u8,
            n_objects: MAX_OBJECTS as u8,
            objects_clamped: false,
            clipgain_x_bits: 0,
            clipgain_y_bits: 0,
            seq_count: 0,
            clipgain: 1.0,
            objects,
            present_mask: (1 << MAX_OBJECTS) - 1,
            data_end_bits: 0,
            trailing_bits: 0,
        }
    }

    /// Phase-shift (sigma(k)) switch for `dmx_config_idx` 3 and 4.
    pub fn phase_shift(&self) -> bool {
        matches!(self.dmx_config_idx, 3 | 4)
    }
}

pub fn num_channels_for_config(config: u32) -> Option<u8> {
    match config {
        0 | 3 => Some(5),
        1 | 2 | 4 => Some(7),
        _ => None,
    }
}

pub fn num_bands_for_index(index: u32) -> Option<usize> {
    match index {
        0 => Some(1),
        1 => Some(3),
        2 => Some(5),
        3 => Some(7),
        4 => Some(9),
        5 => Some(12),
        6 => Some(15),
        7 => Some(23),
        _ => None,
    }
}

fn floored_mod(value: i64, modulus: i64) -> i64 {
    let remainder = value % modulus;
    if remainder < 0 {
        remainder + modulus
    } else {
        remainder
    }
}

#[derive(Clone, Copy)]
enum SymbolKind {
    Mtx,
    Idx,
    Vec,
}

fn select_tree(quant_idx: u8, kind: SymbolKind, n_channels: usize) -> &'static [[i16; 2]] {
    match kind {
        SymbolKind::Idx => {
            if n_channels == 5 {
                &JOC_HUFF_CODE_5CH_POS_INDEX_SPARSE
            } else {
                &JOC_HUFF_CODE_7CH_POS_INDEX_SPARSE
            }
        }
        SymbolKind::Vec => {
            if quant_idx == 0 {
                &JOC_HUFF_CODE_COARSE_COEFF_SPARSE
            } else {
                &JOC_HUFF_CODE_FINE_COEFF_SPARSE
            }
        }
        SymbolKind::Mtx => {
            if quant_idx == 0 {
                &JOC_HUFF_CODE_COARSE_GENERIC
            } else {
                &JOC_HUFF_CODE_FINE_GENERIC
            }
        }
    }
}

fn huff_decode(tree: &[[i16; 2]], reader: &mut BitReader<'_>) -> Result<i16> {
    let mut node = 0i32;
    let mut steps = 0usize;
    while node >= 0 {
        if node as usize >= tree.len() || steps > tree.len() {
            return Err(Eac3JocError::JocSyntax(
                "Huffman tree walk left the valid node range".into(),
            ));
        }
        steps += 1;
        let bit = reader.read(1) as usize;
        if reader.is_failed() {
            return Err(Eac3JocError::BitstreamTruncated);
        }
        node = i32::from(tree[node as usize][bit]);
    }
    Ok(-node as i16 - 1)
}

#[allow(clippy::needless_range_loop)]
fn reconstruct_dense(
    mtx: &[[[i16; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS],
    n_bands: usize,
    n_channels: usize,
    nquant: i64,
    offset: i64,
    q: &mut [[[i64; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS],
) {
    for channel in 0..n_channels {
        q[0][channel][0] = floored_mod(offset + i64::from(mtx[0][channel][0]), nquant);
        for band in 1..n_bands {
            q[0][channel][band] = floored_mod(
                q[0][channel][band - 1] + i64::from(mtx[0][channel][band]),
                nquant,
            );
        }
    }
}

#[allow(clippy::needless_range_loop, clippy::too_many_arguments)]
fn reconstruct_sparse(
    idx: &[[u8; MAX_PARAMETER_BANDS]; MAX_DPOINTS],
    vec: &[[i16; MAX_PARAMETER_BANDS]; MAX_DPOINTS],
    data_point: usize,
    n_bands: usize,
    n_channels: usize,
    nquant: i64,
    offset: i64,
    q: &mut [[[i64; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS],
) -> Result<()> {
    if n_channels != 5 && n_channels != 7 {
        return Err(Eac3JocError::UnsupportedVariant(format!(
            "sparse JOC requires 5 or 7 core channels, got {n_channels}"
        )));
    }
    let initial_channel = usize::from(idx[data_point][0]);
    if initial_channel >= n_channels {
        return Err(Eac3JocError::JocSyntax(format!(
            "sparse JOC initial channel {initial_channel} out of range for {n_channels} channels"
        )));
    }
    for channel in 0..n_channels {
        for band in 0..n_bands {
            q[data_point][channel][band] = nquant / 2;
        }
    }
    let mut active = initial_channel;
    let mut coefficient = offset;
    for band in 0..n_bands {
        if band != 0 {
            active = floored_mod(
                active as i64 + i64::from(idx[data_point][band]),
                n_channels as i64,
            ) as usize;
        }
        coefficient = floored_mod(coefficient + i64::from(vec[data_point][band]), nquant);
        q[data_point][active][band] = coefficient;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ObjectSymbols {
    n_bands: usize,
    n_dpoints: usize,
    sparse: bool,
    quant_idx: u8,
    mtx: [[[i16; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS],
    idx: [[u8; MAX_PARAMETER_BANDS]; MAX_DPOINTS],
    vec: [[i16; MAX_PARAMETER_BANDS]; MAX_DPOINTS],
}

impl Default for ObjectSymbols {
    fn default() -> Self {
        Self {
            n_bands: 1,
            n_dpoints: 1,
            sparse: false,
            quant_idx: 0,
            mtx: [[[0; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS],
            idx: [[0; MAX_PARAMETER_BANDS]; MAX_DPOINTS],
            vec: [[0; MAX_PARAMETER_BANDS]; MAX_DPOINTS],
        }
    }
}

/// Parse an ID14 payload.
pub fn parse_id14(payload: &[u8]) -> Result<JocParams> {
    let mut reader = BitReader::new(payload);
    let dmx_config_idx = reader.read(3) as u8;
    let num_objects_bits = reader.read(6) as u8;
    let ext_config_idx = reader.read(3) as u8;

    let declared_objects = u32::from(num_objects_bits) + 1;
    let n_channels = num_channels_for_config(u32::from(dmx_config_idx)).ok_or_else(|| {
        Eac3JocError::JocSyntax(format!(
            "unknown JOC downmix configuration {dmx_config_idx}"
        ))
    })?;
    let objects_clamped = declared_objects > MAX_OBJECTS as u32;
    let n_objects = declared_objects.min(MAX_OBJECTS as u32) as u8;

    let clipgain_x_bits = reader.read(3) as u8;
    let clipgain_y_bits = reader.read(5) as u8;
    let seq_count = reader.read(10) as u16;
    let clipgain =
        1.0 + f64::from(clipgain_y_bits) / 32.0 * (2.0f64).powi(i32::from(clipgain_x_bits) - 4);

    let mut objects = [JocObject::default(); MAX_OBJECTS];
    for object in objects.iter_mut().take(n_objects as usize) {
        object.present = reader.read(1) != 0;
        if !object.present {
            continue;
        }
        object.num_bands_idx = reader.read(3) as u8;
        let n_bands = num_bands_for_index(u32::from(object.num_bands_idx)).ok_or_else(|| {
            Eac3JocError::JocSyntax(format!(
                "unknown JOC num_bands index {}",
                object.num_bands_idx
            ))
        })?;
        object.n_bands = n_bands;
        object.sparse = reader.read(1) != 0;
        object.quant_idx = reader.read(1) as u8;
        object.slope_idx = reader.read(1) as u8;
        object.n_dpoints = reader.read(1) as usize + 1;
        if object.slope_idx == 1 {
            for data_point in 0..object.n_dpoints {
                object.offset_ts[data_point] = reader.read(5) as u8 + 1;
            }
        }
    }
    if reader.is_failed() {
        return Err(Eac3JocError::BitstreamTruncated);
    }

    let mut symbols = [ObjectSymbols::default(); MAX_OBJECTS];
    for (object_index, object) in objects.iter().take(n_objects as usize).enumerate() {
        if !object.present {
            continue;
        }
        let symbol = &mut symbols[object_index];
        symbol.n_bands = object.n_bands;
        symbol.n_dpoints = object.n_dpoints;
        symbol.sparse = object.sparse;
        symbol.quant_idx = object.quant_idx;
        for data_point in 0..object.n_dpoints {
            if object.sparse {
                let idx_tree =
                    select_tree(object.quant_idx, SymbolKind::Idx, usize::from(n_channels));
                symbol.idx[data_point][0] = reader.read(3) as u8;
                if reader.is_failed() {
                    return Err(Eac3JocError::BitstreamTruncated);
                }
                for band in 1..object.n_bands {
                    symbol.idx[data_point][band] = huff_decode(idx_tree, &mut reader)? as u8;
                }
                let vec_tree =
                    select_tree(object.quant_idx, SymbolKind::Vec, usize::from(n_channels));
                for band in 0..object.n_bands {
                    symbol.vec[data_point][band] = huff_decode(vec_tree, &mut reader)?;
                }
            } else {
                let mtx_tree =
                    select_tree(object.quant_idx, SymbolKind::Mtx, usize::from(n_channels));
                for channel in 0..usize::from(n_channels) {
                    for band in 0..object.n_bands {
                        symbol.mtx[data_point][channel][band] = huff_decode(mtx_tree, &mut reader)?;
                    }
                }
            }
        }
    }

    let data_end_bits = reader.position() as u32;
    let trailing_bits = (payload.len() * 8).saturating_sub(reader.position()) as u32;

    let mut present_mask = 0u16;
    for (object_index, object) in objects.iter_mut().take(n_objects as usize).enumerate() {
        if !object.present {
            continue;
        }
        present_mask |= 1u16 << object_index;
        let symbol = &symbols[object_index];
        let nquant: i64 = if object.quant_idx == 0 { 96 } else { 192 };
        let mut q = [[[0i64; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS];
        for data_point in 0..object.n_dpoints {
            if object.sparse {
                let offset: i64 = if object.quant_idx == 0 { 50 } else { 100 };
                reconstruct_sparse(
                    &symbol.idx,
                    &symbol.vec,
                    data_point,
                    object.n_bands,
                    usize::from(n_channels),
                    nquant,
                    offset,
                    &mut q,
                )?;
            } else {
                let offset: i64 = if object.quant_idx == 0 { 48 } else { 96 };
                reconstruct_dense(
                    &symbol.mtx,
                    object.n_bands,
                    usize::from(n_channels),
                    nquant,
                    offset,
                    &mut q,
                );
            }
        }
        let nquant_half = nquant as f64 / 2.0;
        let denominator = 4096.0 * f64::from(1 + u16::from(object.quant_idx));
        #[allow(clippy::needless_range_loop)]
        for data_point in 0..object.n_dpoints {
            for channel in 0..usize::from(n_channels) {
                for band in 0..object.n_bands {
                    let value = q[data_point][channel][band] as f64 - nquant_half;
                    object.dq[data_point][channel][band] = value * 820.0 / denominator;
                }
            }
        }
    }

    Ok(JocParams {
        dmx_config_idx,
        num_objects_bits,
        ext_config_idx,
        n_channels,
        n_objects,
        objects_clamped,
        clipgain_x_bits,
        clipgain_y_bits,
        seq_count,
        clipgain,
        objects,
        present_mask,
        data_end_bits,
        trailing_bits,
    })
}

/// Score a container as this frame's JOC metadata: 2 when the ID11 and ID14
/// payloads share a group id and ID14 parses, 1 when ID14 parses, else 0.
pub fn score_container_with_frame(frame: &[u8], container: &EmdfContainer) -> i32 {
    let Some(payload) = container.find(emdf::ID_JOC) else {
        return 0;
    };
    let Some(oamd) = container.find(emdf::ID_OAMD) else {
        return 0;
    };
    let parses = emdf::extract_payload_bytes(frame, payload)
        .ok()
        .and_then(|bytes| parse_id14(&bytes).ok())
        .is_some();
    let associated = oamd.has_group_id && payload.has_group_id && oamd.group_id == payload.group_id;
    if associated && parses {
        2
    } else if parses {
        1
    } else {
        0
    }
}

/// Parse the JOC payload from a syncframe, returning the params and container.
pub fn parse_syncframe(frame: &[u8]) -> Result<(JocParams, EmdfContainer)> {
    let container = emdf::find_joc_emdf(frame, None)?;
    let payload = container
        .find(emdf::ID_JOC)
        .ok_or_else(|| Eac3JocError::EmdfTransport("EMDF container has no ID14 payload".into()))?;
    let bytes = emdf::extract_payload_bytes(frame, payload)?;
    let params = parse_id14(&bytes)?;
    Ok((params, container))
}

/// Parse the JOC payload, or `None` when the syncframe carries no EMDF.
pub fn parse_syncframe_optional(frame: &[u8]) -> Result<Option<(JocParams, EmdfContainer)>> {
    let score = |container: &EmdfContainer| score_container_with_frame(frame, container);
    let container = emdf::find_joc_emdf_optional(frame, Some(&score))?;
    let Some(container) = container else {
        return Ok(None);
    };
    let payload = container
        .find(emdf::ID_JOC)
        .ok_or_else(|| Eac3JocError::EmdfTransport("EMDF container has no ID14 payload".into()))?;
    let bytes = emdf::extract_payload_bytes(frame, payload)?;
    Ok(Some((parse_id14(&bytes)?, container)))
}

/// Check that at most seven padding bits remain and all are zero.
pub fn check_id14_padding(payload: &[u8]) -> Result<u32> {
    let params = parse_id14(payload)?;
    if params.trailing_bits > 7 {
        return Err(Eac3JocError::JocSyntax(format!(
            "more than 7 bits left after joc_data ({})",
            params.trailing_bits
        )));
    }
    for bit in params.data_end_bits as usize..payload.len() * 8 {
        if (payload[bit >> 3] >> (7 - (bit & 7))) & 1 != 0 {
            return Err(Eac3JocError::JocSyntax(format!(
                "non-zero trailing padding bit at {bit}"
            )));
        }
    }
    Ok(params.trailing_bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_counts_match_the_spec() {
        assert_eq!(num_channels_for_config(3), Some(5));
        assert_eq!(num_channels_for_config(1), Some(7));
        assert_eq!(num_channels_for_config(5), None);
    }

    #[test]
    fn band_counts_match_the_spec() {
        assert_eq!(num_bands_for_index(5), Some(12));
        assert_eq!(num_bands_for_index(8), None);
    }

    #[test]
    fn silent_params_have_no_matrix() {
        let params = JocParams::silent();
        assert_eq!(params.present_mask, 0x7fff);
        assert_eq!(params.clipgain, 1.0);
        assert!(!params.phase_shift());
    }

    #[test]
    fn dense_reconstruction_is_cumulative() {
        let mut mtx = [[[0i16; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS];
        mtx[0][0][0] = -48;
        mtx[0][0][1] = 1;
        let mut q = [[[0i64; MAX_PARAMETER_BANDS]; MAX_CORE_CHANNELS]; MAX_DPOINTS];
        reconstruct_dense(&mtx, 2, 1, 96, 48, &mut q);
        assert_eq!(q[0][0][0], 0);
        assert_eq!(q[0][0][1], 1);
    }
}
