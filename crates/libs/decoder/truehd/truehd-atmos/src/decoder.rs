//! Decode stage: `truehd::Decoder` presentation 3 -> `SpatialFrame`.

use objectaudio_ir::{
    IrError, ObjectState, PcmBuffer, SpatialDecoder, SpatialFrame, SpatialPayload, SpeakerPosition,
};
use objectaudio_oamd::OamdFrame;
use truehd::process::decode::Decoder;
use truehd::structs::channel::ChannelLabel;

use crate::{TrueHdError, TrueHdFrame};

/// The object presentation of an Atmos TrueHD stream.
pub const OBJECT_PRESENTATION: usize = 3;

/// Full-scale divisor for the 24-bit PCM the `truehd` decoder returns.
pub const PCM_SCALE: f32 = 8_388_608.0;

/// Maximum channels the object presentation can carry.
pub const MAX_CHANNELS: usize = 16;

/// The spatial payload and placement of the most recently decoded access unit.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialSnapshot {
    pub payload: SpatialPayload,
    /// Snapshot offset inside the access unit, in samples.
    pub offset: u32,
}

/// TrueHD Atmos decoder: consumes parsed access units and emits the shared IR.
pub struct TrueHdDecoder {
    decoder: Decoder,
    oamd: Option<OamdFrame>,
    oamd_base_offset: u32,
    snapshot: Option<SpatialSnapshot>,
}

impl Default for TrueHdDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TrueHdDecoder {
    pub fn new() -> Self {
        Self {
            decoder: Decoder::default(),
            oamd: None,
            oamd_base_offset: 0,
            snapshot: None,
        }
    }

    /// Drop all stream state. The next major sync reinitialises the decoder.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Take the spatial payload carried by the access unit just decoded.
    pub fn take_snapshot(&mut self) -> Option<SpatialSnapshot> {
        self.snapshot.take()
    }

    fn apply_oamd(&mut self, frame: &OamdFrame, base_offset: u32) {
        self.oamd = Some(frame.clone());
        self.oamd_base_offset = base_offset;
    }
}

impl SpatialDecoder for TrueHdDecoder {
    type Frame = TrueHdFrame;
    type Error = TrueHdError;

