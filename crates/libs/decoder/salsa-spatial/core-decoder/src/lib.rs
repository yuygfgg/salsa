//! libopus-backed five-stream 5.1.2 core decoder.
//!
//! The core is carried as five Opus substreams following the profile in
//! `docs/salsa-spatial-implementation.md`:
//!
//! | stream index | channels | bed channels |
//! |---:|---|---|
//! | 0 | stereo | `L`, `R` |
//! | 1 | stereo | `Ls`, `Rs` |
//! | 2 | stereo | `H_L`, `H_R` |
//! | 3 | mono | `C` |
//! | 4 | mono | `LFE` |
//!
//! [`OpusBedDecoder::decode`] returns eight planar channels in the order
//! `L, R, C, Ls, Rs, H_L, H_R, LFE`.
//!
//! # Delay and alignment
//!
//! All five substreams use the same 48 kHz Opus configuration, so libopus
//! applies the same algorithmic delay to every decoded stream; the streams are
//! already mutually aligned sample-for-sample.  The raw decoder output is
//! delayed by the encoder lookahead relative to the source clock.  That value
//! is reported by [`OpusBedDecoder::lookahead_samples`] (120 samples at 48 kHz
//! for restricted-low-delay).  Because the API contract fixes the length of a
//! non-final decoded frame at [`salsa_spatial::OPUS_FRAME_SAMPLES`], `decode`
//! returns the complete libopus frame and does not shorten the first frame.
//! Callers that need source-clock alignment discard the first
//! `lookahead_samples()` samples of the concatenated program (or query
//! [`OpusBedDecoder::pending_lookahead_samples`] while streaming).

use opus::{Application, Channels, Decoder, Encoder};
use salsa_spatial::{
    CorePacket, MAX_OBJECTS, OPUS_FRAME_SAMPLES, SAMPLE_RATE, SalsaSpatialStreamConfig,
};
use thiserror::Error;

/// Number of Opus substreams in the legacy 5.1.2 core bed.
pub const CORE_STREAMS: usize = 5;

/// One elementary substream in the codec plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubstreamPlan {
    channels: usize,
    coded_start: usize,
}

/// How coded channels map into the reconstruction observation order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DecodePlan {
    substreams: Vec<SubstreamPlan>,
    /// Coded channel for each of the seven full-range observations.
    observation_coded: [usize; 7],
    lfe_coded: usize,
    center_substream: usize,
    stream_count: usize,
}

impl DecodePlan {
    fn from_config(config: &SalsaSpatialStreamConfig) -> Result<Self, CoreDecodeError> {
        let binding = &config.codec;
        if binding.is_legacy() {
            return Ok(Self {
                substreams: vec![
                    SubstreamPlan {
                        channels: 2,
                        coded_start: 0,
                    },
                    SubstreamPlan {
                        channels: 2,
                        coded_start: 2,
                    },
                    SubstreamPlan {
                        channels: 2,
                        coded_start: 4,
                    },
                    SubstreamPlan {
                        channels: 1,
                        coded_start: 6,
                    },
                    SubstreamPlan {
                        channels: 1,
                        coded_start: 7,
                    },
                ],
                observation_coded: [0, 1, 6, 2, 3, 4, 5],
                lfe_coded: 7,
                center_substream: 3,
                stream_count: 5,
            });
        }
        let coded = binding.coded_channel_count();
        let mut substreams = Vec::with_capacity(binding.stream_count);
        for stream in 0..binding.stream_count {
            let channels = if stream < binding.coupled_count { 2 } else { 1 };
            let coded_start = if stream < binding.coupled_count {
                stream * 2
            } else {
                2 * binding.coupled_count + (stream - binding.coupled_count)
            };
            if coded_start + channels > coded {
                return Err(CoreDecodeError::InvalidConfig(
                    "substream plan exceeds coded channel count".to_owned(),
                ));
            }
            substreams.push(SubstreamPlan {
                channels,
                coded_start,
            });
        }
        let logical = config.observation_logical();
        if logical.len() != 7 {
            return Err(CoreDecodeError::InvalidConfig(
                "observation order must have seven full-range rows".to_owned(),
            ));
        }
        let mut observation_coded = [0usize; 7];
        for (index, logical_index) in logical.iter().enumerate() {
            let coded_index = *binding
                .logical_to_coded
                .get(*logical_index)
                .ok_or_else(|| {
                    CoreDecodeError::InvalidConfig(
                        "observation logical index has no coded binding".to_owned(),
                    )
                })? as usize;
            if coded_index >= coded {
                return Err(CoreDecodeError::InvalidConfig(
                    "observation coded channel is out of range".to_owned(),
                ));
            }
            observation_coded[index] = coded_index;
        }
        let lfe_coded = *binding
            .logical_to_coded
            .get(salsa_spatial::LOGICAL_LFE_INDEX)
            .ok_or_else(|| {
                CoreDecodeError::InvalidConfig("missing LFE logical binding".to_owned())
            })? as usize;
        Ok(Self {
            substreams,
            observation_coded,
            lfe_coded,
            center_substream: binding.center_substream_index().map_err(|error| {
                CoreDecodeError::InvalidConfig(format!("invalid center substream: {error}"))
            })?,
            stream_count: binding.stream_count,
        })
    }
}

