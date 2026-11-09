//! Adaptive integer entropy models and the rANS stack coder.
//!
//! The codec uses `constriction` 0.5.0's [`DefaultAnsCoder`] (a stack coder)
//! with 24-bit probability precision. The encoder walks symbols in logical
//! order, records each symbol's integer interval from the *current* adaptive
//! model, and pushes the intervals into the stack coder in reverse order. The
//! decoder pops symbols in logical order and applies exactly the same integer
//! count updates.
//!
//! All probability arithmetic is integer arithmetic on the half-open interval
//! `[0, 2^24)`:
//!
//! * every legal symbol is assigned at least one unit,
//! * the frequencies are apportioned by largest remainder (ties by symbol
//!   index), and
//! * counts are rescaled deterministically (ceiling halving) when their total
//!   reaches 4096.
//!
//! The module deliberately keeps the rate model and the serialized model
//! identical: [`AdaptiveModel::probability`] and [`EntropyEncoder::cost_bits`]
//! both use the apportioned integer frequencies, never `count / sum(counts)`.

use core::num::NonZeroU32;

use constriction::stream::{Decode, Encode, stack::DefaultAnsCoder};

use crate::error::{Result, SpatialError};

/// Fixed-point probability precision used by the rANS coder.
pub const RANS_PRECISION: usize = 24;
/// Total probability mass of the rANS coder (`2^24`).
pub const RANS_TOTAL: u64 = 1 << RANS_PRECISION;
/// Count total at which models rescale.
pub const RESCALE_THRESHOLD: u64 = 4096;

/// Adaptive categorical model over `symbol_count` integer symbols.
///
/// The model stores only integer counts. The mapping to rANS frequencies is
/// recomputed from those counts on demand, so encoding, decoding and rate
/// estimation always agree exactly.
#[derive(Debug, Clone)]
pub struct AdaptiveModel {
    counts: Vec<u32>,
    total_counts: u64,
}

impl AdaptiveModel {
    /// Uniform nonzero prior over `symbol_count` symbols.
    pub fn uniform(symbol_count: usize) -> Self {
        assert!(symbol_count > 0, "entropy model needs a symbol");
        Self {
            counts: vec![1; symbol_count],
            total_counts: symbol_count as u64,
        }
    }

    /// A fixed zero-centred geometric prior.
    ///
    /// Counts decay with distance from `center`, which is normally the zero
    /// innovation symbol. The prior is deliberately kept small so that the
    /// first update cannot immediately trigger a rescale.
    pub fn geometric(symbol_count: usize, center: usize) -> Self {
        assert!(symbol_count > 0, "entropy model needs a symbol");
        let center = center.min(symbol_count - 1);
        const BASE: u32 = 6;
        let counts: Vec<u32> = (0..symbol_count)
            .map(|index| {
                let distance = index.abs_diff(center) as u32;
                (1u32 << BASE.saturating_sub(distance.min(BASE))).max(1)
            })
            .collect();
        let total_counts = counts.iter().map(|count| u64::from(*count)).sum();
        Self {
            counts,
            total_counts,
        }
    }

    /// A model with explicit nonzero integer counts.
    pub fn with_counts(counts: &[u32]) -> Self {
        assert!(!counts.is_empty(), "entropy model needs a symbol");
        let counts: Vec<u32> = counts.iter().map(|count| (*count).max(1)).collect();
        let total_counts = counts.iter().map(|count| u64::from(*count)).sum();
        Self {
            counts,
            total_counts,
        }
    }

    pub fn symbol_count(&self) -> usize {
        self.counts.len()
    }

    pub fn counts(&self) -> &[u32] {
        &self.counts
    }

