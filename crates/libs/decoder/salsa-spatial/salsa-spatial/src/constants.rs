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
/// Independent checkpoint interval: one second.
pub const CHECKPOINT_SAMPLES: u64 = 48_000;
/// Fixed parameter band count.
pub const PARAM_BAND_COUNT: usize = 24;
/// Half-open nonnegative-bin boundaries of the 24 parameter bands.
pub const PARAM_BAND_BOUNDARIES: [u16; 25] = [
    0, 2, 4, 7, 11, 15, 20, 27, 34, 43, 55, 68, 85, 105, 130, 160, 196, 240, 293, 358, 436, 532,
    648, 789, 961,
];
/// Full-range observation channels, in reconstruction order:
/// `FL, FR, FC, SL, SR, H_L, H_R`.
pub const CORE_CHANNEL_COUNT: usize = 7;
/// Legacy five-substream core layout.
pub const CORE_STREAM_COUNT: usize = 5;
/// LFE index in the decoded eight-channel observation bed.
pub const LFE_CHANNEL_INDEX: usize = 7;
/// Seven full-range observations plus LFE.
pub const BED_CHANNEL_COUNT: usize = 8;

// ---------------------------------------------------------------------------
// Opus multistream profile
// ---------------------------------------------------------------------------

/// Number of standard 7.1 output channels declared to stock players.
pub const PUBLIC_OUTPUT_CHANNELS: usize = 8;
/// Opus mapping family used by the profile.
pub const MAPPING_FAMILY: u8 = 1;
/// Number of core-first logical channels before auxiliary groups and silence:
/// `FL, FR, FC, LFE, SL, SR`.
pub const CORE_LOGICAL_COUNT: usize = 6;
/// Logical index of the separate LFE signal.
pub const LOGICAL_LFE_INDEX: usize = 3;
/// The initial auxiliary group has one coupled pair.
pub const INITIAL_AUX_CHANNELS: usize = 2;
/// Initial logical channel count: six core + two auxiliary + shared silence.
pub const INITIAL_LOGICAL_COUNT: usize = CORE_LOGICAL_COUNT + INITIAL_AUX_CHANNELS + 1;
/// Initial coded channel count: six fallback + two auxiliary + shared silence.
pub const INITIAL_CODED_CHANNELS: usize = 9;
/// Initial number of Opus substreams.
pub const INITIAL_STREAM_COUNT: usize = 6;
/// Initial number of coupled Opus substreams.
pub const INITIAL_COUPLED_COUNT: usize = 3;
/// Maximum coded channel count permitted by the Opus multistream API.
pub const MAX_CODED_CHANNELS: usize = 255;
/// Maximum auxiliary capacity with the shared-silence profile.
pub const MAX_AUXILIARY_CHANNELS: usize = MAX_CODED_CHANNELS - CORE_LOGICAL_COUNT - 1;
/// Hard bound on stream counts accepted by the parser.
pub const MAX_STREAM_COUNT: usize = 255;
/// Maximum number of fragments accepted for one record.
pub const MAX_RECORD_FRAGMENTS: usize = 64;
/// Maximum reassembled private record size.
pub const MAX_RECORD_BYTES: usize = 1 << 20;
/// Standard mapping-family-1 8-channel output order.
pub const PUBLIC_CHANNEL_ORDER: [&str; PUBLIC_OUTPUT_CHANNELS] =
    ["FL", "FC", "FR", "SL", "SR", "BL", "BR", "LFE"];
/// Decoded PCM order used by FFmpeg for 8-channel family-1 output.
pub const FFMPEG_CHANNEL_ORDER: [&str; PUBLIC_OUTPUT_CHANNELS] =
    ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"];
/// Initial public Opus channel map.
pub const INITIAL_CHANNEL_MAPPING: [u8; PUBLIC_OUTPUT_CHANNELS] = [0, 6, 1, 2, 3, 8, 8, 7];
/// Initial private logical-to-coded permutation.
pub const INITIAL_LOGICAL_TO_CODED: [u8; INITIAL_CODED_CHANNELS] = [0, 1, 6, 7, 2, 3, 4, 5, 8];
/// Initial core bitrates indexed by substream role:
/// front, surround, auxiliary, center, LFE.
pub const INITIAL_CORE_BITRATES: [u32; 5] = [200_000, 128_000, 120_000, 112_000, 24_000];
/// Encoder lookahead for the tested restricted-low-delay configuration.
pub const TESTED_LOOKAHEAD_SAMPLES: usize = 120;
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
/// Dimensionless floor applied to the diagonally normalized reconstruction
/// system. This is a numerical-stabilization term, not an audio-quality knob:
/// it is about four orders of magnitude above f64 roundoff for a seven-channel
/// solve and far below any observable signal level.
pub const RECONSTRUCTION_EPSILON: f64 = 1.0e-12;
/// Integer parameter-state grid: half a decibel per unit.
pub const HALF_DB_SCALE: f64 = 0.5;

/// Full-range core channel short names in internal order.
pub const CORE_CHANNEL_NAMES: [&str; CORE_CHANNEL_COUNT] =
    ["L", "R", "C", "Ls", "Rs", "H_L", "H_R"];

/// Opus substream names in stream order.
pub const CORE_STREAM_NAMES: [&str; CORE_STREAM_COUNT] =
    ["front", "surround", "height", "center", "lfe"];
