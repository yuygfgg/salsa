//! Five independent Opus encoders plus the local aligned core decoder.

use anyhow::{Result, bail};
use opus::{Application, Bitrate, Channels, Encoder};
use salsa_spatial::{
    CorePacket, MultistreamPacket, OPUS_FRAME_SAMPLES, SAMPLE_RATE, SalsaSpatialStreamConfig,
};
use salsa_spatial_core_decoder::OpusBedDecoder;

use crate::downmix::TransportSignals;

const STREAM_COUNT: usize = 5;
const MAX_PACKET_BYTES: usize = 8192;
/// Encoder target for the shared silence substream. This is a rate-control
/// setting, not the measured payload rate: exact-zero VBR input produces
/// three-byte packets (about 1.2 kbps payload, 1.6 kbps with framing).
const SILENCE_TARGET_BPS: u32 = 16_000;

/// Encoder/decoder pair for the five legacy core substreams.
#[allow(dead_code)]
pub struct CoreCodec {
    encoders: [Encoder; STREAM_COUNT],
    decoder: OpusBedDecoder,
    decoded: DecodedBuffer,
    lookahead: usize,
}

#[allow(dead_code)]
impl CoreCodec {
    pub fn new(object_count: usize, bitrates: [u32; STREAM_COUNT]) -> Result<Self> {
        let mut encoders = [
            build_encoder(Channels::Stereo, bitrates[0])?,
            build_encoder(Channels::Stereo, bitrates[1])?,
            build_encoder(Channels::Stereo, bitrates[2])?,
            build_encoder(Channels::Mono, bitrates[3])?,
            build_encoder(Channels::Mono, bitrates[4])?,
        ];
        // libopus 1.6.1 produces a startup burst for near-silent LFE at 16 kb/s
        // CBR. Unconstrained VBR avoids the burst at the same target bitrate.
        encoders[4].set_vbr(true)?;
        encoders[4].set_vbr_constraint(false)?;
        let decoder = OpusBedDecoder::new(object_count)?;
        let lookahead = decoder.lookahead_samples();
        Ok(Self {
            encoders,
            decoder,
            decoded: DecodedBuffer::default(),
            lookahead,
        })
    }

    pub fn lookahead_samples(&self) -> usize {
        self.lookahead
    }

    /// Encode one tile.  The final partial 20 ms packet is zero padded; the
    /// true tile length is carried by the frame metadata, not the packet.
    pub fn encode_tile(
        &mut self,
        core: &[Vec<f32>; 7],
        lfe: &[f32],
        frames: usize,
    ) -> Result<Vec<CorePacket>> {
        if frames == 0 {
            return Ok(Vec::new());
        }
        let packet_count = frames.div_ceil(OPUS_FRAME_SAMPLES);
        let mut packets = Vec::with_capacity(packet_count);
        for packet_index in 0..packet_count {
            let offset = packet_index * OPUS_FRAME_SAMPLES;
            let front = interleave(padded(&core[0], offset), padded(&core[1], offset));
            let surround = interleave(padded(&core[3], offset), padded(&core[4], offset));
            let height = interleave(padded(&core[5], offset), padded(&core[6], offset));
            let center = padded(&core[2], offset);
            let lfe_chunk = padded(lfe, offset);
            let packet = CorePacket::from_legacy(
                self.encoders[0].encode_vec_float(&front, MAX_PACKET_BYTES)?,
                self.encoders[1].encode_vec_float(&surround, MAX_PACKET_BYTES)?,
                self.encoders[2].encode_vec_float(&height, MAX_PACKET_BYTES)?,
                self.encoders[3].encode_vec_float(&center, MAX_PACKET_BYTES)?,
                self.encoders[4].encode_vec_float(&lfe_chunk, MAX_PACKET_BYTES)?,
            );
            self.append_decoded(&packet)?;
            packets.push(packet);
        }
        Ok(packets)
    }