    /// Update the model after observing `symbol` in logical order.
    ///
    /// The count is incremented first and the model is rescaled with
    /// deterministic ceiling halving when the total reaches
    /// [`RESCALE_THRESHOLD`].
    pub fn observe(&mut self, symbol: usize) {
        if symbol >= self.counts.len() {
            debug_assert!(false, "symbol outside entropy model support");
            return;
        }
        self.counts[symbol] = self.counts[symbol].saturating_add(1);
        self.total_counts += 1;
        if self.total_counts >= RESCALE_THRESHOLD {
            let mut total = 0u64;
            for count in &mut self.counts {
                *count = ((*count + 1) / 2).max(1);
                total += u64::from(*count);
            }
            self.total_counts = total;
        }
    }

    /// Integer rANS frequencies summing exactly to [`RANS_TOTAL`].
    ///
    /// Apportionment is deterministic; leftover units are distributed in
    /// symbol-index order, which also breaks ties by symbol index. The result always has a nonzero entry for every legal
    /// symbol.
    pub fn frequencies(&self) -> Vec<u32> {
        let mut frequencies = Vec::new();
        self.frequencies_into(&mut frequencies);
        frequencies
    }

    /// Fill `out` with the exact integer rANS frequencies.
    ///
    /// The scratch buffer lets the encoder avoid allocating once per coded
    /// symbol. Apportionment is deterministic; leftover units are distributed
    /// in symbol-index order, which also breaks ties by symbol index.
    pub fn frequencies_into(&self, out: &mut Vec<u32>) {
        let symbol_count = self.counts.len();
        out.clear();
        out.resize(symbol_count, 0);
        let total_counts = self.total_counts.max(1);
        let mut allocated = 0u64;

        for (index, &count) in self.counts.iter().enumerate() {
            let scaled = u64::from(count) * RANS_TOTAL;
            let frequency = (scaled / total_counts).max(1) as u32;
            out[index] = frequency;
            allocated += u64::from(frequency);
        }

        // The floor division loses less than one unit per symbol, so the
        // leftover is at most `symbol_count`. Distribute it in index order:
        // deterministic, integer-only, and O(symbol_count) per symbol.
        if allocated < RANS_TOTAL {
            let mut leftover = (RANS_TOTAL - allocated) as usize;
            let mut index = 0usize;
            while leftover > 0 {
                out[index] += 1;
                index += 1;
                if index == symbol_count {
                    index = 0;
                }
                leftover -= 1;
            }
        } else if allocated > RANS_TOTAL {
            let mut excess = (allocated - RANS_TOTAL) as usize;
            let mut index = symbol_count;
            while excess > 0 {
                index -= 1;
                if out[index] > 1 {
                    out[index] -= 1;
                    excess -= 1;
                }
                if index == 0 {
                    index = symbol_count;
                }
            }
        }
    }

    /// Probability of `symbol` under the exact integer CDF.
    pub fn probability(&self, symbol: usize) -> f64 {
        if symbol >= self.counts.len() {
            return 0.0;
        }
        let frequencies = self.frequencies();
        f64::from(frequencies[symbol]) / RANS_TOTAL as f64
    }

    /// Self-information of `symbol` in bits under the exact integer CDF.
    pub fn cost_bits(&self, symbol: usize) -> f64 {
        let probability = self.probability(symbol);
        if probability <= 0.0 {
            f64::INFINITY
        } else {
            -probability.log2()
        }
    }
}

/// Recorded integer probability interval for one symbol.
#[derive(Debug, Clone, Copy)]
struct Interval {
    start: u32,
    frequency: u32,
}

/// Adapter that feeds a pre-recorded interval to the rANS stack coder.
///
/// `constriction` only requires an interval for encoding, so no full CDF has
/// to be retained per symbol.
struct RecordedInterval {
    start: u32,
    frequency: u32,
}

impl constriction::stream::model::EntropyModel<RANS_PRECISION> for RecordedInterval {
    type Symbol = ();
    type Probability = u32;
}

impl constriction::stream::model::EncoderModel<RANS_PRECISION> for RecordedInterval {
    fn left_cumulative_and_probability(
        &self,
        _symbol: impl core::borrow::Borrow<Self::Symbol>,
    ) -> Option<(
        Self::Probability,
        <Self::Probability as constriction::BitArray>::NonZero,
    )> {
        NonZeroU32::new(self.frequency).map(|frequency| (self.start, frequency))
    }
}

