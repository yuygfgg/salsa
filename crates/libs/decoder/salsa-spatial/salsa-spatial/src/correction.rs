//! Optional rank-r correction of the object reconstruction matrix.
//!
//! The pair covariance model produces a baseline estimator `M0`. At the
//! encoder, the measured source/decoded-core moments give an empirical
//! estimator `M_star = R_sy R_yy^dagger`. The difference is compressed into a
//! small number of sparse-support rank-one factors per parameter band:
//!
//! `Delta M_b = sum_k left_k right_k` (an outer product), `M_b = M0_b + Delta M_b`.
//!
//! Each component stores a sparse complex left vector (`support` + `left`,
//! with `left_scale`) and a dense seven-channel right factor (`right`, with
//! `right_scale`). The right factor already contains the conjugate transpose
//! directions from the SVD, so the decoder applies the plain outer product. Components are quantized on a signed 4-bit grid. The tile
//! correction is appended after the rANS parameter payload as a length-delimited
//! trailer, so decoders that do not understand it skip it and keep the pair
//! baseline.

use num_complex::Complex;

use crate::constants::{CORE_CHANNEL_COUNT, MAX_OBJECTS, PARAM_BAND_COUNT};
use crate::error::{Result, SpatialError};

/// Trailer magic written after the rANS payload of a corrected tile.
pub const CORRECTION_TRAILER_MAGIC: u32 = 0x5343_5254; // "SCRT"
/// Correction payload version.
pub const CORRECTION_VERSION: u8 = 2;
/// Maximum number of rank-one components per band supported by the syntax.
pub const CORRECTION_MAX_RANK: usize = 7;
/// Maximum sparse support length per component.
pub const CORRECTION_MAX_SUPPORT: usize = 32;
/// Maximum number of correction keyframes carried by one tile.
pub const CORRECTION_MAX_KEYFRAMES: usize = 8;
/// Quantized component magnitude levels (signed four-bit grid).
pub const CORRECTION_VALUE_BITS: u8 = 4;

/// One sparse rank-one factor of the reconstruction-matrix correction.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrectionComponent {
    /// Original object indices. Sorted ascending and unique.
    pub support: Vec<u16>,
    /// Quantized left-vector values in `[-1, 1]`, one per support entry.
    pub left: Vec<Complex<f64>>,
    /// Quantized right-vector values in `[-1, 1]`.
    pub right: [Complex<f64>; CORE_CHANNEL_COUNT],
    /// Scale of the left vector before quantization.
    pub left_scale: f64,
    /// Scale of the right vector before quantization.
    pub right_scale: f64,
}

impl CorrectionComponent {
    /// Add this component to one band's object-by-observation matrix.
    pub fn apply(&self, rows: &mut [[Complex<f64>; CORE_CHANNEL_COUNT]]) {
        for (&object, &left) in self.support.iter().zip(&self.left) {
            let object = usize::from(object);
            let Some(row) = rows.get_mut(object) else {
                continue;
            };
            let left = left * self.left_scale;
            for (value, right) in row.iter_mut().zip(&self.right) {
                *value += left * (right * self.right_scale);
            }
        }
    }

    /// Add only the real part of this component to a real observation system.
    pub fn apply_real(&self, rows: &mut [[Complex<f64>; CORE_CHANNEL_COUNT]]) {
        for (&object, &left) in self.support.iter().zip(&self.left) {
            let object = usize::from(object);
            let Some(row) = rows.get_mut(object) else {
                continue;
            };
            let left = left * self.left_scale;
            for (value, right) in row.iter_mut().zip(&self.right) {
                value.re += (left * (right * self.right_scale)).re;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.support.is_empty()
            || self
                .left
                .iter()
                .chain(self.right.iter())
                .all(|value| value.norm_sqr() == 0.0)
    }
}

/// Rank-r correction for one parameter band.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BandCorrectionSet {
    pub components: Vec<CorrectionComponent>,
}

impl BandCorrectionSet {
    pub fn is_empty(&self) -> bool {
        self.components.is_empty() || self.components.iter().all(CorrectionComponent::is_empty)
    }

