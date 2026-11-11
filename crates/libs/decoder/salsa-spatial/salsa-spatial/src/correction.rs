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
//! directions from the SVD, so the decoder applies the plain outer product.
//! Components use a signed four-bit complex grid. The complete optional
//! correction payload is entropy coded with adaptive models and appended after
//! the rANS parameter payload as a length-delimited trailer.

use num_complex::Complex;

use crate::constants::{CORE_CHANNEL_COUNT, MAX_OBJECTS, PARAM_BAND_COUNT};
use crate::entropy::{AdaptiveModel, EntropyDecoder, EntropyEncoder};
use crate::error::{Result, SpatialError};

/// Trailer magic written after the rANS payload of a corrected tile.
pub const CORRECTION_TRAILER_MAGIC: u32 = 0x5343_5254; // "SCRT"
/// Correction payload version.
pub const CORRECTION_VERSION: u8 = 4;
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

struct CorrectionModels {
    keyframe_count: AdaptiveModel,
    band_present: AdaptiveModel,
    rank: AdaptiveModel,
    per_keyframe_rank: AdaptiveModel,
    shared: AdaptiveModel,
    support_len: AdaptiveModel,
    support_first: AdaptiveModel,
    support_gap: AdaptiveModel,
    left: AdaptiveModel,
    right: AdaptiveModel,
    scale_sign: AdaptiveModel,
    scale_exponent: AdaptiveModel,
    scale_mantissa_low: AdaptiveModel,
    scale_mantissa_mid: AdaptiveModel,
    scale_mantissa_high: AdaptiveModel,
}

impl CorrectionModels {
    fn new() -> Self {
        Self {
            keyframe_count: AdaptiveModel::uniform(CORRECTION_MAX_KEYFRAMES + 1),
            band_present: AdaptiveModel::with_counts(&[1, 1]),
            rank: AdaptiveModel::uniform(CORRECTION_MAX_RANK + 1),
            per_keyframe_rank: AdaptiveModel::with_counts(&[1, 1]),
            shared: AdaptiveModel::with_counts(&[1, 1]),
            support_len: AdaptiveModel::uniform(CORRECTION_MAX_SUPPORT + 1),
            support_first: AdaptiveModel::uniform(MAX_OBJECTS),
            support_gap: AdaptiveModel::uniform(MAX_OBJECTS),
            left: AdaptiveModel::uniform(15),
            right: AdaptiveModel::uniform(15),
            scale_sign: AdaptiveModel::with_counts(&[1, 1]),
            scale_exponent: AdaptiveModel::uniform(256),
            scale_mantissa_low: AdaptiveModel::uniform(256),
            scale_mantissa_mid: AdaptiveModel::uniform(256),
            scale_mantissa_high: AdaptiveModel::uniform(128),
        }
    }
}

fn encode_nibble(encoder: &mut EntropyEncoder, model: &mut AdaptiveModel, value: Complex<f64>) {
    let real = ((value.re * 7.0).round().clamp(-7.0, 7.0) as i32 + 7) as usize;
    let imag = ((value.im * 7.0).round().clamp(-7.0, 7.0) as i32 + 7) as usize;
    encoder
        .encode(model, real)
        .expect("correction nibble is encodable");
    encoder
        .encode(model, imag)
        .expect("correction nibble is encodable");
}

fn decode_nibble(decoder: &mut EntropyDecoder<'_>, model: &mut AdaptiveModel) -> Complex<f64> {
    let real = decoder.decode(model).expect("correction nibble") as i32 - 7;
    let imag = decoder.decode(model).expect("correction nibble") as i32 - 7;
    Complex::new(f64::from(real) / 7.0, f64::from(imag) / 7.0)
}

