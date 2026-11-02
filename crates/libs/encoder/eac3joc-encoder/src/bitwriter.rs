// SPDX-License-Identifier: MPL-2.0
//! MSB-first bit writer for EMDF, OAMD and JOC payloads.

/// MSB-first bit writer.
pub(crate) struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self {
            out: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    #[inline]
    pub(crate) fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b & 1) as u8;
        self.nbits += 1;
        if self.nbits == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    /// Write the low `n` bits of `val`, MSB-first.
    pub(crate) fn write(&mut self, val: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((val >> i) & 1);
        }
    }

    /// Copy `nbits` MSB-first bits from `data`.
    pub(crate) fn copy_bits(&mut self, data: &[u8], nbits: usize) {
        for i in 0..nbits {
            self.bit(((data[i >> 3] >> (7 - (i & 7))) & 1) as u32);
        }
    }

    /// `variable_bits` (ETSI TS 103 420 §5.5.1 with a large group cap): chunked base-2^n.
    /// Only emits up to 3 groups — sufficient for every value we produce (< 2^12); panics otherwise.
    pub(crate) fn write_var(&mut self, value: u32, n: u32) {
        let base = 1u32 << n;
        if value < base {
            self.write(value, n);
            self.bit(0);
            return;
        }
        let off2 = base;
        if value < off2 + base * base {
            let v = value - off2;
            self.write(v / base, n);
            self.bit(1);
            self.write(v % base, n);
            self.bit(0);
            return;
        }
        let off3 = base + base * base;
        if value < off3 + base * base * base {
            let v = value - off3;
            self.write(v / (base * base), n);
            self.bit(1);
            self.write((v / base) % base, n);
            self.bit(1);
            self.write(v % base, n);
            self.bit(0);
            return;
        }
        panic!("write_var: value {value} too large for n={n} (>3 groups)");
    }

    pub(crate) fn bit_len(&self) -> usize {
        self.out.len() * 8 + self.nbits as usize
    }

    /// Byte-padded snapshot of the bits written so far, plus the exact bit count.
    pub(crate) fn snapshot(&self) -> (Vec<u8>, usize) {
        let nbits = self.bit_len();
        let mut bytes = self.out.clone();
        if self.nbits != 0 {
            bytes.push(self.cur << (8 - self.nbits));
        }
        (bytes, nbits)
    }

    /// Flush to bytes, zero-padding the final partial byte (MSB-aligned).
    pub(crate) fn into_bytes(mut self) -> Vec<u8> {
        if self.nbits != 0 {
            self.cur <<= 8 - self.nbits;
            self.out.push(self.cur);
        }
        self.out
    }
}

/// MSB-first bit reader (used by the EMDF field walker).
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pub(crate) pos: usize,
    pub(crate) end: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            end: data.len() * 8,
        }
    }

    pub(crate) fn read(&mut self, n: u32) -> u32 {
        let mut value = 0;
        for _ in 0..n {
            let bit = if self.pos < self.end {
                (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1
            } else {
                0
            };
            value = (value << 1) | u32::from(bit);
            self.pos += 1;
        }
        value
    }

    pub(crate) fn read_bit(&mut self) -> bool {
        self.read(1) == 1
    }

    pub(crate) fn read_var(&mut self, n: u32, max_groups: u32) -> u32 {
        let mut value = self.read(n);
        let mut more = self.read_bit();
        let mut num_group = 1u32;
        if max_groups > num_group {
            if more {
                value = (value << n) + (1 << n);
            }
            while more {
                value += self.read(n);
                more = self.read_bit();
                if num_group >= max_groups {
                    break;
                }
                if more {
                    value = (value << n) + (1 << n);
                    num_group += 1;
                }
            }
        }
        value
    }
}
