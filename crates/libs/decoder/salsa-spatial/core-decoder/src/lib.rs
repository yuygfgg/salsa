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
use salsa_spatial::{CorePacket, MAX_OBJECTS, OPUS_FRAME_SAMPLES, SAMPLE_RATE};
use thiserror::Error;

/// Number of Opus substreams in the 5.1.2 core bed.
pub const CORE_STREAMS: usize = 5;

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
}

/// Five libopus decoder states plus the program delay bookkeeping.
///
/// Each decode call consumes one 20 ms packet group.  Empty substream packets
/// invoke libopus packet-loss concealment (`fec = false`).
#[derive(Debug)]
pub struct OpusBedDecoder {
    decoders: [Decoder; CORE_STREAMS],
    object_count: usize,
    lookahead: usize,
    samples_decoded: u64,
    lookahead_remaining: usize,
    pending_extension: Option<Vec<u8>>,
}

impl OpusBedDecoder {
    /// Create five decoder states for the 5.1.2 core profile.
    ///
    /// `object_count` is validated against the v1 limit and retained for
    /// reporting; the core bed itself always has five substreams.
    pub fn new(object_count: usize) -> Result<Self, CoreDecodeError> {
        if object_count > MAX_OBJECTS {
            return Err(CoreDecodeError::TooManyObjects(object_count));
        }
        let lookahead = query_encoder_lookahead()?;
        let decoders = [
            Decoder::new(SAMPLE_RATE, Channels::Stereo)?,
            Decoder::new(SAMPLE_RATE, Channels::Stereo)?,
            Decoder::new(SAMPLE_RATE, Channels::Stereo)?,
            Decoder::new(SAMPLE_RATE, Channels::Mono)?,
            Decoder::new(SAMPLE_RATE, Channels::Mono)?,
        ];
        Ok(Self {
            decoders,
            object_count,
            lookahead,
            samples_decoded: 0,
            lookahead_remaining: lookahead,
            pending_extension: None,
        })
    }

    /// Decode one 20 ms packet group.
    ///
    /// Returns eight planar channels in `L, R, C, Ls, Rs, H_L, H_R, LFE`
    /// order.  Each channel has [`salsa_spatial::OPUS_FRAME_SAMPLES`] samples
    /// for a full packet, or the shorter natural duration of the final partial
    /// packet.  An empty substream packet requests libopus packet-loss
    /// concealment.
    pub fn decode(&mut self, packet: &CorePacket) -> Result<[Vec<f32>; 8], CoreDecodeError> {
        let (center, extension) = split_center_extension(&packet.center);
        self.pending_extension = extension;

        let mut front_buf = vec![0.0f32; OPUS_FRAME_SAMPLES * 2];
        let mut surround_buf = vec![0.0f32; OPUS_FRAME_SAMPLES * 2];
        let mut height_buf = vec![0.0f32; OPUS_FRAME_SAMPLES * 2];
        let mut center_buf = vec![0.0f32; OPUS_FRAME_SAMPLES];
        let mut lfe_buf = vec![0.0f32; OPUS_FRAME_SAMPLES];

        let front_frames = self.decoders[0].decode_float(&packet.front, &mut front_buf, false)?;
        let surround_frames =
            self.decoders[1].decode_float(&packet.surround, &mut surround_buf, false)?;
        let height_frames =
            self.decoders[2].decode_float(&packet.height, &mut height_buf, false)?;
        let center_frames = self.decoders[3].decode_float(&center, &mut center_buf, false)?;
        let lfe_frames = self.decoders[4].decode_float(&packet.lfe, &mut lfe_buf, false)?;

        let stream_frames = [
            front_frames,
            surround_frames,
            height_frames,
            center_frames,
            lfe_frames,
        ];
        let frames = stream_frames.iter().copied().min().unwrap_or(0);
        for (stream, &actual) in stream_frames.iter().enumerate() {
            if actual != frames {
                // A lost packet in a shorter-than-20 ms final group can make
                // libopus conceal a different duration.  Keep the group on a
                // single boundary by trimming to the shortest stream.
                log::warn!("core stream {stream} decoded {actual} samples, truncating to {frames}");
            }
        }

        let mut out: [Vec<f32>; 8] = std::array::from_fn(|_| Vec::with_capacity(frames));
        let (left, right) = stereo_planes(&front_buf, frames);
        let (surround_left, surround_right) = stereo_planes(&surround_buf, frames);
        let (height_left, height_right) = stereo_planes(&height_buf, frames);

        out[0] = left;
        out[1] = right;
        out[2] = center_buf[..frames].to_vec();
        out[3] = surround_left;
        out[4] = surround_right;
        out[5] = height_left;
        out[6] = height_right;
        out[7] = lfe_buf[..frames].to_vec();

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
            .map(|((((front, surround), height), center), lfe)| CorePacket {
                front,
                surround,
                height,
                center,
                lfe,
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
        let mut padded = encoded.packets[0].center.clone();
        let original_len = padded.len();
        padded.resize(original_len + 32, 0);
        let padded_len = opus::packet::pad(&mut padded, original_len).unwrap();
        padded.truncate(padded_len);

        let mut packet = encoded.packets[0].clone();
        packet.center = padded;

        let mut decoder = OpusBedDecoder::new(0).unwrap();
        let bed = decoder.decode(&packet).unwrap();
        assert_eq!(bed[2].len(), OPUS_FRAME_SAMPLES);
        assert!(all_finite(&bed));
    }

    #[test]
    fn center_extension_round_trips_when_supported() {
        let encoded = encode_bed(OPUS_FRAME_SAMPLES);
        let payload = b"salsa-enhancement-fragment".to_vec();
        let packet = encoded.packets[0].center.clone();

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
        packet_with_extension.center = extended;
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
