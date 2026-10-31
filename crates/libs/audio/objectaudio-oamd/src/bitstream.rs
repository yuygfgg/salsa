//! Bit reading over a byte slice, on top of the `bitstream-io` crate.
//!
//! Only what the metadata parser needs. This is deliberately not a public API: the payload is
//! read from a slice, so callers never hold a reader and the crate stays free to change this.
//!
//! Derived from `truehdd/oamd` 0.1.0 (Apache-2.0), merged here with the parser.
//! See `LICENSE-oamd`.

use std::io;

use bitstream_io::{BigEndian, BitRead, BitReader, SignedInteger, UnsignedInteger};

/// A big-endian bit reader that knows how much of its slice is left.
#[derive(Debug)]
pub(crate) struct SliceReader<'a> {
    bits: BitReader<io::Cursor<&'a [u8]>, BigEndian>,
    len: u64,
}

impl<'a> SliceReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self {
            bits: BitReader::new(io::Cursor::new(buf)),
            len: (buf.len() as u64) << 3,
        }
    }

    #[inline(always)]
    pub(crate) fn get(&mut self) -> io::Result<bool> {
        self.bits.read_bit()
    }

    #[inline(always)]
    pub(crate) fn get_n<I: UnsignedInteger>(&mut self, n: u32) -> io::Result<I> {
        self.bounded(n, "get_n", |bits| bits.read_unsigned_var(n))
    }

    #[inline(always)]
    pub(crate) fn get_s<S: SignedInteger>(&mut self, n: u32) -> io::Result<S> {
        self.bounded(n, "get_s", |bits| bits.read_signed_var(n))
    }

    /// A value split into `n`-bit groups, each followed by a bit saying whether another
    /// group follows, up to `max_num_groups`.
    pub(crate) fn get_variable_bits_max(&mut self, n: u32, max_num_groups: u32) -> io::Result<u32> {
        let mut value = 0;
        let mut groups = 0;
        let mut more = true;

        while more && groups < max_num_groups {
            value += self.get_n::<u32>(n)?;
            more = self.get()?;
            if more {
                value = (value + 1) << n;
                groups += 1;
            }
        }

        Ok(value)
    }

    #[inline(always)]
    pub(crate) fn available(&mut self) -> io::Result<u64> {
        self.bits.position_in_bits().map(|pos| self.len - pos)
    }

    #[inline(always)]
    pub(crate) fn position(&mut self) -> io::Result<u64> {
        self.bits.position_in_bits()
    }

    pub(crate) fn skip_n(&mut self, n: u32) -> io::Result<()> {
        // bitstream-io reports EOF itself for a short skip, so only a long one is worth checking
        if n <= 64 {
            return self.bits.skip(n);
        }
        match self.available()? {
            available if n as u64 > available => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "skip_n: out of bounds bits",
            )),
            _ => self.bits.skip(n),
        }
    }

    /// Reads past the end report where they ran out, which a bare `UnexpectedEof` does not say.
    #[inline(always)]
    fn bounded<T>(
        &mut self,
        n: u32,
        what: &str,
        read: impl FnOnce(&mut BitReader<io::Cursor<&'a [u8]>, BigEndian>) -> io::Result<T>,
    ) -> io::Result<T> {
        if n > 32 && n as u64 > self.available()? {
            return Err(self.out_of_bounds(n, what));
        }
        match read(&mut self.bits) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(self.out_of_bounds(n, what)),
            other => other,
        }
    }

    fn out_of_bounds(&mut self, n: u32, what: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "{what}({n}): out of bounds bits at {}",
                self.bits.position_in_bits().unwrap_or(0)
            ),
        )
    }
}
