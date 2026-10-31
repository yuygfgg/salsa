//! MSB-first bit reader shared by the EMDF and JOC parsers.

use crate::error::Eac3JocError;

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
            failed: false,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    #[allow(dead_code)]
    pub fn remaining_bits(&self) -> usize {
        self.limit.saturating_sub(self.position)
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Restrict reads to `limit_bits` (clamped to the buffer length).
    pub fn set_limit_bits(&mut self, limit_bits: usize) {
        self.limit = limit_bits.min(self.size_bits);
    }

    pub fn can_read(&self, count: usize) -> bool {
        !self.failed && count <= self.limit && self.position <= self.limit - count
    }

    pub fn read(&mut self, mut count: u32) -> u32 {
        if count == 0 {
            return 0;
        }
        if !self.can_read(count as usize) {
            self.failed = true;
            return 0;
        }
        let mut value = 0u32;
        if self.position.is_multiple_of(8) && count >= 8 {
            while count >= 8 {
                value = (value << 8) | u32::from(self.data[self.position / 8]);
                self.position += 8;
                count -= 8;
            }
        }
        while count > 0 {
            let bit = (self.data[self.position / 8] >> (7 - (self.position % 8))) & 1;
            value = (value << 1) | u32::from(bit);
            self.position += 1;
            count -= 1;
        }
        value
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
        if !self.can_read(count * 8) {
            self.failed = true;
            return None;
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.read(8) as u8);
        }
        Some(out)
    }

    /// `variable_bits()` as used by EMDF: `max_groups` groups of `width` bits,
    /// each followed by a continuation bit.
    pub fn variable_bits(&mut self, width: u32, max_groups: u32) -> Result<u32, Eac3JocError> {
        let mut value = 0u32;
        for _ in 0..max_groups {
            value = value.wrapping_add(self.read(width));
            if self.failed {
                return Err(Eac3JocError::BitstreamTruncated);
            }
            let more = self.read(1);
            if self.failed {
                return Err(Eac3JocError::BitstreamTruncated);
            }
            if more == 0 {
                return Ok(value);
            }
            value = (value.wrapping_add(1)) << width;
        }
        Err(Eac3JocError::EmdfSyntax(
            "variable_bits extension groups exceeded".into(),
        ))
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
        let _ = reader.read(1);
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
    fn variable_bits_extends() {
        // 5-bit group 1, continuation 1, then 5-bit group 2, continuation 0.
        let bytes = [0x0c, 0x40];
        let mut reader = BitReader::new(&bytes);
        assert_eq!(reader.variable_bits(5, 8).unwrap(), (1 + 1) << 5 | 2);
    }
}
