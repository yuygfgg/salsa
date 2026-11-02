use crate::IrError;

/// Planar `f32` PCM. All channels have the same number of samples.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PcmBuffer {
    channels: Vec<Vec<f32>>,
}

impl PcmBuffer {
    /// Allocate a zeroed planar buffer.
    pub fn silent(channel_count: usize, frames: usize) -> Self {
        Self {
            channels: (0..channel_count).map(|_| vec![0.0; frames]).collect(),
        }
    }

    /// Build from planar channel vectors. Channel lengths must match.
    pub fn from_planar(channels: Vec<Vec<f32>>) -> Result<Self, IrError> {
        if let Some(first) = channels.first() {
            let frames = first.len();
            for (channel, samples) in channels.iter().enumerate() {
                if samples.len() != frames {
                    return Err(IrError::UnevenChannelLengths {
                        channel,
                        expected: frames,
                        actual: samples.len(),
                    });
                }
            }
        }
        Ok(Self { channels })
    }

    /// Deinterleave `samples` into `channel_count` planar channels.
    pub fn from_interleaved(samples: &[f32], channel_count: usize) -> Result<Self, IrError> {
        if channel_count == 0 {
            if samples.is_empty() {
                return Ok(Self::default());
            }
            return Err(IrError::InterleavedLengthMismatch {
                len: samples.len(),
                channels: 0,
            });
        }
        if !samples.len().is_multiple_of(channel_count) {
            return Err(IrError::InterleavedLengthMismatch {
                len: samples.len(),
                channels: channel_count,
            });
        }

        let frames = samples.len() / channel_count;
        let mut channels: Vec<Vec<f32>> = (0..channel_count)
            .map(|_| Vec::with_capacity(frames))
            .collect();
        for frame in 0..frames {
            let base = frame * channel_count;
            for (channel_index, channel) in channels.iter_mut().enumerate() {
                channel.push(samples[base + channel_index]);
            }
        }
        Ok(Self { channels })
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty() || self.frames() == 0
    }

    pub fn channel(&self, index: usize) -> Option<&[f32]> {
        self.channels.get(index).map(Vec::as_slice)
    }

    pub fn channel_mut(&mut self, index: usize) -> Option<&mut [f32]> {
        self.channels.get_mut(index).map(Vec::as_mut_slice)
    }

    pub fn channels(&self) -> &[Vec<f32>] {
        &self.channels
    }

    pub fn channels_mut(&mut self) -> impl Iterator<Item = &mut [f32]> {
        self.channels.iter_mut().map(Vec::as_mut_slice)
    }

    /// Interleave into `[frame0 ch0, frame0 ch1, ..., frame1 ch0, ...]`.
    pub fn interleave(&self) -> Vec<f32> {
        let frames = self.frames();
        let mut out = Vec::with_capacity(frames * self.channel_count());
        for frame in 0..frames {
            for channel in &self.channels {
                out.push(channel[frame]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn interleave_roundtrip() {
        let planar = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]];
        let buffer = PcmBuffer::from_planar(planar.clone()).unwrap();
        assert_eq!(buffer.frames(), 3);
        assert_eq!(buffer.channel_count(), 2);
        assert_eq!(buffer.interleave(), vec![1.0, 4.0, 2.0, 5.0, 3.0, 6.0]);
        let roundtrip = PcmBuffer::from_interleaved(&buffer.interleave(), 2).unwrap();
        assert_eq!(roundtrip, buffer);
    }

    #[test]
    fn rejects_uneven_channels() {
        let error = PcmBuffer::from_planar(vec![vec![0.0], vec![0.0, 1.0]]).unwrap_err();
        assert_eq!(
            error,
            IrError::UnevenChannelLengths {
                channel: 1,
                expected: 1,
                actual: 2,
            }
        );
    }

    #[test]
    fn rejects_partial_interleaved_frame() {
        let error = PcmBuffer::from_interleaved(&[0.0, 1.0, 2.0], 2).unwrap_err();
        assert_eq!(
            error,
            IrError::InterleavedLengthMismatch {
                len: 3,
                channels: 2,
            }
        );
    }

    proptest! {
        #[test]
        fn planar_interleave_roundtrip(
            channel_count in 1usize..8,
            frames in 0usize..64,
            values in prop::collection::vec(-1.0f32..1.0, 0..512),
        ) {
            let total = channel_count * frames;
            let samples: Vec<f32> = (0..total)
                .map(|index| {
                    values
                        .get(index % values.len().max(1))
                        .copied()
                        .unwrap_or(0.0)
                })
                .collect();
            let buffer = PcmBuffer::from_interleaved(&samples, channel_count).unwrap();
            prop_assert_eq!(buffer.channel_count(), channel_count);
            prop_assert_eq!(buffer.frames(), frames);
            prop_assert_eq!(buffer.interleave(), samples);
        }
    }
}