    /// Encode one extra zero packet per stream without transporting it.  This
    /// supplies the decoder lookahead tail needed to align the final tile to
    /// the source sample clock.
    /// TODO: transport this packet with end trimming when the final packet's
    /// padding is shorter than the encoder lookahead.
    pub fn flush(&mut self) -> Result<()> {
        let silence_stereo = vec![0.0f32; OPUS_FRAME_SAMPLES * 2];
        let silence_mono = vec![0.0f32; OPUS_FRAME_SAMPLES];
        let packet = CorePacket::from_legacy(
            self.encoders[0].encode_vec_float(&silence_stereo, MAX_PACKET_BYTES)?,
            self.encoders[1].encode_vec_float(&silence_stereo, MAX_PACKET_BYTES)?,
            self.encoders[2].encode_vec_float(&silence_stereo, MAX_PACKET_BYTES)?,
            self.encoders[3].encode_vec_float(&silence_mono, MAX_PACKET_BYTES)?,
            self.encoders[4].encode_vec_float(&silence_mono, MAX_PACKET_BYTES)?,
        );
        self.append_decoded(&packet)?;
        Ok(())
    }

    fn append_decoded(&mut self, packet: &CorePacket) -> Result<()> {
        let bed = self.decoder.decode(packet)?;
        self.decoded.push(&bed);
        Ok(())
    }

    /// Extract decoded full-range core samples aligned to the program clock.
    ///
    /// `start_sample` is relative to the encoded program start.
    pub fn take_aligned(&mut self, start_sample: u64, frames: usize) -> Result<[Vec<f32>; 7]> {
        let start = start_sample.saturating_add(self.lookahead as u64);
        let Some(slice) = self.decoded.slice(start, frames) else {
            bail!(
                "local Opus decode is short: need raw samples {start}..{}",
                start + frames as u64
            );
        };
        self.decoded.drain_before(start);
        Ok(slice)
    }
}

/// Opus encoders and the local decoder for the multistream profile.
///
/// Every substream is encoded independently so that the shared rear-silence
/// signal can use its own VBR rate-control policy.
pub struct MultistreamCodec {
    #[allow(dead_code)]
    config: SalsaSpatialStreamConfig,
    encoders: Vec<Encoder>,
    substream_channels: Vec<usize>,
    decoder: OpusBedDecoder,
    decoded: DecodedBuffer,
    lookahead: usize,
}

impl MultistreamCodec {
    pub fn new(config: SalsaSpatialStreamConfig) -> Result<Self> {
        config.validate()?;
        let binding = &config.codec;
        if binding.stream_count != 6 || binding.coupled_count != 3 {
            bail!(
                "the initial encoder profile requires six substreams and three coupled substreams"
            );
        }
        let mut encoders = Vec::with_capacity(binding.stream_count);
        let mut substream_channels = Vec::with_capacity(binding.stream_count);
        for stream in 0..binding.stream_count {
            let channels = if stream < binding.coupled_count { 2 } else { 1 };
            let opus_channels = if channels == 2 {
                Channels::Stereo
            } else {
                Channels::Mono
            };
            let bitrate = match stream {
                0 => config.core_bitrates[0],
                1 => config.core_bitrates[1],
                2 => config.core_bitrates[2],
                3 => config.core_bitrates[3],
                4 => config.core_bitrates[4],
                _ => SILENCE_TARGET_BPS,
            };
            let mut encoder = build_encoder(opus_channels, bitrate)?;
            if stream >= 5 {
                // Shared rear silence: unconstrained VBR avoids CBR padding.
                encoder.set_vbr(true)?;
                encoder.set_vbr_constraint(false)?;
            }
            encoders.push(encoder);
            substream_channels.push(channels);
        }
        let decoder = OpusBedDecoder::from_config(&config)?;
        let lookahead = decoder.lookahead_samples();
        Ok(Self {
            config,
            encoders,
            substream_channels,
            decoder,
            decoded: DecodedBuffer::default(),
            lookahead,
        })
    }

    #[allow(dead_code)]
    pub fn config(&self) -> &SalsaSpatialStreamConfig {
        &self.config
    }

    pub fn lookahead_samples(&self) -> usize {
        self.lookahead
    }

