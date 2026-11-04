//! MSB-first bit reader and writer shared by the Salsa codecs.
//!
//! The E-AC-3, EMDF, JOC and OAMD code paths used to carry independent readers
//! (and two writers). Keeping one implementation makes the truncation,
//! bounds-check and variable-group rules identical everywhere.

#![forbid(unsafe_code)]

use std::fmt;

/// Bit-level failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitstreamError {
    /// The requested bits extend past the buffer or the configured limit.
    Truncated,
    /// A variable-group value kept its continuation bit past the group cap.
    TooManyGroups,
    /// The value does not fit in the requested number of bits.
    ValueTooLarge,
}

impl fmt::Display for BitstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("bitstream truncated"),
            Self::TooManyGroups => f.write_str("variable-bit extension groups exceeded"),
            Self::ValueTooLarge => f.write_str("value does not fit in the requested bits"),
        }
    }
}

impl std::error::Error for BitstreamError {}

/// Cursor over a byte buffer that reads bits most-significant first.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    size_bits: usize,
    position: usize,
    limit: usize,
    failed: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self::at(data, 0)
    }

    pub fn at(data: &'a [u8], start_bit: usize) -> Self {
        let size_bits = data.len() * 8;
        Self {
            data,
            size_bits,
            position: start_bit,
            limit: size_bits,
            failed: start_bit > size_bits,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    /// Total number of bits in the backing buffer.
    pub fn end(&self) -> usize {
        self.size_bits
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn remaining_bits(&self) -> usize {
        self.limit.min(self.size_bits).saturating_sub(self.position)
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Restrict reads to `limit_bits` (clamped to the buffer length).
    pub fn set_limit_bits(&mut self, limit_bits: usize) {
        self.limit = limit_bits.min(self.size_bits);
    }

    pub fn can_read(&self, count: usize) -> bool {
        !self.failed
            && count <= self.size_bits.saturating_sub(self.position)
            && count <= self.limit.saturating_sub(self.position)
    }

    /// Read `count` bits (at most 32) without failing.
    ///
    /// A read past the end sets [`is_failed`](Self::is_failed) and returns 0.
    pub fn read(&mut self, count: u32) -> u32 {
        match self.try_read(count) {
            Ok(value) => value,
            Err(_) => {
                self.failed = true;
                0
            }
        }
    }

    /// Read `count` bits (at most 32), reporting a truncated stream.
    pub fn try_read(&mut self, count: u32) -> Result<u32, BitstreamError> {
        if count > 32 {
            return Err(BitstreamError::ValueTooLarge);
        }
        if count == 0 {
            return Ok(0);
        }
        if !self.can_read(count as usize) {
            self.failed = true;
            return Err(BitstreamError::Truncated);
        }
        let mut value = 0u32;
        let mut remaining = count;
        if self.position.is_multiple_of(8) && remaining >= 8 {
            while remaining >= 8 {
                value = (value << 8) | u32::from(self.data[self.position / 8]);
                self.position += 8;
                remaining -= 8;
            }
        }
        while remaining > 0 {
            let bit = (self.data[self.position / 8] >> (7 - (self.position % 8))) & 1;
            value = (value << 1) | u32::from(bit);
            self.position += 1;
            remaining -= 1;
        }
        Ok(value)
    }

    pub fn read_bit(&mut self) -> bool {
        self.read(1) == 1
    }

    pub fn skip(&mut self, count: usize) -> bool {
        if !self.can_read(count) {
            self.failed = true;
            return false;
        }
        self.position += count;
        true
    }

    pub fn read_bytes(&mut self, count: usize) -> Option<Vec<u8>> {
        if !self.can_read(count.checked_mul(8)?) {
            self.failed = true;
            return None;
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.read(8) as u8);
        }
        Some(out)
    }

    /// `variable_bits()` as used by EMDF/OAMD: `max_groups` groups of `width`
    /// bits, each followed by a continuation bit.
    pub fn variable_bits(&mut self, width: u32, max_groups: u32) -> Result<u32, BitstreamError> {
        let mut value = 0u32;
        for group in 0..max_groups {
            value = value.wrapping_add(self.try_read(width)?);
            let more = self.try_read(1)? != 0;
            if !more {
                return Ok(value);
            }
            if group + 1 == max_groups {
                return Err(BitstreamError::TooManyGroups);
            }
            value = (value.wrapping_add(1)) << width;
        }
        Err(BitstreamError::TooManyGroups)
    }
}

/// MSB-first bit writer.
#[derive(Debug, Clone, Default)]
pub struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn bit(&mut self, bit: u32) {
        self.cur = (self.cur << 1) | (bit & 1) as u8;
        self.nbits += 1;
        if self.nbits == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    /// Write a boolean bit, matching the `bitstream-io` API used by the OAMD
    /// writer.
    pub fn write_bit(&mut self, value: bool) -> Result<(), BitstreamError> {
        self.bit(u32::from(value));
        Ok(())
    }

    /// Write the low `n` bits of `val`, MSB-first.
    pub fn write(&mut self, val: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((val >> i) & 1);
        }
    }

    /// Write the low `n` bits of `val`, checking that it fits.
    pub fn write_bits(&mut self, val: u32, n: u32) -> Result<(), BitstreamError> {
        if n > 32 || (n < 32 && val >= (1u32 << n)) {
            return Err(BitstreamError::ValueTooLarge);
        }
        self.write(val, n);
        Ok(())
    }

    /// Copy `nbits` MSB-first bits from `data`.
    pub fn copy_bits(&mut self, data: &[u8], nbits: usize) {
        for i in 0..nbits {
            self.bit(((data[i >> 3] >> (7 - (i & 7))) & 1) as u32);
        }
    }

    /// Append whole bytes.
    pub fn write_bytes(&mut self, data: &[u8]) -> Result<(), BitstreamError> {
        for byte in data {
            self.write(u32::from(*byte), 8);
        }
        Ok(())
    }

    /// `variable_bits` inverse: chunked base-2^n with one continuation bit per
    /// group.
    pub fn write_variable_bits(&mut self, value: u32, n: u32) -> Result<(), BitstreamError> {
        self.write_variable_bits_max(value, n, 32)
    }

    /// `variable_bits` inverse with an explicit group cap.
    pub fn write_variable_bits_max(
        &mut self,
        value: u32,
        n: u32,
        max_groups: u32,
    ) -> Result<(), BitstreamError> {
        if n == 0 || n > 31 || max_groups == 0 {
            return Err(BitstreamError::ValueTooLarge);
        }
        let base = 1u64 << n;
        // Find the smallest group count whose offset plus span covers `value`.
        let value = u64::from(value);
        let mut groups = 1u32;
        let mut offset = 0u64;
        let mut span = base;
        while value >= offset.saturating_add(span) {
            offset = offset.saturating_add(span);
            span = span.saturating_mul(base);
            groups += 1;
            if groups > max_groups {
                return Err(BitstreamError::ValueTooLarge);
            }
        }
        let mut remaining = value - offset;
        let mut digits = [0u64; 32];
        for index in (0..groups as usize).rev() {
            digits[index] = remaining % base;
            remaining /= base;
        }
        for (index, digit) in digits[..groups as usize].iter().enumerate() {
            self.write(*digit as u32, n);
            self.bit(u32::from(index + 1 != groups as usize));
        }
        Ok(())
    }

    /// Number of bits written so far.
    pub fn bit_len(&self) -> usize {
        self.out.len() * 8 + self.nbits as usize
    }

    /// Zero-pad the current byte.
    pub fn byte_align(&mut self) -> Result<(), BitstreamError> {
        if self.nbits != 0 {
            self.cur <<= 8 - self.nbits;
            self.out.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
        Ok(())
    }

    /// Byte-padded snapshot of the bits written so far, plus the exact bit count.
    pub fn snapshot(&self) -> (Vec<u8>, usize) {
        let nbits = self.bit_len();
        let mut bytes = self.out.clone();
        if self.nbits != 0 {
            bytes.push(self.cur << (8 - self.nbits));
        }
        (bytes, nbits)
    }

    /// Flush to bytes, zero-padding the final partial byte (MSB-aligned).
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.nbits != 0 {
            self.cur <<= 8 - self.nbits;
            self.out.push(self.cur);
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        let mut reader = BitReader::new(&[0b1010_0110, 0b0011_0000]);
        assert_eq!(reader.read(3), 0b101);
        assert_eq!(reader.read(5), 0b0_0110);
        assert_eq!(reader.read(8), 0b0011_0000);
        assert!(!reader.is_failed());
        assert_eq!(reader.try_read(1), Err(BitstreamError::Truncated));
        assert!(reader.is_failed());
    }

    #[test]
    fn limit_is_respected() {
        let mut reader = BitReader::new(&[0xff, 0xff]);
        reader.set_limit_bits(8);
        assert_eq!(reader.read(8), 0xff);
        assert!(!reader.can_read(1));
    }

    #[test]
    fn variable_bits_round_trip() {
        for value in [0, 1, 15, 16, 31, 255, 256, 4095, 4096, 65_535] {
            for width in [2, 4, 8] {
                let mut writer = BitWriter::new();
                writer.write_variable_bits(value, width).unwrap();
                let bytes = writer.into_bytes();
                let mut reader = BitReader::new(&bytes);
                assert_eq!(
                    reader.variable_bits(width, 8),
                    Ok(value),
                    "value {value}, width {width}"
                );
            }
        }
    }

    #[test]
    fn variable_bits_rejects_too_many_groups() {
        let mut reader = BitReader::new(&[0xff; 4]);
        assert_eq!(
            reader.variable_bits(2, 1),
            Err(BitstreamError::TooManyGroups)
        );
    }

    #[test]
    fn variable_bits_handles_the_full_u32_range() {
        let mut writer = BitWriter::new();
        writer.write_variable_bits_max(u32::MAX, 1, 32).unwrap();
        let bytes = writer.into_bytes();
        let mut reader = BitReader::new(&bytes);
        assert_eq!(reader.variable_bits(1, 32), Ok(u32::MAX));
    }

    #[test]
    fn writer_matches_known_layout() {
        let mut writer = BitWriter::new();
        writer.write_variable_bits(1, 5).unwrap();
        writer.write_variable_bits(2, 5).unwrap();
        let bytes = writer.into_bytes();
        let mut reader = BitReader::new(&bytes);
        assert_eq!(reader.variable_bits(5, 8), Ok(1));
        assert_eq!(reader.variable_bits(5, 8), Ok(2));
    }
}