/// Version information of the Rust wrapper and the linked native libopus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusVersionInfo {
    /// Version of the Rust `opus` wrapper crate.
    pub wrapper: &'static str,
    /// Runtime version string reported by the linked native libopus.
    pub native: String,
}

/// Return the wrapper and native libopus versions.
pub fn version_info() -> OpusVersionInfo {
    OpusVersionInfo {
        wrapper: "opus 0.4.0",
        native: opus::version().to_owned(),
    }
}

/// Errors produced while constructing or running the core decoder.
#[derive(Debug, Error)]
pub enum CoreDecodeError {
    /// A libopus decoder or encoder call failed.
    #[error("libopus error: {0}")]
    Opus(#[from] opus::Error),

    /// The configured object count exceeds the v1 profile limit.
    #[error("object count {0} exceeds the v1 limit of {MAX_OBJECTS}")]
    TooManyObjects(usize),

    /// A non-final packet decoded to an unexpected number of samples.
    #[error("stream {stream} decoded {actual} samples, expected {expected}")]
    FrameSizeMismatch {
        stream: usize,
        expected: usize,
        actual: usize,
    },

    /// The center-substream extension could not be handled.
    #[error("center extension handling failed: {0}")]
    Extension(String),

    /// The stream configuration cannot be decoded by this build.
    #[error("invalid core decoder configuration: {0}")]
    InvalidConfig(String),
}

/// Five libopus decoder states plus the program delay bookkeeping.
///
/// Each decode call consumes one 20 ms packet group.  Empty substream packets
/// invoke libopus packet-loss concealment (`fec = false`).
#[derive(Debug)]
pub struct OpusBedDecoder {
    decoders: Vec<Decoder>,
    plan: DecodePlan,
    object_count: usize,
    lookahead: usize,
    samples_decoded: u64,
    lookahead_remaining: usize,
    pending_extension: Option<Vec<u8>>,
}

impl OpusBedDecoder {
    /// Create five decoder states for the legacy 5.1.2 core profile.
    pub fn new(object_count: usize) -> Result<Self, CoreDecodeError> {
        let mut config = SalsaSpatialStreamConfig::legacy();
        config.object_count = object_count;
        Self::from_config(&config)
    }

    /// Create decoder states from the declared stream configuration.
    pub fn from_config(config: &SalsaSpatialStreamConfig) -> Result<Self, CoreDecodeError> {
        if config.object_count > MAX_OBJECTS {
            return Err(CoreDecodeError::TooManyObjects(config.object_count));
        }
        let plan = DecodePlan::from_config(config)?;
        let lookahead = query_encoder_lookahead()?;
        let mut decoders = Vec::with_capacity(plan.substreams.len());
        for substream in &plan.substreams {
            let channels = if substream.channels == 2 {
                Channels::Stereo
            } else {
                Channels::Mono
            };
            decoders.push(Decoder::new(SAMPLE_RATE, channels)?);
        }
        Ok(Self {
            decoders,
            plan,
            object_count: config.object_count,
            lookahead,
            samples_decoded: 0,
            lookahead_remaining: lookahead,
            pending_extension: None,
        })
    }