    /// Encode one tile of transport signals into ordered substream packets.
    pub fn encode_tile(&mut self, signals: &TransportSignals) -> Result<Vec<MultistreamPacket>> {
        let frames = signals.frames;
        if frames == 0 {
            return Ok(Vec::new());
        }
        if signals.aux.len() < 2 {
            bail!("the initial profile requires two auxiliary observations");
        }
        let packet_count = frames.div_ceil(OPUS_FRAME_SAMPLES);
        let mut packets = Vec::with_capacity(packet_count);
        for packet_index in 0..packet_count {
            let offset = packet_index * OPUS_FRAME_SAMPLES;
            let front = interleave(
                padded(&signals.base[0], offset),
                padded(&signals.base[1], offset),
            );
            let surround = interleave(
                padded(&signals.base[3], offset),
                padded(&signals.base[4], offset),
            );
            let auxiliary = interleave(
                padded(&signals.aux[0], offset),
                padded(&signals.aux[1], offset),
            );
            let center = padded(&signals.base[2], offset);
            let lfe = padded(&signals.lfe, offset);
            let silence = vec![0.0f32; OPUS_FRAME_SAMPLES];
            let substreams = vec![
                self.encoders[0].encode_vec_float(&front, MAX_PACKET_BYTES)?,
                self.encoders[1].encode_vec_float(&surround, MAX_PACKET_BYTES)?,
                self.encoders[2].encode_vec_float(&auxiliary, MAX_PACKET_BYTES)?,
                self.encoders[3].encode_vec_float(&center, MAX_PACKET_BYTES)?,
                self.encoders[4].encode_vec_float(&lfe, MAX_PACKET_BYTES)?,
                self.encoders[5].encode_vec_float(&silence, MAX_PACKET_BYTES)?,
            ];
            let packet = MultistreamPacket::new(substreams);
            self.append_decoded(&packet)?;
            packets.push(packet);
        }
        Ok(packets)
    }

    /// Advance the local decoder by one silent packet per substream and return
    /// the transported tail packet for end trimming.
    pub fn flush(&mut self) -> Result<Option<MultistreamPacket>> {
        let mut substreams = Vec::with_capacity(self.encoders.len());
        for (encoder, channels) in self.encoders.iter_mut().zip(&self.substream_channels) {
            let silence = vec![0.0f32; OPUS_FRAME_SAMPLES * channels];
            substreams.push(encoder.encode_vec_float(&silence, MAX_PACKET_BYTES)?);
        }
        let packet = MultistreamPacket::new(substreams);
        self.append_decoded(&packet)?;
        Ok(Some(packet))
    }

    fn append_decoded(&mut self, packet: &MultistreamPacket) -> Result<()> {
        let bed = self.decoder.decode(packet)?;
        self.decoded.push(&bed);
        Ok(())
    }

    /// Extract decoded full-range observations aligned to the program clock.
    pub fn take_aligned(&mut self, start_sample: u64, frames: usize) -> Result<[Vec<f32>; 7]> {
        let start = start_sample.saturating_add(self.lookahead as u64);
        let Some(slice) = self.decoded.slice(start, frames) else {
            bail!(
                "local Opus decode is short: need raw samples {start}..{}",
                start + frames as u64
            );
        };
        self.decoded.drain_before(start);
        Ok(slice)
    }
}

fn build_encoder(channels: Channels, bitrate: u32) -> Result<Encoder> {
    let mut encoder = Encoder::new(SAMPLE_RATE, channels, Application::LowDelay)?;
    encoder.set_complexity(10)?;
    encoder.set_bitrate(Bitrate::Bits(bitrate as i32))?;
    encoder.set_vbr(false)?;
    encoder.set_vbr_constraint(true)?;
    encoder.set_dtx(false)?;
    encoder.set_inband_fec(false)?;
    // DRED is not exposed by `opus` 0.4.0 (libopus 1.6.1), so it remains off.
    Ok(encoder)
}

fn padded(channel: &[f32], offset: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; OPUS_FRAME_SAMPLES];
    if offset < channel.len() {
        let end = (offset + OPUS_FRAME_SAMPLES).min(channel.len());
        out[..end - offset].copy_from_slice(&channel[offset..end]);
    }
    out
}

fn interleave(left: Vec<f32>, right: Vec<f32>) -> Vec<f32> {
    let mut out = Vec::with_capacity(left.len() * 2);
    for (l, r) in left.into_iter().zip(right) {
        out.push(l);
        out.push(r);
    }
    out
}

#[derive(Default)]
struct DecodedBuffer {
    channels: [Vec<f32>; 7],
    base: u64,
}

impl DecodedBuffer {
    fn push(&mut self, bed: &[Vec<f32>; 8]) {
        let frames = bed.iter().map(Vec::len).min().unwrap_or(0);
        for (target, source) in self.channels.iter_mut().zip(bed) {
            target.extend_from_slice(&source[..frames]);
        }
    }

    fn slice(&self, start: u64, frames: usize) -> Option<[Vec<f32>; 7]> {
        let offset = start.checked_sub(self.base)? as usize;
        if offset + frames > self.channels[0].len() {
            return None;
        }
        Some(std::array::from_fn(|channel| {
            self.channels[channel][offset..offset + frames].to_vec()
        }))
    }