    /// Add every component of this band to an object-by-observation matrix.
    pub fn apply(&self, rows: &mut [[Complex<f64>; CORE_CHANNEL_COUNT]]) {
        for component in &self.components {
            component.apply(rows);
        }
    }

    /// Add only the real part of every component.
    pub fn apply_real(&self, rows: &mut [[Complex<f64>; CORE_CHANNEL_COUNT]]) {
        for component in &self.components {
            component.apply_real(rows);
        }
    }
}

/// Per-tile correction, one [`BandCorrectionSet`] per parameter band.
#[derive(Debug, Clone, PartialEq)]
pub struct TileCorrection {
    pub bands: Vec<BandCorrectionSet>,
}

impl Default for TileCorrection {
    fn default() -> Self {
        Self {
            bands: vec![BandCorrectionSet::default(); PARAM_BAND_COUNT],
        }
    }
}

impl TileCorrection {
    pub fn is_empty(&self) -> bool {
        self.bands.iter().all(BandCorrectionSet::is_empty)
    }

    pub fn band(&self, band: usize) -> Option<&BandCorrectionSet> {
        self.bands.get(band).filter(|set| !set.is_empty())
    }

    /// Materialize the correction as `[band][object][channel]` dense rows.
    ///
    /// The decoder only needs the rows that are actually supported, but the
    /// dense form keeps the per-hop interpolation straightforward.
    pub fn dense(&self, object_count: usize) -> Vec<Vec<[Complex<f64>; CORE_CHANNEL_COUNT]>> {
        (0..PARAM_BAND_COUNT)
            .map(|band| {
                let mut rows = vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; object_count];
                if let Some(set) = self.band(band) {
                    set.apply(&mut rows);
                }
                rows
            })
            .collect()
    }

    /// Serialize this correction as a single-keyframe trailer.
    pub fn encode(&self) -> Vec<u8> {
        encode_keyframes(std::slice::from_ref(self))
    }
}

fn write_bands(payload: &mut Vec<u8>, correction: &TileCorrection) {
    for band in 0..PARAM_BAND_COUNT {
        let components = correction
            .bands
            .get(band)
            .map_or(&[][..], |set| &set.components);
        let rank = components.len().min(CORRECTION_MAX_RANK);
        payload.push(rank as u8);
        for component in components.iter().take(rank) {
            let support_len = component
                .support
                .len()
                .min(CORRECTION_MAX_SUPPORT)
                .min(component.left.len());
            payload.push(support_len as u8);
            for &object in component.support.iter().take(support_len) {
                payload.push(object.min((MAX_OBJECTS - 1) as u16) as u8);
            }
            for value in component.left.iter().take(support_len) {
                payload.push(pack_complex(*value));
            }
            for value in &component.right {
                payload.push(pack_complex(*value));
            }
            payload.extend_from_slice(&(component.left_scale as f32).to_le_bytes());
            payload.extend_from_slice(&(component.right_scale as f32).to_le_bytes());
        }
    }
}

/// Serialize a list of per-tile correction keyframes, including the trailer.
pub fn encode_keyframes(corrections: &[TileCorrection]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(CORRECTION_VERSION);
    let count = corrections.len().min(CORRECTION_MAX_KEYFRAMES);
    payload.push(count as u8);
    for correction in corrections.iter().take(count) {
        write_bands(&mut payload, correction);
    }
    let length = payload.len() as u32;
    payload.extend_from_slice(&length.to_le_bytes());
    payload.extend_from_slice(&CORRECTION_TRAILER_MAGIC.to_le_bytes());
    payload
}

/// Parse every optional correction keyframe from a complete tile payload.
///
/// The rANS payload comes first and is ignored here; corrections are present
/// only when the final eight bytes are `[payload length][magic]`.
pub fn decode_trailing_corrections(bytes: &[u8]) -> Result<Vec<TileCorrection>> {
    Ok(decode_optional_payload(bytes)?.unwrap_or_default())
}