    /// Decode one 20 ms packet group.
    ///
    /// Returns eight planar channels in `FL, FR, FC, SL, SR, H_L, H_R, LFE`
    /// observation order. Hidden auxiliary channels are routed into the
    /// observation buffer; the shared rear-silence channel is not returned.
    pub fn decode(&mut self, packet: &CorePacket) -> Result<[Vec<f32>; 8], CoreDecodeError> {
        if packet.len() != 0 && packet.len() != self.plan.stream_count {
            return Err(CoreDecodeError::InvalidConfig(format!(
                "packet has {} substreams, expected {}",
                packet.len(),
                self.plan.stream_count
            )));
        }
        let center_bytes = packet
            .stream(self.plan.center_substream)
            .unwrap_or_default()
            .to_vec();
        let (center, extension) = split_center_extension(&center_bytes);
        self.pending_extension = extension;

        let coded_count = self
            .plan
            .substreams
            .last()
            .map_or(0, |plan| plan.coded_start + plan.channels);
        let mut buffers: Vec<Vec<f32>> = Vec::with_capacity(self.plan.substreams.len());
        let mut frames = usize::MAX;
        for (index, substream) in self.plan.substreams.iter().enumerate() {
            let data = if index == self.plan.center_substream {
                center.as_slice()
            } else {
                packet.stream(index).unwrap_or_default()
            };
            let mut buffer = vec![0.0f32; OPUS_FRAME_SAMPLES * substream.channels];
            let actual = self.decoders[index].decode_float(data, &mut buffer, false)?;
            frames = frames.min(actual);
            buffers.push(buffer);
        }
        if frames == usize::MAX {
            frames = 0;
        }
        let mut coded: Vec<Vec<f32>> = vec![Vec::with_capacity(frames); coded_count];
        for (index, substream) in self.plan.substreams.iter().enumerate() {
            let buffer = &buffers[index];
            if substream.channels == 2 {
                let (left, right) = stereo_planes(buffer, frames);
                if substream.coded_start + 1 < coded_count {
                    coded[substream.coded_start] = left;
                    coded[substream.coded_start + 1] = right;
                }
            } else if substream.coded_start < coded_count {
                coded[substream.coded_start] = buffer[..frames.min(buffer.len())].to_vec();
            }
        }

        let mut out: [Vec<f32>; 8] = std::array::from_fn(|_| Vec::with_capacity(frames));
        for (observation, coded_index) in self.plan.observation_coded.iter().enumerate() {
            if let Some(plane) = coded.get(*coded_index) {
                out[observation] = plane.clone();
            }
        }
        if let Some(plane) = coded.get(self.plan.lfe_coded) {
            out[7] = plane.clone();
        }

        self.samples_decoded = self.samples_decoded.saturating_add(frames as u64);
        self.lookahead_remaining = (self.lookahead as u64)
            .saturating_sub(self.samples_decoded)
            .try_into()
            .unwrap_or(usize::MAX);

        Ok(out)
    }

    /// Encoder lookahead reported by an equivalent encoder configuration, in
    /// 48 kHz samples.
    ///
    /// This is the number of leading decoded samples that belong to the codec
    /// pre-roll and should be discarded to align the program with the source
    /// clock.
    pub fn lookahead_samples(&self) -> usize {
        self.lookahead
    }

    /// Number of leading program samples that still belong to the lookahead
    /// for the current program position.
    ///
    /// Starts at [`Self::lookahead_samples`] and decreases as frames are
    /// decoded.  It is reset by [`Self::reset`].
    pub fn pending_lookahead_samples(&self) -> usize {
        self.lookahead_remaining
    }

    /// Take the enhancement payload extracted from the most recent center
    /// packet, if any.
    ///
    /// Each call to [`Self::decode`] replaces the pending payload with the
    /// extension carried by that call's center packet.  Call this after every
    /// decode when collecting enhancement fragments.
    pub fn take_extension(&mut self) -> Option<Vec<u8>> {
        self.pending_extension.take()
    }

