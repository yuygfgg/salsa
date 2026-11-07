//! Five independent Opus encoders plus the local aligned core decoder.

use anyhow::{Result, bail};
use opus::{Application, Bitrate, Channels, Encoder};
use salsa_spatial::{CorePacket, OPUS_FRAME_SAMPLES, SAMPLE_RATE};
use salsa_spatial_core_decoder::OpusBedDecoder;

const STREAM_COUNT: usize = 5;
const MAX_PACKET_BYTES: usize = 8192;

/// Encoder/decoder pair for the five core substreams.
pub struct CoreCodec {
    encoders: [Encoder; STREAM_COUNT],
    decoder: OpusBedDecoder,
    decoded: DecodedBuffer,
    lookahead: usize,
}

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
            let packet = CorePacket {
                front: self.encoders[0].encode_vec_float(&front, MAX_PACKET_BYTES)?,
                surround: self.encoders[1].encode_vec_float(&surround, MAX_PACKET_BYTES)?,
                height: self.encoders[2].encode_vec_float(&height, MAX_PACKET_BYTES)?,
                center: self.encoders[3].encode_vec_float(&center, MAX_PACKET_BYTES)?,
                lfe: self.encoders[4].encode_vec_float(&lfe_chunk, MAX_PACKET_BYTES)?,
            };
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
        let packet = CorePacket {
            front: self.encoders[0].encode_vec_float(&silence_stereo, MAX_PACKET_BYTES)?,
            surround: self.encoders[1].encode_vec_float(&silence_stereo, MAX_PACKET_BYTES)?,
            height: self.encoders[2].encode_vec_float(&silence_stereo, MAX_PACKET_BYTES)?,
            center: self.encoders[3].encode_vec_float(&silence_mono, MAX_PACKET_BYTES)?,
            lfe: self.encoders[4].encode_vec_float(&silence_mono, MAX_PACKET_BYTES)?,
        };
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
        for channel in 0..7 {
            self.channels[channel].extend_from_slice(&bed[channel][..frames]);
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