fn encode_scale(encoder: &mut EntropyEncoder, models: &mut CorrectionModels, value: f32) {
    let bits = value.to_bits();
    encoder
        .encode(&mut models.scale_sign, ((bits >> 31) & 1) as usize)
        .expect("correction scale sign");
    encoder
        .encode(&mut models.scale_exponent, ((bits >> 23) & 0xff) as usize)
        .expect("correction scale exponent");
    encoder
        .encode(&mut models.scale_mantissa_low, (bits & 0xff) as usize)
        .expect("correction scale mantissa");
    encoder
        .encode(
            &mut models.scale_mantissa_mid,
            ((bits >> 8) & 0xff) as usize,
        )
        .expect("correction scale mantissa");
    encoder
        .encode(
            &mut models.scale_mantissa_high,
            ((bits >> 16) & 0x7f) as usize,
        )
        .expect("correction scale mantissa");
}

fn decode_scale(decoder: &mut EntropyDecoder<'_>, models: &mut CorrectionModels) -> f32 {
    let sign = decoder.decode(&mut models.scale_sign).expect("scale sign") as u32;
    let exponent = decoder
        .decode(&mut models.scale_exponent)
        .expect("scale exponent") as u32;
    let low = decoder
        .decode(&mut models.scale_mantissa_low)
        .expect("scale mantissa") as u32;
    let mid = decoder
        .decode(&mut models.scale_mantissa_mid)
        .expect("scale mantissa") as u32;
    let high = decoder
        .decode(&mut models.scale_mantissa_high)
        .expect("scale mantissa") as u32;
    let bits = (sign << 31) | (exponent << 23) | (high << 16) | (mid << 8) | low;
    f32::from_bits(bits)
}

fn encode_support_left(
    encoder: &mut EntropyEncoder,
    models: &mut CorrectionModels,
    component: &CorrectionComponent,
) {
    let length = component
        .support
        .len()
        .min(component.left.len())
        .min(CORRECTION_MAX_SUPPORT);
    encoder
        .encode(&mut models.support_len, length)
        .expect("correction support length");
    let mut previous: Option<u16> = None;
    for &object in component.support.iter().take(length) {
        let object = object.min((MAX_OBJECTS - 1) as u16);
        let symbol = if let Some(previous) = previous {
            usize::from(object.saturating_sub(previous + 1))
        } else {
            usize::from(object)
        };
        encoder
            .encode(
                if previous.is_some() {
                    &mut models.support_gap
                } else {
                    &mut models.support_first
                },
                symbol,
            )
            .expect("correction support symbol");
        previous = Some(object);
    }
    let model = &mut models.left;
    for value in component.left.iter().take(length) {
        encode_nibble(encoder, model, *value);
    }
    encode_scale(encoder, models, component.left_scale as f32);
}

fn decode_support_left(
    decoder: &mut EntropyDecoder<'_>,
    models: &mut CorrectionModels,
) -> Result<(Vec<u16>, Vec<Complex<f64>>, f64)> {
    let length = decoder.decode(&mut models.support_len)?;
    if length > CORRECTION_MAX_SUPPORT {
        return Err(SpatialError::Parameter("correction support too large"));
    }
    let mut support = Vec::with_capacity(length);
    let mut previous: Option<u16> = None;
    for _ in 0..length {
        let symbol = decoder.decode(if previous.is_some() {
            &mut models.support_gap
        } else {
            &mut models.support_first
        })?;
        let object = if let Some(previous) = previous {
            previous
                .checked_add(1)
                .and_then(|value| value.checked_add(symbol as u16))
                .ok_or(SpatialError::Parameter("correction support overflow"))?
        } else {
            symbol as u16
        };
        if usize::from(object) >= MAX_OBJECTS {
            return Err(SpatialError::Parameter("correction object out of range"));
        }
        if support.last().is_some_and(|&previous| object <= previous) {
            return Err(SpatialError::Parameter("correction support is not sorted"));
        }
        support.push(object);
        previous = Some(object);
    }
    let mut left = Vec::with_capacity(length);
    for _ in 0..length {
        left.push(decode_nibble(decoder, &mut models.left));
    }
    let left_scale = f64::from(decode_scale(decoder, models));
    if !left_scale.is_finite() {
        return Err(SpatialError::Parameter("non-finite correction scale"));
    }
    Ok((support, left, left_scale))
}