/// Parse the first correction keyframe, if any. Kept for single-keyframe
/// callers and tests.
pub fn decode_trailing_correction(bytes: &[u8]) -> Result<Option<TileCorrection>> {
    Ok(decode_trailing_corrections(bytes)?.into_iter().next())
}

fn decode_optional_payload(bytes: &[u8]) -> Result<Option<Vec<TileCorrection>>> {
    if bytes.len() < 8 {
        return Ok(None);
    }
    let magic = u32::from_le_bytes(
        bytes[bytes.len() - 4..]
            .try_into()
            .map_err(|_| SpatialError::Truncated)?,
    );
    if magic != CORRECTION_TRAILER_MAGIC {
        return Ok(None);
    }
    let length = u32::from_le_bytes(
        bytes[bytes.len() - 8..bytes.len() - 4]
            .try_into()
            .map_err(|_| SpatialError::Truncated)?,
    ) as usize;
    let start = bytes
        .len()
        .checked_sub(8)
        .and_then(|end| end.checked_sub(length))
        .ok_or(SpatialError::Truncated)?;
    decode_payload(&bytes[start..start + length]).map(Some)
}

fn decode_payload(bytes: &[u8]) -> Result<Vec<TileCorrection>> {
    let mut reader = SliceReader { bytes, offset: 0 };
    let version = reader.u8()?;
    let corrections = match version {
        1 => vec![decode_bands(&mut reader)?],
        CORRECTION_VERSION => {
            let count = usize::from(reader.u8()?);
            if count > CORRECTION_MAX_KEYFRAMES {
                return Err(SpatialError::Parameter("too many correction keyframes"));
            }
            let mut corrections = Vec::with_capacity(count);
            for _ in 0..count {
                corrections.push(decode_bands(&mut reader)?);
            }
            corrections
        }
        other => {
            return Err(SpatialError::UnsupportedVersion {
                version: u16::from(other),
            });
        }
    };
    if reader.offset != bytes.len() {
        return Err(SpatialError::Syntax("trailing correction payload"));
    }
    Ok(corrections)
}

fn decode_bands(reader: &mut SliceReader<'_>) -> Result<TileCorrection> {
    let mut bands = Vec::with_capacity(PARAM_BAND_COUNT);
    for _band in 0..PARAM_BAND_COUNT {
        let rank = usize::from(reader.u8()?);
        if rank > CORRECTION_MAX_RANK {
            return Err(SpatialError::Parameter("correction rank out of range"));
        }
        let mut components = Vec::with_capacity(rank);
        for _ in 0..rank {
            let support_len = usize::from(reader.u8()?);
            if support_len > CORRECTION_MAX_SUPPORT {
                return Err(SpatialError::Parameter("correction support too large"));
            }
            let mut support = Vec::with_capacity(support_len);
            for _ in 0..support_len {
                let object = u16::from(reader.u8()?);
                if usize::from(object) >= MAX_OBJECTS {
                    return Err(SpatialError::Parameter("correction object out of range"));
                }
                if support.last().is_some_and(|&previous| object <= previous) {
                    return Err(SpatialError::Parameter("correction support is not sorted"));
                }
                support.push(object);
            }
            let mut left = Vec::with_capacity(support.len());
            for _ in 0..support.len() {
                left.push(unpack_complex(reader.u8()?));
            }
            let mut right = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
            for value in &mut right {
                *value = unpack_complex(reader.u8()?);
            }
            let left_scale = f64::from(reader.f32()?);
            let right_scale = f64::from(reader.f32()?);
            if !left_scale.is_finite() || !right_scale.is_finite() {
                return Err(SpatialError::Parameter("non-finite correction scale"));
            }
            components.push(CorrectionComponent {
                support,
                left,
                right,
                left_scale,
                right_scale,
            });
        }
        bands.push(BandCorrectionSet { components });
    }
    Ok(TileCorrection { bands })
}