impl constriction::stream::model::EntropyModel<RANS_PRECISION> for AdaptiveModel {
    type Symbol = usize;
    type Probability = u32;
}

impl constriction::stream::model::EncoderModel<RANS_PRECISION> for AdaptiveModel {
    fn left_cumulative_and_probability(
        &self,
        symbol: impl core::borrow::Borrow<Self::Symbol>,
    ) -> Option<(
        Self::Probability,
        <Self::Probability as constriction::BitArray>::NonZero,
    )> {
        let symbol = *symbol.borrow();
        if symbol >= self.counts.len() {
            return None;
        }
        let frequencies = self.frequencies();
        let start: u32 = frequencies[..symbol].iter().copied().sum();
        NonZeroU32::new(frequencies[symbol]).map(|frequency| (start, frequency))
    }
}

impl constriction::stream::model::DecoderModel<RANS_PRECISION> for AdaptiveModel {
    fn quantile_function(
        &self,
        quantile: Self::Probability,
    ) -> (
        Self::Symbol,
        Self::Probability,
        <Self::Probability as constriction::BitArray>::NonZero,
    ) {
        let frequencies = self.frequencies();
        let mut start = 0u32;
        for (symbol, &frequency) in frequencies.iter().enumerate() {
            let end = start.saturating_add(frequency);
            if quantile < end {
                return (
                    symbol,
                    start,
                    NonZeroU32::new(frequency).expect("every legal symbol is nonzero"),
                );
            }
            start = end;
        }
        // Numerical safety net: quantile is always `< 2^24` and frequencies sum
        // to `2^24`, so this is unreachable for a well-formed model.
        let last = frequencies.len() - 1;
        (
            last,
            start.saturating_sub(frequencies[last]),
            NonZeroU32::new(frequencies[last]).expect("every legal symbol is nonzero"),
        )
    }
}

/// Per-tile entropy contexts.
///
/// Every context listed in the design has its own adaptive model. The set is
/// reset at the start of each 100 ms tile so that no probability tables need to
/// be transmitted.
#[derive(Debug, Clone)]
pub struct EntropyContexts {
    pub activity_change: AdaptiveModel,
    pub activity_value: AdaptiveModel,
    pub base_update: AdaptiveModel,
    /// Base innovations, conditioned on the previous frequency band (`24` for
    /// the first band of an object).
    pub base_innovation: Vec<AdaptiveModel>,
    /// Fast innovations, conditioned on the DCT coefficient index.
    pub fast_innovation: Vec<AdaptiveModel>,
    pub correction_count: AdaptiveModel,
    pub correction_gap: AdaptiveModel,
    pub correction_value: AdaptiveModel,
    pub correlation_count: AdaptiveModel,
    pub correlation_present: AdaptiveModel,
    pub correlation_magnitude: Vec<AdaptiveModel>,
    pub correlation_phase: Vec<AdaptiveModel>,
    pub correlation_pair_a: AdaptiveModel,
    pub correlation_pair_b: AdaptiveModel,
    pub error_innovation: Vec<AdaptiveModel>,
    pub error_mask_change: AdaptiveModel,
    pub error_mask_value: AdaptiveModel,
    pub anchor_count: AdaptiveModel,
    pub anchor_time: AdaptiveModel,
    pub trajectory_update: AdaptiveModel,
    pub trajectory_count: AdaptiveModel,
    pub trajectory_time: AdaptiveModel,
    pub trajectory_position: AdaptiveModel,
    pub trajectory_position_delta: AdaptiveModel,
    pub snapshot_flag: AdaptiveModel,
    pub snapshot_bits: AdaptiveModel,
    pub snapshot_ell: AdaptiveModel,
    pub snapshot_base: AdaptiveModel,
    pub snapshot_fast: AdaptiveModel,
    pub snapshot_correction: AdaptiveModel,
    pub snapshot_error: AdaptiveModel,
    pub snapshot_trajectory: AdaptiveModel,
    pub snapshot_position_delta: AdaptiveModel,
    /// Shared binary model for escape tails.
    pub tail: AdaptiveModel,
}

