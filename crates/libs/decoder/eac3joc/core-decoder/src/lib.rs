//! E-AC-3 core PCM decoding through `libavcodec` (`ffmpeg-next`).
//!
//! The decoder deliberately does not parse EMDF/JOC. It accepts the raw
//! syncframes of one audio frame, decodes the 5.1/7.1 core PCM with libavcodec
//! and returns planar `f32` samples.

use std::sync::Once;

use ffmpeg_next as ffmpeg;

use ffmpeg::Packet;
use ffmpeg::format::Sample;
use ffmpeg::frame::Audio;
use ffmpeg::util::format::sample::Type as SampleType;
use thiserror::Error;

static INIT: Once = Once::new();

#[derive(Debug, Error)]
pub enum CoreDecoderError {
    #[error("the libavcodec E-AC-3 decoder is not available: {0}")]
    Ffmpeg(#[from] ffmpeg::Error),

    #[error("libavcodec returned {channels} channels; expected a 5.1 or 7.1 core")]
    UnsupportedChannelCount { channels: usize },

    #[error("libavcodec returned sample format {0:?}; expected planar f32")]
    UnsupportedSampleFormat(Sample),

    #[error("decoded {actual} samples for a {expected}-sample audio frame")]
    SampleCountMismatch { expected: usize, actual: usize },

    #[error("the core decoder produced {actual} channels, the frame declared {declared}")]
    ChannelMismatch { declared: usize, actual: usize },
}

/// Planar `f32` PCM for one or more decoded frames.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CorePcm {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: usize,
    pub planes: Vec<Vec<f32>>,
}

impl CorePcm {
    pub fn interleave(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.samples * self.channels);
        for sample in 0..self.samples {
            for plane in &self.planes {
                out.push(plane[sample]);
            }
        }
        out
    }

    fn append(&mut self, other: CorePcm) {
        if self.channels == 0 {
            self.sample_rate = other.sample_rate;
            self.channels = other.channels;
            self.planes = other.planes;
            self.samples = other.samples;
            return;
        }
        for (destination, source) in self.planes.iter_mut().zip(other.planes) {
            destination.extend_from_slice(&source);
        }
        self.samples += other.samples;
    }
}

/// A libavcodec E-AC-3 decoder instance.
pub struct CoreDecoder {
    inner: ffmpeg::codec::decoder::Audio,
}

impl CoreDecoder {
    pub fn new() -> Result<Self, CoreDecoderError> {
        INIT.call_once(|| {
            let _ = ffmpeg::init();
        });
        let codec = ffmpeg::decoder::find_by_name("eac3")
            .or_else(|| ffmpeg::decoder::find(ffmpeg::codec::Id::EAC3))
            .ok_or(ffmpeg::Error::DecoderNotFound)?;
        let mut options = ffmpeg::Dictionary::new();
        // Match the reference pipeline: no dynrng, no target-level normalisation.
        options.set("drc_scale", "0");
        options.set("target_level", "0");
        let decoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .decoder()
            .open_as_with(codec, options)?
            .audio()?;
        Ok(Self { inner: decoder })
    }

    /// Decode the syncframes of one audio frame (already concatenated or as
    /// separate packets) into planar PCM.
    pub fn decode_packets(&mut self, packets: &[Vec<u8>]) -> Result<CorePcm, CoreDecoderError> {
        let mut pcm = CorePcm::default();
        for bytes in packets {
            let packet = Packet::copy(bytes);
            self.inner.send_packet(&packet)?;
            pcm.append(self.receive_frames()?);
        }
        Ok(pcm)
    }

    /// Decode a single packet (normally every syncframe of an audio frame).
    pub fn decode(&mut self, bytes: &[u8]) -> Result<CorePcm, CoreDecoderError> {
        let packet = Packet::copy(bytes);
        self.inner.send_packet(&packet)?;
        self.receive_frames()
    }

    /// Drain delayed frames after the last packet.
    pub fn flush(&mut self) -> Result<CorePcm, CoreDecoderError> {
        self.inner.send_eof()?;
        self.receive_frames()
    }

    fn receive_frames(&mut self) -> Result<CorePcm, CoreDecoderError> {
        let mut pcm = CorePcm::default();
        loop {
            let mut frame = Audio::empty();
            match self.inner.receive_frame(&mut frame) {
                Ok(()) => pcm.append(frame_to_pcm(&frame)?),
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                    break;
                }
                Err(ffmpeg::Error::Eof) => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(pcm)
    }
}

fn frame_to_pcm(frame: &Audio) -> Result<CorePcm, CoreDecoderError> {
    let channels = usize::from(frame.channels());
    if channels != 6 && channels != 8 {
        return Err(CoreDecoderError::UnsupportedChannelCount { channels });
    }
    let sample_rate = frame.rate();
    let samples = frame.samples();
    let mut pcm = CorePcm {
        sample_rate,
        channels,
        samples,
        planes: Vec::with_capacity(channels),
    };
    match frame.format() {
        Sample::F32(SampleType::Planar) => {
            for channel in 0..channels {
                pcm.planes.push(frame.plane::<f32>(channel).to_vec());
            }
        }
        Sample::F32(SampleType::Packed) => {
            let data = frame.plane::<f32>(0);
            for channel in 0..channels {
                pcm.planes
                    .push(data[channel..].iter().step_by(channels).copied().collect());
            }
        }
        other => return Err(CoreDecoderError::UnsupportedSampleFormat(other)),
    }
    Ok(pcm)
}

/// Resample/reorder a decoded 5.1/7.1 core into `channels` lanes.
pub fn select_lanes(pcm: &CorePcm, lanes: &[Option<usize>], samples: usize) -> Vec<Vec<f32>> {
    let mut out = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let mut plane = vec![0.0f32; samples];
        if let Some(source) = lane
            && let Some(input) = pcm.planes.get(*source)
        {
            let count = samples.min(input.len());
            plane[..count].copy_from_slice(&input[..count]);
        }
        out.push(plane);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_is_channel_major() {
        let pcm = CorePcm {
            sample_rate: 48_000,
            channels: 2,
            samples: 2,
            planes: vec![vec![1.0, 3.0], vec![2.0, 4.0]],
        };
        assert_eq!(pcm.interleave(), vec![1.0, 2.0, 3.0, 4.0]);
    }
}