/// Quantize one complex value onto the signed four-bit grid.
pub fn quantize_complex(value: Complex<f64>) -> Complex<f64> {
    Complex::new(quantize_real(value.re), quantize_real(value.im))
}

fn quantize_real(value: f64) -> f64 {
    let quantized = (value * 7.0).round().clamp(-7.0, 7.0) as i8;
    f64::from(quantized) / 7.0
}

fn pack_complex(value: Complex<f64>) -> u8 {
    let real = ((value.re * 7.0).round().clamp(-7.0, 7.0) as i8 + 8) as u8 & 0x0F;
    let imag = ((value.im * 7.0).round().clamp(-7.0, 7.0) as i8 + 8) as u8 & 0x0F;
    (real << 4) | imag
}

fn unpack_complex(value: u8) -> Complex<f64> {
    let real = i8::try_from((value >> 4) & 0x0F).expect("nibble") - 8;
    let imag = i8::try_from(value & 0x0F).expect("nibble") - 8;
    Complex::new(f64::from(real) / 7.0, f64::from(imag) / 7.0)
}

struct SliceReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> SliceReader<'a> {
    fn u8(&mut self) -> Result<u8> {
        let value = *self.bytes.get(self.offset).ok_or(SpatialError::Truncated)?;
        self.offset += 1;
        Ok(value)
    }

    fn f32(&mut self) -> Result<f32> {
        let end = self.offset.checked_add(4).ok_or(SpatialError::Truncated)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(SpatialError::Truncated)?;
        self.offset = end;
        Ok(f32::from_le_bytes(
            slice.try_into().map_err(|_| SpatialError::Truncated)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantization_roundtrips_the_signed_four_bit_grid() {
        for step in -7..=7 {
            let value = f64::from(step) / 7.0;
            assert!((quantize_real(value) - value).abs() < 1e-12);
        }
        let value = Complex::new(0.5, -0.25);
        assert!((quantize_complex(value) - value).norm() < 0.08);
    }

    #[test]
    fn tile_correction_trailer_roundtrips() {
        let mut correction = TileCorrection::default();
        correction.bands[3].components.push(CorrectionComponent {
            support: vec![1, 7, 42],
            left: vec![
                Complex::new(1.0, 0.0),
                Complex::new(0.5, -0.25),
                Complex::new(-0.125, 0.0),
            ],
            right: [
                Complex::new(1.0, 0.0),
                Complex::new(0.25, 0.5),
                Complex::new(0.0, -0.5),
                Complex::new(-0.25, 0.0),
                Complex::new(0.125, 0.0),
                Complex::new(0.0, 0.0),
                Complex::new(-0.125, 0.125),
            ],
            left_scale: 0.75,
            right_scale: 2.0,
        });
        let mut bytes = vec![9, 8, 7, 6];
        bytes.extend(correction.encode());
        let decoded = decode_trailing_correction(&bytes).unwrap().unwrap();
        assert_eq!(decoded.bands[3].components.len(), 1);
        let component = &decoded.bands[3].components[0];
        assert_eq!(component.support, vec![1, 7, 42]);
        assert!((component.left_scale - 0.75).abs() < 1e-6);
        assert!((component.right_scale - 2.0).abs() < 1e-6);
    }

    #[test]
    fn missing_trailer_is_not_an_error() {
        let bytes = [0u8; 32];
        assert!(decode_trailing_correction(&bytes).unwrap().is_none());
    }

    #[test]
    fn dense_rows_match_component_application() {
        let mut correction = TileCorrection::default();
        correction.bands[0].components.push(CorrectionComponent {
            support: vec![1],
            left: vec![Complex::new(0.5, 0.0)],
            right: [Complex::new(1.0, 0.0); CORE_CHANNEL_COUNT],
            left_scale: 2.0,
            right_scale: 1.0,
        });
        let dense = correction.dense(3);
        let mut expected = vec![[Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT]; 3];
        correction.bands[0].apply(&mut expected);
        assert_eq!(dense[0], expected);
        assert!((dense[0][1][0] - Complex::new(1.0, 0.0)).norm() < 1e-12);
    }
}
