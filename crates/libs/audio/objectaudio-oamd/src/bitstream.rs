//! Bit reading over a byte slice.
//!
//! The parser keeps an `io::Result` facade because the merged `truehdd/oamd`
//! code grew around `bitstream-io`; the byte-level rules now come from
//! `salsa-bitstream`, which the E-AC-3/JOC codecs share.

use std::io;

use salsa_bitstream::{BitReader, BitstreamError};

/// A big-endian bit reader that knows how much of its slice is left.
#[derive(Debug)]
pub(crate) struct SliceReader<'a> {
    bits: BitReader<'a>,
}

impl<'a> SliceReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self {
            bits: BitReader::new(buf),
        }
    }

    #[inline(always)]
    pub(crate) fn get(&mut self) -> io::Result<bool> {
        Ok(self.bits.try_read(1).map_err(to_io)? != 0)
    }

    #[inline(always)]
    pub(crate) fn get_n<I: TryFrom<u32>>(&mut self, n: u32) -> io::Result<I> {
        let value = self.bits.try_read(n).map_err(to_io)?;
        I::try_from(value).map_err(|_| invalid("get_n", n, self.bits.position()))
    }

    #[inline(always)]
    pub(crate) fn get_s<S: TryFrom<i64>>(&mut self, n: u32) -> io::Result<S> {
        let position = self.bits.position();
        let value = if n == 0 {
            0
        } else {
            let raw = i64::from(self.bits.try_read(n).map_err(to_io)?);
            let shift = 64 - n;
            (raw << shift) >> shift
        };
        S::try_from(value).map_err(|_| invalid("get_s", n, position))
    }

    /// A value split into `n`-bit groups, each followed by a bit saying whether another
    /// group follows, up to `max_num_groups`.
    pub(crate) fn get_variable_bits_max(&mut self, n: u32, max_num_groups: u32) -> io::Result<u32> {
        self.bits.variable_bits(n, max_num_groups).map_err(to_io)
    }

    #[inline(always)]
    pub(crate) fn available(&mut self) -> io::Result<u64> {
        Ok(self.bits.remaining_bits() as u64)
    }

    #[inline(always)]
    pub(crate) fn position(&mut self) -> io::Result<u64> {
        Ok(self.bits.position() as u64)
    }

    pub(crate) fn skip_n(&mut self, n: u32) -> io::Result<()> {
        if self.bits.skip(n as usize) {
            Ok(())
        } else {
            Err(out_of_bounds(n, "skip_n", self.bits.position()))
        }
    }
}

fn to_io(error: BitstreamError) -> io::Error {
    let kind = match error {
        BitstreamError::Truncated | BitstreamError::TooManyGroups => io::ErrorKind::UnexpectedEof,
        BitstreamError::ValueTooLarge => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, error)
}

fn invalid(what: &str, n: u32, position: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{what}({n}): value does not fit the target type at {position}"),
    )
}

/// Reads past the end report where they ran out, which a bare `UnexpectedEof` does not say.
fn out_of_bounds(n: u32, what: &str, position: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("{what}({n}): out of bounds bits at {position}"),
    )
}