fn encode_right(
    encoder: &mut EntropyEncoder,
    models: &mut CorrectionModels,
    component: &CorrectionComponent,
) {
    let model = &mut models.right;
    for value in &component.right {
        encode_nibble(encoder, model, *value);
    }
    encode_scale(encoder, models, component.right_scale as f32);
}

fn decode_right(
    decoder: &mut EntropyDecoder<'_>,
    models: &mut CorrectionModels,
) -> Result<([Complex<f64>; CORE_CHANNEL_COUNT], f64)> {
    let mut right = [Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT];
    for value in &mut right {
        *value = decode_nibble(decoder, &mut models.right);
    }
    let right_scale = f64::from(decode_scale(decoder, models));
    if !right_scale.is_finite() {
        return Err(SpatialError::Parameter("non-finite correction scale"));
    }
    Ok((right, right_scale))
}

fn encode_component(
    encoder: &mut EntropyEncoder,
    models: &mut CorrectionModels,
    component: &CorrectionComponent,
) {
    encode_support_left(encoder, models, component);
    encode_right(encoder, models, component);
}

fn decode_component(
    decoder: &mut EntropyDecoder<'_>,
    models: &mut CorrectionModels,
) -> Result<CorrectionComponent> {
    let (support, left, left_scale) = decode_support_left(decoder, models)?;
    let (right, right_scale) = decode_right(decoder, models)?;
    Ok(CorrectionComponent {
        support,
        left,
        right,
        left_scale,
        right_scale,
    })
}

/// Serialize a list of per-tile correction keyframes, including the trailer.
///
/// Support, nibble payloads, and scales are entropy coded. A band's support,
/// left factor, and left scale are shared across keyframes when they match.
pub fn encode_keyframes(corrections: &[TileCorrection]) -> Vec<u8> {
    let count = corrections.len().min(CORRECTION_MAX_KEYFRAMES);
    let corrections = &corrections[..count];
    let mut models = CorrectionModels::new();
    let mut encoder = EntropyEncoder::new();
    encoder
        .encode(&mut models.keyframe_count, count)
        .expect("correction keyframe count");
    for band in 0..PARAM_BAND_COUNT {
        let present = corrections
            .iter()
            .any(|correction| !correction.bands[band].is_empty());
        encoder
            .encode(&mut models.band_present, usize::from(present))
            .expect("correction band flag");
        if !present {
            continue;
        }
        let rank = corrections[0].bands[band]
            .components
            .len()
            .min(CORRECTION_MAX_RANK);
        let same_rank = corrections.iter().all(|correction| {
            correction.bands[band]
                .components
                .len()
                .min(CORRECTION_MAX_RANK)
                == rank
        });
        encoder
            .encode(&mut models.per_keyframe_rank, usize::from(!same_rank))
            .expect("correction rank mode");
        if same_rank {
            encoder
                .encode(&mut models.rank, rank)
                .expect("correction rank");
            for component in 0..rank {
                let first = &corrections[0].bands[band].components[component];
                let shared = corrections.iter().all(|correction| {
                    let other = &correction.bands[band].components[component];
                    first.support == other.support
                        && first.left == other.left
                        && first.left_scale == other.left_scale
                });
                encoder
                    .encode(&mut models.shared, usize::from(shared))
                    .expect("correction sharing flag");
                if shared {
                    encode_support_left(&mut encoder, &mut models, first);
                }
                for correction in corrections {
                    let current = &correction.bands[band].components[component];
                    if !shared {
                        encode_support_left(&mut encoder, &mut models, current);
                    }
                    encode_right(&mut encoder, &mut models, current);
                }
            }
        } else {
            for correction in corrections {
                let components = &correction.bands[band].components;
                let rank = components.len().min(CORRECTION_MAX_RANK);
                encoder
                    .encode(&mut models.rank, rank)
                    .expect("correction rank");
                for component in components.iter().take(rank) {
                    encode_component(&mut encoder, &mut models, component);
                }
            }
        }
    }
    let stream = encoder.finish();
    let mut payload = Vec::with_capacity(1 + stream.len());
    payload.push(CORRECTION_VERSION);
    payload.extend_from_slice(&stream);
    finish_payload(payload)
}