/// Number of directly coded zigzag values before the escape symbol.
pub const DIRECT_INNOVATION_COUNT: usize = 32;

fn flag_model() -> AdaptiveModel {
    AdaptiveModel::with_counts(&[1, 1])
}

fn tail_model() -> AdaptiveModel {
    AdaptiveModel::with_counts(&[1, 1])
}

fn innovation_model() -> AdaptiveModel {
    AdaptiveModel::geometric(DIRECT_INNOVATION_COUNT + 1, 0)
}

impl EntropyContexts {
    /// Build fresh contexts for a scene with `object_count` objects.
    pub fn new(object_count: usize) -> Self {
        let max_pairs = core::cmp::min(8, object_count / 2);
        Self {
            activity_change: flag_model(),
            activity_value: flag_model(),
            base_update: flag_model(),
            base_innovation: (0..=crate::constants::PARAM_BAND_COUNT)
                .map(|_| innovation_model())
                .collect(),
            fast_innovation: (0..crate::constants::DCT_COEFF_COUNT)
                .map(|_| innovation_model())
                .collect(),
            correction_count: AdaptiveModel::uniform(crate::constants::PARAM_BAND_COUNT + 1),
            correction_gap: AdaptiveModel::uniform(crate::constants::PARAM_BAND_COUNT),
            correction_value: innovation_model(),
            correlation_count: AdaptiveModel::uniform(max_pairs + 1),
            correlation_present: flag_model(),
            correlation_magnitude: (0..crate::constants::PARAM_BAND_COUNT)
                .map(|_| innovation_model())
                .collect(),
            correlation_phase: (0..crate::constants::PARAM_BAND_COUNT)
                .map(|_| innovation_model())
                .collect(),
            correlation_pair_a: AdaptiveModel::uniform(object_count.max(1)),
            correlation_pair_b: AdaptiveModel::uniform(object_count.max(1)),
            error_innovation: (0..crate::constants::CORE_CHANNEL_COUNT)
                .map(|_| innovation_model())
                .collect(),
            error_mask_change: flag_model(),
            error_mask_value: flag_model(),
            anchor_count: innovation_model(),
            anchor_time: innovation_model(),
            trajectory_update: flag_model(),
            trajectory_count: innovation_model(),
            trajectory_time: innovation_model(),
            trajectory_position: flag_model(),
            trajectory_position_delta: innovation_model(),
            snapshot_flag: flag_model(),
            snapshot_bits: flag_model(),
            snapshot_ell: innovation_model(),
            snapshot_base: innovation_model(),
            snapshot_fast: innovation_model(),
            snapshot_correction: innovation_model(),
            snapshot_error: innovation_model(),
            snapshot_trajectory: innovation_model(),
            snapshot_position_delta: innovation_model(),
            tail: tail_model(),
        }
    }

    /// Reset every context to its tile-start prior.
    pub fn reset_tile(&mut self) {
        *self = Self::new(self.correlation_pair_a.symbol_count());
    }
}

/// rANS stack encoder that records logical-order intervals and emits reversed.
#[derive(Debug, Default)]
pub struct EntropyEncoder {
    intervals: Vec<Interval>,
    estimated_bits: f64,
    frequencies: Vec<u32>,
}

