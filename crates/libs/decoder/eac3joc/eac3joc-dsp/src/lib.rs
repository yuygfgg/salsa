#![forbid(unsafe_code)]
#![doc = "JOC reconstruction: QMF analysis/synthesis, object matrix and E-AC-3 decode."]

mod decoder;
mod kernel;
mod qmf_tables;

pub use decoder::{DspError, Eac3JocDecoder};
pub use eac3joc::{FRAME_SAMPLES, MAX_CORE_CHANNELS, MAX_OBJECTS};
pub use kernel::{JocKernel, KernelError, OUTPUT_CHANNELS};