fn finish_payload(mut payload: Vec<u8>) -> Vec<u8> {
    let length = payload.len() as u32;
    payload.extend_from_slice(&length.to_le_bytes());
    payload.extend_from_slice(&CORRECTION_TRAILER_MAGIC.to_le_bytes());
    payload
}

/// Parse every optional correction keyframe from a complete tile payload.
///
/// The rANS parameter payload comes first and is ignored here; corrections are
/// present only when the final eight bytes are `[payload length][magic]`.
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
    let Some((&version, stream)) = bytes.split_first() else {
        return Err(SpatialError::Truncated);
    };
    if version != CORRECTION_VERSION {
        return Err(SpatialError::UnsupportedVersion {
            version: u16::from(version),
        });
    }
    let mut models = CorrectionModels::new();
    let mut decoder = EntropyDecoder::new(stream)?;
    let count = decoder.decode(&mut models.keyframe_count)?;
    if count > CORRECTION_MAX_KEYFRAMES {
        return Err(SpatialError::Parameter("too many correction keyframes"));
    }
    let mut corrections = vec![TileCorrection::default(); count];
    for band in 0..PARAM_BAND_COUNT {
        let present = decoder.decode(&mut models.band_present)?;
        if present == 0 {
            continue;
        }
        if present > 1 {
            return Err(SpatialError::Parameter("invalid correction band flag"));
        }
        let per_keyframe_rank = decoder.decode(&mut models.per_keyframe_rank)? != 0;
        if per_keyframe_rank {
            for correction in &mut corrections {
                let rank = decoder.decode(&mut models.rank)?;
                if rank > CORRECTION_MAX_RANK {
                    return Err(SpatialError::Parameter("correction rank out of range"));
                }
                for _ in 0..rank {
                    correction.bands[band]
                        .components
                        .push(decode_component(&mut decoder, &mut models)?);
                }
            }
            continue;
        }
        let rank = decoder.decode(&mut models.rank)?;
        if rank > CORRECTION_MAX_RANK {
            return Err(SpatialError::Parameter("correction rank out of range"));
        }
        for _ in 0..rank {
            let shared = decoder.decode(&mut models.shared)? != 0;
            let shared_data = if shared {
                Some(decode_support_left(&mut decoder, &mut models)?)
            } else {
                None
            };
            for correction in &mut corrections {
                let (support, left, left_scale) =
                    if let Some((support, left, left_scale)) = &shared_data {
                        (support.clone(), left.clone(), *left_scale)
                    } else {
                        decode_support_left(&mut decoder, &mut models)?
                    };
                let (right, right_scale) = decode_right(&mut decoder, &mut models)?;
                correction.bands[band].components.push(CorrectionComponent {
                    support,
                    left,
                    right,
                    left_scale,
                    right_scale,
                });
            }
        }
    }
    decoder.finish()?;
    Ok(corrections)
}

/// Quantize one complex value onto the signed four-bit grid.
pub fn quantize_complex(value: Complex<f64>) -> Complex<f64> {
    Complex::new(quantize_real(value.re), quantize_real(value.im))
}

fn quantize_real(value: f64) -> f64 {
    let quantized = (value * 7.0).round().clamp(-7.0, 7.0) as i8;
    f64::from(quantized) / 7.0
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
            left_scale: 0.123_456_789,
            right_scale: 2.0,
        });
        let mut bytes = vec![9, 8, 7, 6];
        bytes.extend(correction.encode());
        let decoded = decode_trailing_correction(&bytes).unwrap().unwrap();
        assert_eq!(decoded.bands[3].components.len(), 1);
        let component = &decoded.bands[3].components[0];
        assert_eq!(component.support, vec![1, 7, 42]);
        assert_eq!(component.left_scale, f64::from(0.123_456_79_f32));
        assert_eq!(component.right_scale, 2.0);
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