    fn decode(&mut self, frame: &Self::Frame) -> Result<SpatialFrame, Self::Error> {
        self.snapshot = None;

        let mut saw_oamd = false;
        for payload in &frame.oamd {
            let parsed = objectaudio_oamd::parse(&payload.bytes)?;
            self.apply_oamd(&parsed, payload.evo_sample_offset);
            saw_oamd = true;
        }

        let decoded = self
            .decoder
            .decode_presentation(&frame.access_unit, OBJECT_PRESENTATION)
            .map_err(TrueHdError::Decode)?;

        if decoded.channel_count > MAX_CHANNELS {
            return Err(TrueHdError::ChannelCount {
                channels: decoded.channel_count,
                max: MAX_CHANNELS,
            });
        }
        let frames = decoded.sample_length;
        if frames > decoded.pcm_data.len() {
            return Err(TrueHdError::SampleLength {
                expected: decoded.pcm_data.len(),
                actual: frames,
            });
        }

        let channel_count = decoded.channel_count;
        let labels = &decoded.channel_labels;
        let label_count = labels.len().min(channel_count);
        let declared_objects = channel_count - label_count;
        let lfe_label = labels
            .iter()
            .take(label_count)
            .position(|label| *label == ChannelLabel::LFE)
            .ok_or(IrError::MissingLfe)?;

        // The presentation bed is authoritative for TrueHD. Give the shared
        // OAMD mapping the same channel order the PCM planes use.
        let bed_positions: Vec<SpeakerPosition> = labels
            .iter()
            .take(label_count)
            .enumerate()
            .filter(|(index, _)| *index != lfe_label)
            .map(|(_, label)| bed_position(*label))
            .collect();

        let mapping = self.oamd.as_ref().map(|oamd| {
            let base = if saw_oamd { self.oamd_base_offset } else { 0 };
            oamd.map_to_ir_with_bed(base, &bed_positions)
        });
        let dynamic_count = mapping
            .as_ref()
            .map_or(declared_objects, |mapping| mapping.dynamic_count);
        let object_count = if saw_oamd {
            dynamic_count.min(declared_objects)
        } else {
            declared_objects
        };

        let mut planes = Vec::with_capacity(1 + object_count + bed_positions.len());
        planes.push(plane(&decoded.pcm_data, lfe_label, frames));

        let mut objects = Vec::with_capacity(object_count + bed_positions.len());
        if let Some(mapping) = &mapping {
            objects.extend(mapping.objects.iter().take(object_count).cloned());
        }
        for id in objects.len() as u32 + 1..=object_count as u32 {
            objects.push(ObjectState::new(id, [0.0, 0.0, 0.0]));
        }

        for id in 1..=object_count as u32 {
            let channel = label_count + id as usize - 1;
            if channel >= channel_count {
                break;
            }
            planes.push(plane(&decoded.pcm_data, channel, frames));
        }

        // The shared mapping appended one bed object per presentation bed
        // channel; renumber them after the (possibly truncated) dynamic set.
        let bed_states: Vec<ObjectState> = match &mapping {
            Some(mapping) => mapping.objects[mapping.dynamic_count..].to_vec(),
            None => bed_positions
                .iter()
                .map(|position| ObjectState::new(0, position.adm_position()))
                .collect(),
        };
        for (offset, mut state) in bed_states.into_iter().enumerate() {
            state.id = object_count as u32 + 1 + offset as u32;
            objects.push(state);
        }
        for (index, _) in labels.iter().take(label_count).enumerate() {
            if index == lfe_label {
                continue;
            }
            planes.push(plane(&decoded.pcm_data, index, frames));
        }

        let events = mapping
            .as_ref()
            .map(|mapping| {
                mapping
                    .events
                    .iter()
                    .filter(|event| {
                        saw_oamd
                            && event.object <= object_count as u32
                            && event.offset as usize <= frames
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let pcm = PcmBuffer::from_planar(planes)?;
        let sample_rate = decoded.sampling_frequency;
        let spatial = SpatialFrame::new(sample_rate, frames, objects, events, pcm)?;
        let payload = spatial.to_payload();
        self.snapshot = Some(SpatialSnapshot {
            payload,
            offset: if saw_oamd {
                mapping
                    .as_ref()
                    .map_or(0, |mapping| mapping.snapshot_offset)
            } else {
                0
            },
        });
        Ok(spatial)
    }
}

fn plane(pcm_data: &[[i32; 16]], channel: usize, frames: usize) -> Vec<f32> {
    let mut plane = Vec::with_capacity(frames);
    for sample in pcm_data.iter().take(frames) {
        plane.push((sample[channel] as f32) / PCM_SCALE);
    }
    plane
}

fn bed_position(label: ChannelLabel) -> SpeakerPosition {
    bed_channel(label).unwrap_or(SpeakerPosition::Center)
}

fn bed_channel(label: ChannelLabel) -> Option<SpeakerPosition> {
    use ChannelLabel as L;
    Some(match label {
        L::L => SpeakerPosition::Left,
        L::R => SpeakerPosition::Right,
        L::C => SpeakerPosition::Center,
        L::LFE => SpeakerPosition::Lfe,
        L::Ls => SpeakerPosition::LeftSurround,
        L::Rs => SpeakerPosition::RightSurround,
        L::Lb => SpeakerPosition::LeftRearSurround,
        L::Rb => SpeakerPosition::RightRearSurround,
        L::Tfl => SpeakerPosition::TopFrontLeft,
        L::Tfr => SpeakerPosition::TopFrontRight,
        L::Tsl => SpeakerPosition::TopSideLeft,
        L::Tsr => SpeakerPosition::TopSideRight,
        L::Tbl => SpeakerPosition::TopBackLeft,
        L::Tbr => SpeakerPosition::TopBackRight,
        L::Lw => SpeakerPosition::LeftWide,
        L::Rw => SpeakerPosition::RightWide,
        L::LFE2 => SpeakerPosition::Lfe2,
        // Lsc/Rsc/Cb/Tc/Lsd/Rsd/Tfc have no canonical IR position; keep the
        // PCM channel but place it at a neutral object position.
        L::Lsc | L::Rsc | L::Cb | L::Tc | L::Lsd | L::Rsd | L::Tfc => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bed_label_maps_or_is_explicit_aux() {
        assert_eq!(bed_channel(ChannelLabel::L), Some(SpeakerPosition::Left));
        assert_eq!(bed_channel(ChannelLabel::LFE), Some(SpeakerPosition::Lfe));
        assert_eq!(
            bed_channel(ChannelLabel::Tfl),
            Some(SpeakerPosition::TopFrontLeft)
        );
        assert_eq!(bed_channel(ChannelLabel::Cb), None);
        assert_eq!(bed_position(ChannelLabel::Cb), SpeakerPosition::Center);
    }

    #[test]
    fn the_object_presentation_is_three() {
        assert_eq!(OBJECT_PRESENTATION, 3);
    }

    #[test]
    fn oamd_objects_are_rebased_to_contiguous_ids() {
        let parsed = objectaudio_oamd::parse(objectaudio_oamd::EXAMPLE_DATA).unwrap();
        let mapping = parsed.map_to_ir(0, objectaudio_oamd::BedPolicy::Ignore);
        let ids = mapping
            .objects
            .iter()
            .map(|state| state.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, (1..=15).collect::<Vec<_>>());
    }
}
