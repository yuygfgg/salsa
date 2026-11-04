//! MSB-first bit reader shared by the EMDF and JOC parsers.
//!
//! The implementation lives in `salsa-bitstream` so the encoder and OAMD
//! parser use exactly the same truncation and variable-group rules.

pub use salsa_bitstream::BitReader;