    fn drain_before(&mut self, sample: u64) {
        if sample <= self.base {
            return;
        }
        let count = (sample - self.base) as usize;
        for channel in &mut self.channels {
            let count = count.min(channel.len());
            channel.drain(..count);
        }
        self.base = sample;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frequency: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|index| {
                let t = index as f64 / f64::from(SAMPLE_RATE);
                (0.2 * (2.0 * std::f64::consts::PI * frequency * t).sin()) as f32
            })
            .collect()
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

    #[test]
    fn opus_core_roundtrip_aligns_and_correlates() {
        let frames = OPUS_FRAME_SAMPLES;
        let signal = tone(440.0, frames);
        let core: [Vec<f32>; 7] = std::array::from_fn(|channel| {
            if channel == 1 || channel == 2 {
                signal.clone()
            } else {
                vec![0.0; frames]
            }
        });
        let lfe = tone(80.0, frames);
        let mut codec = CoreCodec::new(1, [160_000, 128_000, 160_000, 112_000, 16_000]).unwrap();
        let packets = codec.encode_tile(&core, &lfe, frames).unwrap();
        assert_eq!(packets.len(), 1);
        codec.flush().unwrap();
        let decoded = codec.take_aligned(0, frames).unwrap();
        assert!(correlation(&decoded[1], &signal) > 0.7);
    }

    #[test]
    fn multistream_routes_auxiliary_and_lfe_and_keeps_silence_small() {
        let frames = OPUS_FRAME_SAMPLES;
        let config = SalsaSpatialStreamConfig {
            object_count: 0,
            ..SalsaSpatialStreamConfig::default()
        };
        let mut codec = MultistreamCodec::new(config.clone()).unwrap();
        let signals = TransportSignals {
            base: std::array::from_fn(|channel| tone(200.0 * (channel as f64 + 1.0), frames)),
            aux: vec![tone(700.0, frames), tone(800.0, frames)],
            lfe: tone(60.0, frames),
            frames,
        };
        let packets = codec.encode_tile(&signals).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), 6);
        assert!(
            packets[0].substreams[5].len() <= 4,
            "silence packet is not tiny"
        );
        codec.flush().unwrap();

        let mut decoder = OpusBedDecoder::from_config(&config).unwrap();
        let decoded = decoder.decode(&packets[0]).unwrap();
        assert_eq!(decoded[0].len(), frames);
        let lookahead = decoder.lookahead_samples().min(frames);
        for (channel, expected) in decoded.iter().enumerate() {
            let reference = if channel < 5 {
                &signals.base[channel]
            } else if channel < 7 {
                &signals.aux[channel - 5]
            } else {
                &signals.lfe
            };
            assert!(
                correlation(&expected[lookahead..], &reference[..frames - lookahead]) > 0.8,
                "channel {channel}"
            );
        }
    }

    #[test]
    fn multistream_silence_is_shared_by_both_rear_outputs() {
        let config = SalsaSpatialStreamConfig {
            object_count: 0,
            ..SalsaSpatialStreamConfig::default()
        };
        let mapping = config.codec.channel_mapping.clone();
        assert_eq!(mapping[5], mapping[6]);
        assert_eq!(
            mapping[5],
            config.codec.logical_to_coded[config.codec.silence_logical_index()]
        );
    }

    #[test]
    fn quiet_lfe_does_not_produce_a_startup_burst() {
        let frames = 3 * OPUS_FRAME_SAMPLES;
        let core = std::array::from_fn(|_| vec![0.0; frames]);
        let lfe: Vec<f32> = (0..frames)
            .map(|i| if i % 7 < 3 { 1.0e-7 } else { -1.0e-7 })
            .collect();
        for bitrate in [16_000, 24_000, 32_000, 48_000] {
            let mut codec =
                CoreCodec::new(1, [160_000, 128_000, 160_000, 112_000, bitrate]).unwrap();
            let packets = codec.encode_tile(&core, &lfe, frames).unwrap();
            let mut decoder = OpusBedDecoder::new(1).unwrap();
            let peak = packets
                .iter()
                .flat_map(|packet| decoder.decode(packet).unwrap()[7].clone())
                .map(f32::abs)
                .fold(0.0f32, f32::max);
            assert!(
                peak < 2.0e-6,
                "near-silent LFE at {bitrate} bits/s produced a peak of {peak}"
            );
        }
    }
}
