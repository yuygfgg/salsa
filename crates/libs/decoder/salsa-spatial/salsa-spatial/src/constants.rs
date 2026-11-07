//! Fixed constants of the salsa-spatial v1 profile.

/// Sample rate of the core and the parameter grid.
pub const SAMPLE_RATE: u32 = 48_000;
/// STFT hop: 5 ms.
pub const HOP_SAMPLES: usize = 240;
/// Complex STFT size: 40 ms.
pub const FFT_SIZE: usize = 1920;
/// Number of nonnegative FFT bins including DC and Nyquist.
pub const FFT_BINS: usize = 961;
/// One Opus packet: 20 ms.
pub const OPUS_FRAME_SAMPLES: usize = 960;
/// Four 5 ms hops per Opus packet.
pub const HOPS_PER_PACKET: usize = 4;
/// Normal parameter interval: 20 ms.
pub const PARAM_INTERVAL_SAMPLES: usize = 960;
/// Normal parameter interval in hops.
pub const PARAM_INTERVAL_HOPS: usize = 4;
/// One parameter coding tile: 100 ms.
pub const TILE_SAMPLES: usize = 4800;
/// Tile length in hops.
pub const TILE_HOPS: usize = 20;
/// Tile length in Opus packets.
pub const TILE_PACKETS: usize = 5;
/// Independent parameter snapshot interval: one second.
pub const SNAPSHOT_SAMPLES: u64 = 48_000;
/// Fixed parameter band count.
pub const PARAM_BAND_COUNT: usize = 24;
/// Half-open nonnegative-bin boundaries of the 24 parameter bands.
pub const PARAM_BAND_BOUNDARIES: [u16; 25] = [
    0, 2, 4, 7, 11, 15, 20, 27, 34, 43, 55, 68, 85, 105, 130, 160, 196, 240, 293, 358, 436, 532,
    648, 789, 961,
];
/// Full-range core channels, in internal order.
pub const CORE_CHANNEL_COUNT: usize = 7;
/// Five Opus substreams.
pub const CORE_STREAM_COUNT: usize = 5;
/// LFE index in the decoded eight-channel bed.
pub const LFE_CHANNEL_INDEX: usize = 7;
/// Seven full-range observations plus LFE.
pub const BED_CHANNEL_COUNT: usize = 8;
/// Maximum number of source objects covered by the first version.
pub const MAX_OBJECTS: usize = 128;
/// Number of fast DCT coefficients in the power model.
pub const DCT_COEFF_COUNT: usize = 6;
/// Maximum quantized correlation magnitude (15/16).
pub const CORRELATION_MAX_MAGNITUDE: u8 = 15;
/// Number of uniform correlation phase directions.
pub const CORRELATION_PHASE_STEPS: u16 = 32;
/// Candidate quantizer scales in dB.
pub const CORRECTION_SCALES_DB: [f64; 4] = [0.5, 1.0, 2.0, 4.0];
/// Initial run-level RD weight per encoded bit.
pub const DEFAULT_BETA: f64 = 1.0e-6;
/// Diagonal loading of the reconstruction system.
pub const REGULARIZATION_LAMBDA: f64 = 1.0e-3;
/// Integer parameter-state grid: half a decibel per unit.
pub const HALF_DB_SCALE: f64 = 0.5;

/// Full-range core channel short names in internal order.
pub const CORE_CHANNEL_NAMES: [&str; CORE_CHANNEL_COUNT] =
    ["L", "R", "C", "Ls", "Rs", "H_L", "H_R"];

/// Opus substream names in stream order.
pub const CORE_STREAM_NAMES: [&str; CORE_STREAM_COUNT] =
    ["front", "surround", "height", "center", "lfe"];