impl EntropyEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the interval for `symbol`, then update the adaptive counts.
    pub fn encode(&mut self, model: &mut AdaptiveModel, symbol: usize) -> Result<()> {
        if symbol >= model.symbol_count() {
            return Err(SpatialError::Syntax("entropy symbol out of range"));
        }
        model.frequencies_into(&mut self.frequencies);
        let mut start = 0u32;
        for value in &self.frequencies[..symbol] {
            start += *value;
        }
        let frequency = self.frequencies[symbol];
        debug_assert!(frequency > 0);
        self.intervals.push(Interval { start, frequency });
        self.estimated_bits += (RANS_TOTAL as f64 / f64::from(frequency)).log2();
        model.observe(symbol);
        Ok(())
    }

    pub fn cost_bits(model: &AdaptiveModel, symbol: usize) -> f64 {
        model.cost_bits(symbol)
    }

    /// Number of symbols recorded so far.
    pub fn len(&self) -> usize {
        self.intervals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    /// Estimated payload bits from the recorded integer intervals.
    pub fn estimated_bits(&self) -> f64 {
        self.estimated_bits
    }

    /// Estimated total bits including the byte framing header and the fixed
    /// one-bit rANS start marker.
    pub fn estimated_total_bits(&self) -> f64 {
        if self.intervals.is_empty() {
            0.0
        } else {
            self.estimated_bits + 8.0 * FRAMING_BYTES as f64 + 1.0
        }
    }

    /// Encode a nonnegative integer with an entropy-coded unary length and
    /// binary payload. `model` must be a binary adaptive model.
    pub fn encode_unsigned(&mut self, value: u64, model: &mut AdaptiveModel) -> Result<()> {
        if model.symbol_count() != 2 {
            return Err(SpatialError::Syntax("unsigned tail model must be binary"));
        }
        let bit_count = 64 - value.leading_zeros();
        for _ in 0..bit_count {
            self.encode(model, 1)?;
        }
        self.encode(model, 0)?;
        for bit in (0..bit_count.saturating_sub(1)).rev() {
            self.encode(model, ((value >> bit) & 1) as usize)?;
        }
        Ok(())
    }

    /// Encode an unsigned value, using `direct` direct categorical symbols and
    /// an escape symbol followed by an entropy-coded tail.
    pub fn encode_unsigned_escape(
        &mut self,
        value: u64,
        direct: usize,
        model: &mut AdaptiveModel,
        tail: &mut AdaptiveModel,
    ) -> Result<()> {
        if model.symbol_count() != direct + 1 {
            return Err(SpatialError::Syntax("unsigned escape model size mismatch"));
        }
        if (value as u128) < direct as u128 {
            self.encode(model, value as usize)
        } else {
            self.encode(model, direct)?;
            self.encode_unsigned(value - direct as u64, tail)
        }
    }

    /// Encode a signed value with the zero-centred zigzag mapping, `direct`
    /// direct symbols and an escape tail.
    pub fn encode_signed_escape(
        &mut self,
        value: i64,
        direct: usize,
        model: &mut AdaptiveModel,
        tail: &mut AdaptiveModel,
    ) -> Result<()> {
        let zigzag = zigzag_encode(value);
        if model.symbol_count() != direct + 1 {
            return Err(SpatialError::Syntax("signed escape model size mismatch"));
        }
        if zigzag < direct as u64 {
            self.encode(model, zigzag as usize)
        } else {
            self.encode(model, direct)?;
            self.encode_unsigned(zigzag - direct as u64, tail)
        }
    }

    pub fn finish(self) -> Vec<u8> {
        if self.intervals.is_empty() {
            return Vec::new();
        }

        let mut coder = DefaultAnsCoder::new();
        // Seed the stack with a fixed symbol whose interval has a nonzero left
        // cumulative. Without this, an all-zero symbol sequence leaves the
        // rANS state at zero and `constriction` would emit an empty payload
        // that cannot be distinguished from "no symbols". The marker is
        // decoded and verified by `EntropyDecoder::finish`.
        coder
            .encode_symbol((), START_MARKER)
            .expect("start marker is always encodable");
        for interval in self.intervals.iter().rev() {
            let model = RecordedInterval {
                start: interval.start,
                frequency: interval.frequency,
            };
            coder
                .encode_symbol((), model)
                .expect("recorded intervals are always encodable");
        }
        let words = match coder.into_compressed() {
            Ok(words) => words,
            Err(never) => match never {},
        };
        let mut bytes = Vec::with_capacity(FRAMING_BYTES + words.len() * 4);
        bytes.extend_from_slice(&(words.len() as u32).to_le_bytes());
        for word in words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }
}

