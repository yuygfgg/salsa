// SPDX-License-Identifier: MPL-2.0
//! DAMF (Dolby Atmos Master) to E-AC-3 JOC encoder.
//!
//! The library owns the downmix, LFE filtering, JOC analysis/encoding, OAMD
//! and EMDF signing pipeline. The `eac3joc-encode` binary is a thin CLI
//! over [`encode`].

mod bitwriter;
pub mod core;
mod emdf;
mod joc;
mod joc_analysis;
mod lfe;
mod phase_shift;
pub mod render;

pub use core::{CoreOptions, EncodeOptions, JocMode, JocOptions, JocQuant, JocSelect, encode};
pub use reac3e;
pub use render::PanMode;