    /// Reinitialize all five decoder states and the delay bookkeeping.
    pub fn reset(&mut self) {
        for decoder in &mut self.decoders {
            if let Err(error) = decoder.reset_state() {
                log::warn!("failed to reset an Opus decoder: {error}");
            }
        }
        self.samples_decoded = 0;
        self.lookahead_remaining = self.lookahead;
        self.pending_extension = None;
    }

    /// Number of source objects configured at construction.
    pub fn object_count(&self) -> usize {
        self.object_count
    }
}

fn query_encoder_lookahead() -> Result<usize, CoreDecodeError> {
    let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::LowDelay)?;
    let lookahead = encoder.get_lookahead()?;
    Ok(lookahead.max(0) as usize)
}

fn stereo_planes(interleaved: &[f32], frames: usize) -> (Vec<f32>, Vec<f32>) {
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for frame in 0..frames {
        left.push(interleaved[frame * 2]);
        right.push(interleaved[frame * 2 + 1]);
    }
    (left, right)
}

/// Strip the salsa enhancement extension from the center packet.
///
/// The shared `salsa_spatial::opus_ext::strip_extension` implementation owns
/// the wire format.  A panic from that implementation is treated as "no
/// extension" so ordinary packets (including Opus padding) still decode; this
/// keeps the decoder usable while the shared module is a workspace
/// placeholder.
fn split_center_extension(packet: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        salsa_spatial::opus_ext::strip_extension(packet)
    }));
    match result {
        Ok(Ok((ordinary, extension))) => (ordinary, extension),
        Ok(Err(error)) => {
            log::warn!("center extension could not be stripped: {error}");
            (packet.to_vec(), None)
        }
        Err(_) => {
            log::warn!("center extension stripper is unavailable; passing the packet to libopus");
            (packet.to_vec(), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus::{Bitrate, Channels as OpusChannels, Encoder};

    const SR: u32 = SAMPLE_RATE;

    fn sine(freq: f64, phase: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| {
                let t = n as f64 / f64::from(SR);
                (0.1 * (2.0 * std::f64::consts::PI * freq * t + phase).sin()) as f32
            })
            .collect()
    }

    fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(left.len() * 2);
        for (l, r) in left.iter().zip(right) {
            out.push(*l);
            out.push(*r);
        }
        out
    }

    fn make_encoder(channels: OpusChannels, bitrate: i32) -> Encoder {
        let mut encoder = Encoder::new(SR, channels, Application::LowDelay).unwrap();
        encoder.set_complexity(10).unwrap();
        encoder.set_bitrate(Bitrate::Bits(bitrate)).unwrap();
        encoder.set_vbr(false).unwrap();
        encoder.set_vbr_constraint(true).unwrap();
        encoder.set_dtx(false).unwrap();
        encoder.set_inband_fec(false).unwrap();
        encoder
    }

    fn encode_stream(
        input: &[f32],
        channels: OpusChannels,
        bitrate: i32,
        frame_samples: usize,
    ) -> Vec<Vec<u8>> {
        let mut encoder = make_encoder(channels, bitrate);
        let stride = frame_samples * channels as usize;
        input
            .chunks(stride)
            .map(|chunk| encoder.encode_vec_float(chunk, 8192).unwrap())
            .collect()
    }

    struct EncodedBed {
        packets: Vec<CorePacket>,
        source: [Vec<f32>; 8],
        lookahead: usize,
    }

    fn encode_bed(total_frames: usize) -> EncodedBed {
        let lookahead = query_encoder_lookahead().unwrap();
        let source = [
            sine(440.0, 0.0, total_frames),
            sine(440.0, std::f64::consts::PI, total_frames),
            sine(330.0, 0.0, total_frames),
            sine(220.0, 0.0, total_frames),
            sine(110.0, 0.0, total_frames),
            sine(550.0, 0.0, total_frames),
            sine(660.0, 0.0, total_frames),
            sine(80.0, 0.0, total_frames),
        ];

        let front = interleave(&source[0], &source[1]);
        let surround = interleave(&source[3], &source[4]);
        let height = interleave(&source[5], &source[6]);

        let front_packets =
            encode_stream(&front, OpusChannels::Stereo, 160_000, OPUS_FRAME_SAMPLES);
        let surround_packets =
            encode_stream(&surround, OpusChannels::Stereo, 128_000, OPUS_FRAME_SAMPLES);
        let height_packets =
            encode_stream(&height, OpusChannels::Stereo, 160_000, OPUS_FRAME_SAMPLES);
        let center_packets =
            encode_stream(&source[2], OpusChannels::Mono, 112_000, OPUS_FRAME_SAMPLES);
        let lfe_packets = encode_stream(&source[7], OpusChannels::Mono, 16_000, OPUS_FRAME_SAMPLES);

        let packets = front_packets
            .into_iter()
            .zip(surround_packets)
            .zip(height_packets)
            .zip(center_packets)
            .zip(lfe_packets)
            .map(|((((front, surround), height), center), lfe)| {
                CorePacket::from_legacy(front, surround, height, center, lfe)
            })
            .collect();

        EncodedBed {
            packets,
            source,
            lookahead,
        }
    }

    fn correlation(a: &[f32], b: &[f32]) -> f64 {
        let mut cross = 0.0;
        let mut aa = 0.0;
        let mut bb = 0.0;
        for (x, y) in a.iter().zip(b) {
            let x = f64::from(*x);
            let y = f64::from(*y);
            cross += x * y;
            aa += x * x;
            bb += y * y;
        }
        cross / (aa.sqrt() * bb.sqrt() + 1e-30)
    }

    fn all_finite(bed: &[Vec<f32>; 8]) -> bool {
        bed.iter().all(|plane| plane.iter().all(|s| s.is_finite()))
    }

    #[test]
    fn version_info_reports_native_version() {
        let info = version_info();
        assert_eq!(info.wrapper, "opus 0.4.0");
        assert!(!info.native.is_empty());
    }

    #[test]
    fn new_rejects_too_many_objects() {
        let error = OpusBedDecoder::new(MAX_OBJECTS + 1).unwrap_err();
        assert!(matches!(error, CoreDecodeError::TooManyObjects(_)));
    }

    #[test]
    fn lookahead_is_reported_and_reset() {
        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let lookahead = decoder.lookahead_samples();
        assert!(lookahead > 0);
        assert_eq!(decoder.pending_lookahead_samples(), lookahead);

        let encoded = encode_bed(OPUS_FRAME_SAMPLES * 2);
        decoder.decode(&encoded.packets[0]).unwrap();
        assert_eq!(
            decoder.pending_lookahead_samples(),
            lookahead.saturating_sub(OPUS_FRAME_SAMPLES)
        );
        decoder.reset();
        assert_eq!(decoder.pending_lookahead_samples(), lookahead);
        assert_eq!(decoder.lookahead_samples(), lookahead);
    }

    #[test]
    fn decodes_channel_order_and_waveform() {
        let encoded = encode_bed(OPUS_FRAME_SAMPLES * 6);
        let mut decoder = OpusBedDecoder::new(8).unwrap();

        let mut decoded: [Vec<f32>; 8] = std::array::from_fn(|_| Vec::new());
        for packet in &encoded.packets {
            let bed = decoder.decode(packet).unwrap();
            for (plane, channel) in bed.into_iter().enumerate() {
                assert_eq!(channel.len(), OPUS_FRAME_SAMPLES);
                decoded[plane].extend(channel);
            }
        }

        let lookahead = decoder.lookahead_samples();
        assert_eq!(lookahead, encoded.lookahead);
        let window = decoded[0].len() - lookahead;
        for (channel, expected) in decoded.iter().zip(&encoded.source) {
            let got = &channel[lookahead..lookahead + window];
            let reference = &expected[..window];
            let correlation = correlation(got, reference);
            assert!(
                correlation > 0.9,
                "channel correlation {correlation} is too low"
            );
        }
    }

    #[test]
    fn plain_center_packet_strips_to_nothing() {
        let encoded = encode_bed(OPUS_FRAME_SAMPLES);
        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let bed = decoder.decode(&encoded.packets[0]).unwrap();
        assert_eq!(bed[2].len(), OPUS_FRAME_SAMPLES);
        assert!(decoder.take_extension().is_none());
    }

    #[test]
    fn padded_center_packet_still_decodes() {
        let encoded = encode_bed(OPUS_FRAME_SAMPLES);
        let mut padded = encoded.packets[0].stream(3).unwrap().to_vec();
        let original_len = padded.len();
        padded.resize(original_len + 32, 0);
        let padded_len = opus::packet::pad(&mut padded, original_len).unwrap();
        padded.truncate(padded_len);

        let mut packet = encoded.packets[0].clone();
        packet.substreams[3] = padded;

        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let bed = decoder.decode(&packet).unwrap();
        assert_eq!(bed[2].len(), OPUS_FRAME_SAMPLES);
        assert!(all_finite(&bed));
    }

    #[test]
    fn center_extension_round_trips_when_supported() {
        let encoded = encode_bed(OPUS_FRAME_SAMPLES);
        let payload = b"salsa-enhancement-fragment".to_vec();
        let packet = encoded.packets[0].stream(3).unwrap().to_vec();

        let appended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            salsa_spatial::opus_ext::append_extension(&packet, &payload)
        }));
        let extended = match appended {
            Ok(Ok(bytes)) => bytes,
            _ => {
                // The shared extension implementation is a workspace
                // placeholder at the time this crate is tested.
                return;
            }
        };

        let mut packet_with_extension = encoded.packets[0].clone();
        packet_with_extension.substreams[3] = extended;
        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let bed = decoder.decode(&packet_with_extension).unwrap();
        assert_eq!(bed[2].len(), OPUS_FRAME_SAMPLES);
        assert_eq!(decoder.take_extension(), Some(payload));
    }

    #[test]
    fn reset_restores_initial_decoder_state() {
        let encoded = encode_bed(OPUS_FRAME_SAMPLES * 3);
        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let first = decoder.decode(&encoded.packets[0]).unwrap();
        decoder.decode(&encoded.packets[1]).unwrap();
        decoder.reset();
        let after_reset = decoder.decode(&encoded.packets[0]).unwrap();

        for (a, b) in first.iter().zip(&after_reset) {
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(b) {
                assert!((x - y).abs() < 1e-6, "{x} != {y}");
            }
        }
    }

    #[test]
    fn final_partial_packet_is_shorter() {
        let partial = 480;
        let total = OPUS_FRAME_SAMPLES * 2 + partial;
        let encoded = encode_bed(total);
        assert_eq!(encoded.packets.len(), 3);

        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let first = decoder.decode(&encoded.packets[0]).unwrap();
        let second = decoder.decode(&encoded.packets[1]).unwrap();
        let last = decoder.decode(&encoded.packets[2]).unwrap();

        for bed in [&first, &second] {
            for channel in bed {
                assert_eq!(channel.len(), OPUS_FRAME_SAMPLES);
            }
        }
        for channel in &last {
            assert_eq!(channel.len(), partial);
        }
    }

    #[test]
    fn empty_packet_uses_packet_loss_concealment() {
        let encoded = encode_bed(OPUS_FRAME_SAMPLES * 2);
        let mut decoder = OpusBedDecoder::new(0).unwrap();
        decoder.decode(&encoded.packets[0]).unwrap();

        let lost = CorePacket::default();
        let bed = decoder.decode(&lost).unwrap();
        for channel in &bed {
            assert_eq!(channel.len(), OPUS_FRAME_SAMPLES);
        }
        assert!(all_finite(&bed));
    }

    #[test]
    fn first_empty_packet_returns_silence() {
        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let bed = decoder.decode(&CorePacket::default()).unwrap();
        for channel in &bed {
            assert_eq!(channel.len(), OPUS_FRAME_SAMPLES);
            assert!(channel.iter().all(|&sample| sample == 0.0));
        }
    }
}