const FRAMING_BYTES: usize = 4;

/// Fixed rANS start marker: symbol 1 of a `[1, 1]` binary model.
const START_MARKER: RecordedInterval = RecordedInterval {
    start: RANS_TOTAL as u32 / 2,
    frequency: RANS_TOTAL as u32 / 2,
};

/// rANS stack decoder.
pub struct EntropyDecoder<'a> {
    coder: Option<DefaultAnsCoder>,
    _bytes: &'a [u8],
}

impl<'a> EntropyDecoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self {
                coder: None,
                _bytes: bytes,
            });
        }
        if bytes.len() < FRAMING_BYTES {
            return Err(SpatialError::Truncated);
        }
        let word_count = u32::from_le_bytes(
            bytes[..FRAMING_BYTES]
                .try_into()
                .map_err(|_| SpatialError::Truncated)?,
        ) as usize;
        let expected = FRAMING_BYTES
            .checked_add(word_count.checked_mul(4).ok_or(SpatialError::Truncated)?)
            .ok_or(SpatialError::Truncated)?;
        if bytes.len() < expected {
            return Err(SpatialError::Truncated);
        }
        if word_count == 0 {
            return Ok(Self {
                coder: None,
                _bytes: bytes,
            });
        }
        let mut words = Vec::with_capacity(word_count);
        for chunk in bytes[FRAMING_BYTES..expected].chunks_exact(4) {
            words.push(u32::from_le_bytes(
                chunk.try_into().map_err(|_| SpatialError::Truncated)?,
            ));
        }
        let coder =
            DefaultAnsCoder::from_compressed(words).map_err(|_| SpatialError::RansTruncated)?;
        Ok(Self {
            coder: Some(coder),
            _bytes: bytes,
        })
    }

    /// Decode one symbol from the current model, then update the counts.
    pub fn decode(&mut self, model: &mut AdaptiveModel) -> Result<usize> {
        let coder = self.coder.as_mut().ok_or(SpatialError::RansTruncated)?;
        let symbol: usize = coder
            .decode_symbol(&*model)
            .map_err(|_| SpatialError::RansTruncated)?;
        model.observe(symbol);
        Ok(symbol)
    }

    /// Decode a nonnegative integer written by
    /// [`EntropyEncoder::encode_unsigned`].
    pub fn decode_unsigned(&mut self, model: &mut AdaptiveModel) -> Result<u64> {
        if model.symbol_count() != 2 {
            return Err(SpatialError::Syntax("unsigned tail model must be binary"));
        }
        let mut bit_count = 0u32;
        while self.decode(model)? == 1 {
            bit_count += 1;
            if bit_count > 64 {
                return Err(SpatialError::Syntax("unsigned tail too long"));
            }
        }
        let mut value = 0u64;
        for _ in 0..bit_count.saturating_sub(1) {
            value = (value << 1) | self.decode(model)? as u64;
        }
        if bit_count > 0 {
            value |= 1u64 << (bit_count - 1);
        }
        Ok(value)
    }

    pub fn decode_unsigned_escape(
        &mut self,
        direct: usize,
        model: &mut AdaptiveModel,
        tail: &mut AdaptiveModel,
    ) -> Result<u64> {
        if model.symbol_count() != direct + 1 {
            return Err(SpatialError::Syntax("unsigned escape model size mismatch"));
        }
        let symbol = self.decode(model)?;
        if symbol == direct {
            Ok(direct as u64 + self.decode_unsigned(tail)?)
        } else {
            Ok(symbol as u64)
        }
    }

    pub fn decode_signed_escape(
        &mut self,
        direct: usize,
        model: &mut AdaptiveModel,
        tail: &mut AdaptiveModel,
    ) -> Result<i64> {
        if model.symbol_count() != direct + 1 {
            return Err(SpatialError::Syntax("signed escape model size mismatch"));
        }
        let symbol = self.decode(model)?;
        let zigzag = if symbol == direct {
            direct as u64 + self.decode_unsigned(tail)?
        } else {
            symbol as u64
        };
        Ok(zigzag_decode(zigzag))
    }

    pub fn finish(mut self) -> Result<()> {
        if let Some(coder) = self.coder.as_mut() {
            let marker_model = AdaptiveModel::with_counts(&[1, 1]);
            let marker: usize = coder
                .decode_symbol(&marker_model)
                .map_err(|_| SpatialError::RansTruncated)?;
            if marker != 1 {
                return Err(SpatialError::Syntax("invalid rANS start marker"));
            }
            if !coder.is_empty() {
                return Err(SpatialError::Syntax("trailing rANS data"));
            }
        }
        Ok(())
    }
}

