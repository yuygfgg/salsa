#![forbid(unsafe_code)]
#![doc = "Salsa spatial panner, filterbank, covariance and object reconstruction DSP."]

pub mod analysis;
pub mod covariance;
pub mod dct;
pub mod decoder;
pub mod panner;
pub mod reconstruct;
pub mod stft;

pub use analysis::{BandStatistics, CoreErrorMeasurement, FrameStatistics, StatisticsAccumulator};
pub use covariance::{
    BandCovariance, CorrelatedPair, covariance_from_anchor, covariance_from_band_powers,
    interpolate_covariances,
};
pub use dct::Dct;
pub use decoder::{DecoderError, SalsaSpatialDecoder};
pub use panner::{CorePanner, core_gain_column, interpolated_core_columns};
pub use reconstruct::{
    Reconstruction, ReconstructionError, SolveError, estimate_objects, solve_regularized,
};
pub use stft::{
    Analysis, BandMatrices, BatchAnalyzer, Spectrum, Stft, StftConfig, Synthesis, analyze,
    apply_band_matrices, apply_band_matrices_full, synthesize, window,
};