/// Zigzag-map a signed value onto the nonnegative integers.
pub fn zigzag_encode(value: i64) -> u64 {
    ((value.wrapping_shl(1)) ^ (value >> 63)) as u64
}

/// Inverse of [`zigzag_encode`].
pub fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(symbols: &[usize], mut model: AdaptiveModel) -> Vec<usize> {
        let mut encoder = EntropyEncoder::new();
        let mut encode_model = model.clone();
        for &symbol in symbols {
            encoder.encode(&mut encode_model, symbol).unwrap();
        }
        let bytes = encoder.finish();
        let mut decoder = EntropyDecoder::new(&bytes).unwrap();
        let decoded = symbols
            .iter()
            .map(|_| decoder.decode(&mut model).unwrap())
            .collect::<Vec<_>>();
        decoder.finish().unwrap();
        assert_eq!(decoded, symbols);
        decoded
    }

    #[test]
    fn forward_adaptation_reverse_encoding_symmetry() {
        let symbols = [4, 4, 3, 9, 0, 0, 1, 7, 2, 2, 2, 31, 5];
        roundtrip(&symbols, AdaptiveModel::geometric(33, 0));
    }

    #[test]
    fn all_zero_sequence() {
        let symbols = vec![0usize; 200];
        roundtrip(&symbols, AdaptiveModel::geometric(33, 0));
    }

    #[test]
    fn escape_path_roundtrips_large_and_signed_values() {
        let values = [0i64, -1, 1, -20, 20, 1000, -1000, 123456, -9];
        let mut encoder = EntropyEncoder::new();
        let mut encode_model = AdaptiveModel::geometric(DIRECT_INNOVATION_COUNT + 1, 0);
        let mut encode_tail = AdaptiveModel::uniform(2);
        for &value in &values {
            encoder
                .encode_signed_escape(
                    value,
                    DIRECT_INNOVATION_COUNT,
                    &mut encode_model,
                    &mut encode_tail,
                )
                .unwrap();
        }
        let bytes = encoder.finish();

        let mut decode_model = AdaptiveModel::geometric(DIRECT_INNOVATION_COUNT + 1, 0);
        let mut decode_tail = AdaptiveModel::uniform(2);
        let mut decoder = EntropyDecoder::new(&bytes).unwrap();
        for &expected in &values {
            let value = decoder
                .decode_signed_escape(DIRECT_INNOVATION_COUNT, &mut decode_model, &mut decode_tail)
                .unwrap();
            assert_eq!(value, expected);
        }
        decoder.finish().unwrap();
    }

    #[test]
    fn escape_path_roundtrips_unsigned_values() {
        let values = [0u64, 1, 31, 32, 33, 1000, 1 << 40];
        let mut encoder = EntropyEncoder::new();
        let mut model = AdaptiveModel::geometric(5, 0);
        let mut tail = AdaptiveModel::uniform(2);
        for &value in &values {
            encoder
                .encode_unsigned_escape(value, 4, &mut model, &mut tail)
                .unwrap();
        }
        let bytes = encoder.finish();

        let mut model = AdaptiveModel::geometric(5, 0);
        let mut tail = AdaptiveModel::uniform(2);
        let mut decoder = EntropyDecoder::new(&bytes).unwrap();
        for &expected in &values {
            assert_eq!(
                decoder
                    .decode_unsigned_escape(4, &mut model, &mut tail)
                    .unwrap(),
                expected
            );
        }
        decoder.finish().unwrap();
    }

    #[test]
    fn counts_rescale_deterministically() {
        let mut model = AdaptiveModel::uniform(3);
        for _ in 0..5000 {
            model.observe(1);
        }
        let total: u64 = model.counts().iter().map(|count| u64::from(*count)).sum();
        assert!(total < RESCALE_THRESHOLD);
        assert!(model.counts().iter().all(|count| *count >= 1));

        let mut clone = model.clone();
        for _ in 0..10 {
            clone.observe(2);
        }
        let mut again = model.clone();
        for _ in 0..10 {
            again.observe(2);
        }
        assert_eq!(clone.counts(), again.counts());

        // The rescaled model still round-trips.
        roundtrip(&[1, 1, 2, 0, 1], model);
    }

    #[test]
    fn frequencies_sum_to_rans_total_and_are_nonzero() {
        let models = [
            AdaptiveModel::uniform(1),
            AdaptiveModel::uniform(7),
            AdaptiveModel::geometric(33, 0),
            AdaptiveModel::with_counts(&[1, 1000, 2, 4095]),
        ];
        for model in models {
            let frequencies = model.frequencies();
            assert_eq!(
                frequencies.iter().map(|f| u64::from(*f)).sum::<u64>(),
                RANS_TOTAL
            );
            assert!(frequencies.iter().all(|frequency| *frequency > 0));
            for (symbol, _) in frequencies.iter().enumerate() {
                let expected = f64::from(frequencies[symbol]) / RANS_TOTAL as f64;
                assert_eq!(model.probability(symbol), expected);
                assert_eq!(model.cost_bits(symbol), -expected.log2());
            }
        }
    }

    #[test]
    fn truncated_data_is_rejected() {
        let mut encoder = EntropyEncoder::new();
        let mut model = AdaptiveModel::uniform(4);
        for symbol in [0usize, 1, 2, 3, 3, 2, 1, 0] {
            encoder.encode(&mut model, symbol).unwrap();
        }
        let bytes = encoder.finish();

        assert!(EntropyDecoder::new(&bytes[..2]).is_err());
        assert!(EntropyDecoder::new(&bytes[..bytes.len() - 2]).is_err());

        let truncated = &bytes[..bytes.len() - 1];
        if let Ok(mut decoder) = EntropyDecoder::new(truncated) {
            assert!(decoder.decode(&mut AdaptiveModel::uniform(4)).is_err());
        }
    }

    #[test]
    fn empty_sequence_has_empty_payload() {
        let encoder = EntropyEncoder::new();
        assert!(encoder.finish().is_empty());
        let mut decoder = EntropyDecoder::new(&[]).unwrap();
        assert!(decoder.decode(&mut AdaptiveModel::uniform(2)).is_err());
        decoder.finish().unwrap();
    }

    #[test]
    fn context_reset_restores_priors() {
        let mut contexts = EntropyContexts::new(16);
        let before = contexts.activity_change.counts().to_vec();
        contexts.activity_change.observe(1);
        contexts.tail.observe(1);
        assert_ne!(contexts.activity_change.counts(), before.as_slice());
        contexts.reset_tile();
        assert_eq!(contexts.activity_change.counts(), before.as_slice());
        assert_eq!(contexts.correlation_pair_a.symbol_count(), 16);
        assert_eq!(contexts.correlation_count.symbol_count(), 9);
    }

    #[test]
    fn cost_estimate_matches_recorded_intervals() {
        let mut encoder = EntropyEncoder::new();
        let mut model = AdaptiveModel::geometric(17, 0);
        let mut expected = 0.0;
        for symbol in [0usize, 1, 2, 3, 3, 2, 1, 0, 8, 15, 16] {
            expected += EntropyEncoder::cost_bits(&model, symbol);
            encoder.encode(&mut model, symbol).unwrap();
        }
        assert!((encoder.estimated_bits() - expected).abs() < 1.0e-12);
    }
}
