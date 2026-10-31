//! High-precision object-to-speaker renderer.
//!
//! Input is interleaved `f32` with channel 0 = LFE and the remaining channels
//! are point objects. All panning, ramps and accumulation use `f64`.

use crate::layout::{
    AxisGroup, MAX_CHANNELS, MAX_GROUPS, MAX_POINTS, RegionGeometry, SpeakerLayout,
};

/// Renderer block size; `sample_count` must be a multiple of this.
pub const BLOCK_SAMPLES: usize = objectaudio_ir::METADATA_BLOCK_SAMPLES;

const Q15_SCALE: f64 = 32768.0;
const Q15_MAX: f64 = 32767.0 / Q15_SCALE;
const GAIN_SNAP_THRESHOLD: f64 = 1.0e-4;

/// A complete object-state snapshot at one sample offset.
///
/// The per-object arrays are borrowed slices so a renderer can consume an
/// arbitrary object count without copying a fixed-size buffer per event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeakerMetadata<'a> {
    /// Offset in samples from the start of this call.
    pub offset: u32,
    /// Ramp length in samples. Zero means an immediate change.
    pub ramp_samples: u32,
    /// Q15 positions for PCM channels 1..=object_count.
    pub positions_q15: &'a [[u16; 3]],
    /// Optional region index per object; defaults to region 0.
    pub region_indices: Option<&'a [u8]>,
    /// Optional elevation enable per object; defaults to enabled.
    pub height_enabled: Option<&'a [u8]>,
    /// Optional linear object gain; defaults to 1.0.
    pub object_gains: Option<&'a [f64]>,
}

impl<'a> SpeakerMetadata<'a> {
    pub fn new(offset: u32, ramp_samples: u32, positions_q15: &'a [[u16; 3]]) -> Self {
        Self {
            offset,
            ramp_samples,
            positions_q15,
            region_indices: None,
            height_enabled: None,
            object_gains: None,
        }
    }
}

/// Errors returned by [`SpeakerRenderer::process`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("input must contain sample_count * (object_count + 1) samples")]
    InputSize,
    #[error("output must contain sample_count * {0} samples")]
    OutputSize(usize),
    #[error("sample_count must be a multiple of 32")]
    BlockAlignment,
    #[error("metadata offset {offset} is past sample_count {sample_count}")]
    OffsetOutOfRange { offset: u32, sample_count: usize },
    #[error("metadata offsets must be nondecreasing")]
    OffsetsNotSorted,
    #[error("metadata has {actual} objects, the renderer expects {expected}")]
    ObjectCount { expected: usize, actual: usize },
    #[error("region index {0} is above 6")]
    RegionIndex(u8),
    #[error("object gain is not finite")]
    NonFiniteGain,
    #[error("Q15 object coordinate is above 32767")]
    CoordinateRange,
}

type PointGains = [f64; MAX_POINTS];
type ChannelGains = [f64; MAX_CHANNELS];
type ObjectChannelGains = [f64; MAX_CHANNELS];
type RemainingCounts = [u32; MAX_CHANNELS];

/// Stateful speaker renderer for one layout.
pub struct SpeakerRenderer {
    layout: &'static SpeakerLayout,
    attenuation_db: f64,
    floor_y_exponent: i32,
    object_count: usize,
    current: Vec<ObjectChannelGains>,
    target: Vec<ObjectChannelGains>,
    step: Vec<ObjectChannelGains>,
    remaining: Vec<RemainingCounts>,
    standard_index_for_internal: [u8; MAX_CHANNELS],
}

impl std::fmt::Debug for SpeakerRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeakerRenderer")
            .field("layout", &self.layout.name)
            .field("attenuation_db", &self.attenuation_db)
            .field("object_count", &self.object_count)
            .finish_non_exhaustive()
    }
}

impl SpeakerRenderer {
    /// Create a renderer for `object_count` point objects plus the LFE channel.
    ///
    /// `object_count == 0` is valid and renders LFE only.
    pub fn new(layout: &'static SpeakerLayout, object_count: usize) -> Self {
        let mut standard_index_for_internal = [0u8; MAX_CHANNELS];
        for standard in 0..layout.channel_count() {
            standard_index_for_internal[usize::from(layout.standard_from_internal[standard])] =
                standard as u8;
        }
        let mut renderer = Self {
            layout,
            attenuation_db: layout_attenuation_db(layout.speaker_bitfield),
            floor_y_exponent: floor_y_exponent(layout.speaker_bitfield),
            object_count,
            current: vec![[0.0; MAX_CHANNELS]; object_count],
            target: vec![[0.0; MAX_CHANNELS]; object_count],
            step: vec![[0.0; MAX_CHANNELS]; object_count],
            remaining: vec![[0; MAX_CHANNELS]; object_count],
            standard_index_for_internal,
        };
        renderer.reset();
        renderer
    }

    pub fn layout(&self) -> &'static SpeakerLayout {
        self.layout
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn input_channels(&self) -> usize {
        self.object_count + 1
    }

    pub fn reset(&mut self) {
        self.current.fill([0.0; MAX_CHANNELS]);
        self.target.fill([0.0; MAX_CHANNELS]);
        self.step.fill([0.0; MAX_CHANNELS]);
        self.remaining.fill([0; MAX_CHANNELS]);
    }

    /// Render `sample_count` samples.
    ///
    /// `input` is interleaved `[sample][object_count + 1]` with the LFE first;
    /// `output` is interleaved `[sample][layout.channel_count()]` and is
    /// overwritten.
    pub fn process(
        &mut self,
        input: &[f32],
        sample_count: usize,
        metadata: &[SpeakerMetadata<'_>],
        output: &mut [f64],
    ) -> Result<(), RenderError> {
        if !sample_count.is_multiple_of(BLOCK_SAMPLES) {
            return Err(RenderError::BlockAlignment);
        }
        if input.len() < sample_count * self.input_channels() {
            return Err(RenderError::InputSize);
        }
        let channel_count = self.layout.channel_count();
        if output.len() < sample_count * channel_count {
            return Err(RenderError::OutputSize(channel_count));
        }
        self.validate_metadata(sample_count, metadata)?;

        output[..sample_count * channel_count].fill(0.0);
        let total_blocks = sample_count / BLOCK_SAMPLES;
        let mut event = 0;
        for block in 0..total_blocks {
            while event < metadata.len() && aligned_block(metadata[event].offset) == block {
                self.apply_event(&metadata[event]);
                event += 1;
            }
            self.mix_block(input, output, block);
        }
        while event < metadata.len() && aligned_block(metadata[event].offset) == total_blocks {
            self.apply_event(&metadata[event]);
            event += 1;
        }
        debug_assert_eq!(event, metadata.len(), "metadata event was not applied");
        Ok(())
    }

    fn validate_metadata(
        &self,
        sample_count: usize,
        metadata: &[SpeakerMetadata<'_>],
    ) -> Result<(), RenderError> {
        let mut previous = 0u32;
        for (index, event) in metadata.iter().enumerate() {
            if event.offset as usize > sample_count {
                return Err(RenderError::OffsetOutOfRange {
                    offset: event.offset,
                    sample_count,
                });
            }
            if index != 0 && event.offset < previous {
                return Err(RenderError::OffsetsNotSorted);
            }
            previous = event.offset;
            if event.positions_q15.len() < self.object_count {
                return Err(RenderError::ObjectCount {
                    expected: self.object_count,
                    actual: event.positions_q15.len(),
                });
            }
            if let Some(regions) = event.region_indices
                && regions.len() < self.object_count
            {
                return Err(RenderError::ObjectCount {
                    expected: self.object_count,
                    actual: regions.len(),
                });
            }
            if let Some(enabled) = event.height_enabled
                && enabled.len() < self.object_count
            {
                return Err(RenderError::ObjectCount {
                    expected: self.object_count,
                    actual: enabled.len(),
                });
            }
            if let Some(gains) = event.object_gains
                && gains.len() < self.object_count
            {
                return Err(RenderError::ObjectCount {
                    expected: self.object_count,
                    actual: gains.len(),
                });
            }
            for object in 0..self.object_count {
                if let Some(regions) = event.region_indices
                    && regions[object] >= 7
                {
                    return Err(RenderError::RegionIndex(regions[object]));
                }
                if let Some(gains) = event.object_gains
                    && !gains[object].is_finite()
                {
                    return Err(RenderError::NonFiniteGain);
                }
                if event.positions_q15[object]
                    .iter()
                    .any(|coordinate| *coordinate > 32767)
                {
                    return Err(RenderError::CoordinateRange);
                }
            }
        }
        Ok(())
    }

    fn render_point(
        &self,
        position: &[u16; 3],
        region_index: u8,
        enable_height: bool,
        object_gain: f64,
    ) -> ChannelGains {
        let mut output = [0.0; MAX_CHANNELS];
        let region = &self.layout.regions[usize::from(region_index)];
        let u = f64::from(position[0]) / Q15_SCALE;
        let v = f64::from(position[1]) / Q15_SCALE;
        let w = f64::from(position[2]) / Q15_SCALE;
        let floor_v = clamp(ldexp(v, self.floor_y_exponent), 0.0, 1.0);
        let mut floor = [0.0; MAX_POINTS];
        plane_gains(
            region,
            &region.axis0_groups,
            region.axis0_group_count,
            u,
            floor_v,
            region.mode,
            &mut floor,
        );
        let mut point = floor;
        if region.mode == 3 {
            let mut height = [0.0; MAX_POINTS];
            plane_gains(
                region,
                &region.axis1_groups,
                region.axis1_group_count,
                u,
                v,
                3,
                &mut height,
            );
            let z = if enable_height {
                clamp(w, 0.0, Q15_MAX)
            } else {
                0.0
            };
            if z >= Q15_MAX {
                point = height;
            } else if z > 0.0 {
                let (floor_weight, height_weight) = equal_power_pair(z);
                for index in 0..usize::from(region.point_count) {
                    point[index] = floor[index] * floor_weight + height[index] * height_weight;
                }
            }
        }
        let y_term = clamp(v / 0.6, 0.0, 1.0);
        let z_term = clamp((w - 0.2) / 0.8, 0.0, 1.0);
        let amount = clamp(y_term + z_term, 0.0, 1.0);
        let gain = 10.0f64.powf(self.attenuation_db * amount / 20.0) * object_gain;
        for index in 0..usize::from(region.point_count) {
            output[usize::from(region.points[index].speaker_id)] = point[index] * gain;
        }
        output
    }

    fn apply_event(&mut self, event: &SpeakerMetadata<'_>) {
        let blocks = ramp_blocks(event.ramp_samples);
        for object in 0..self.object_count {
            let region = event.region_indices.map_or(0, |regions| regions[object]);
            let height = event
                .height_enabled
                .is_none_or(|enabled| enabled[object] != 0);
            let gain = event.object_gains.map_or(1.0, |gains| gains[object]);
            let next = self.render_point(&event.positions_q15[object], region, height, gain);
            for (channel, next_gain) in next.iter().take(self.layout.channel_count()).enumerate() {
                let difference = next_gain - self.current[object][channel];
                self.target[object][channel] = next[channel];
                if difference.abs() >= GAIN_SNAP_THRESHOLD && blocks != 0 {
                    self.step[object][channel] = difference / f64::from(blocks);
                    self.remaining[object][channel] = blocks;
                } else {
                    self.current[object][channel] = next[channel];
                    self.step[object][channel] = 0.0;
                    self.remaining[object][channel] = 0;
                }
            }
        }
    }

    fn mix_block(&mut self, input: &[f32], output: &mut [f64], block: usize) {
        let channel_count = self.layout.channel_count();
        let input_channels = self.input_channels();
        let start = block * BLOCK_SAMPLES;
        if self.layout.has_lfe() {
            for sample in 0..BLOCK_SAMPLES {
                output[(start + sample) * channel_count + 3] =
                    f64::from(input[(start + sample) * input_channels]);
            }
        }
        for object in 0..self.object_count {
            for internal in 0..channel_count {
                let active = self.remaining[object][internal] != 0;
                let fixed_gain = self.target[object][internal];
                if !active && fixed_gain == 0.0 {
                    continue;
                }
                let standard = usize::from(self.standard_index_for_internal[internal]);
                for sample in 0..BLOCK_SAMPLES {
                    let gain = if active {
                        self.current[object][internal]
                            + (sample as f64 / BLOCK_SAMPLES as f64) * self.step[object][internal]
                    } else {
                        fixed_gain
                    };
                    output[(start + sample) * channel_count + standard] +=
                        f64::from(input[(start + sample) * input_channels + object + 1]) * gain;
                }
                if active {
                    self.current[object][internal] += self.step[object][internal];
                    self.remaining[object][internal] -= 1;
                    if self.remaining[object][internal] == 0 {
                        self.current[object][internal] = self.target[object][internal];
                    }
                } else {
                    self.current[object][internal] = self.target[object][internal];
                }
            }
        }
    }
}

fn aligned_block(sample: u32) -> usize {
    (sample as usize + BLOCK_SAMPLES / 2 - 1) / BLOCK_SAMPLES
}

fn ramp_blocks(duration: u32) -> u32 {
    (duration as usize + BLOCK_SAMPLES / 2 - 1) as u32 / BLOCK_SAMPLES as u32
}

fn clamp(value: f64, low: f64, high: f64) -> f64 {
    value.clamp(low, high)
}

fn ldexp(value: f64, exponent: i32) -> f64 {
    value * 2.0f64.powi(exponent)
}

fn equal_power_pair(position: f64) -> (f64, f64) {
    let angle = (std::f64::consts::PI * 0.5) * position;
    (angle.cos(), angle.sin())
}

fn coordinate(region: &RegionGeometry, point: usize, component: usize) -> f64 {
    f64::from(region.points[point].coordinate_q15[component]) / Q15_SCALE
}

fn axis0_gains(
    region: &RegionGeometry,
    groups: &[AxisGroup; MAX_GROUPS],
    group_count: u8,
    value: f64,
    output: &mut PointGains,
) {
    *output = [0.0; MAX_POINTS];
    for group in groups.iter().take(usize::from(group_count)) {
        if group.size == 0 {
            continue;
        }
        let first = usize::from(group.indices[0]);
        let last = usize::from(group.indices[usize::from(group.size) - 1]);
        let first_value = coordinate(region, first, 0);
        let last_value = coordinate(region, last, 0);
        if value <= first_value {
            output[first] = 1.0;
            continue;
        }
        if value >= last_value {
            output[last] = 1.0;
            continue;
        }
        for index in 0..usize::from(group.size) - 1 {
            let lower_index = usize::from(group.indices[index]);
            let upper_index = usize::from(group.indices[index + 1]);
            let lower_value = coordinate(region, lower_index, 0);
            let upper_value = coordinate(region, upper_index, 0);
            if value > lower_value && value <= upper_value {
                let position = (value - lower_value) / (upper_value - lower_value);
                let (lower, upper) = equal_power_pair(position);
                output[lower_index] = lower;
                output[upper_index] = upper;
                break;
            }
        }
    }
}

fn axis1_gains(
    region: &RegionGeometry,
    groups: &[AxisGroup; MAX_GROUPS],
    group_count: u8,
    value: f64,
    output: &mut PointGains,
) {
    *output = [0.0; MAX_POINTS];
    if group_count == 0 {
        return;
    }
    let first_group = &groups[0];
    let last_group = &groups[usize::from(group_count) - 1];
    let first_value = coordinate(region, usize::from(first_group.indices[0]), 1);
    let last_value = coordinate(region, usize::from(last_group.indices[0]), 1);
    if value <= first_value {
        for index in 0..usize::from(first_group.size) {
            output[usize::from(first_group.indices[index])] = 1.0;
        }
        return;
    }
    if value > last_value {
        for index in 0..usize::from(last_group.size) {
            output[usize::from(last_group.indices[index])] = 1.0;
        }
        return;
    }
    for (lower_group, upper_group) in groups[..usize::from(group_count)]
        .iter()
        .zip(groups[1..usize::from(group_count)].iter())
    {
        let lower_value = coordinate(region, usize::from(lower_group.indices[0]), 1);
        let upper_value = coordinate(region, usize::from(upper_group.indices[0]), 1);
        if value >= lower_value && value <= upper_value {
            let position = (value - lower_value) / (upper_value - lower_value);
            let (lower_gain, upper_gain) = equal_power_pair(position);
            for index in 0..usize::from(lower_group.size) {
                output[usize::from(lower_group.indices[index])] = lower_gain;
            }
            for index in 0..usize::from(upper_group.size) {
                output[usize::from(upper_group.indices[index])] = upper_gain;
            }
            return;
        }
    }
}

fn plane_gains(
    region: &RegionGeometry,
    groups: &[AxisGroup; MAX_GROUPS],
    group_count: u8,
    u: f64,
    v: f64,
    mode: u8,
    output: &mut PointGains,
) {
    axis0_gains(region, groups, group_count, u, output);
    if mode >= 2 {
        let mut vertical = [0.0; MAX_POINTS];
        axis1_gains(region, groups, group_count, v, &mut vertical);
        for point in 0..usize::from(region.point_count) {
            output[point] *= vertical[point];
        }
    }
}

fn expand_speaker_bitfield(compact: u32) -> u64 {
    const EXPANSIONS: [u64; 22] = [
        0x0000_0003,
        0x0000_0004,
        0x0000_0008,
        0x0000_0030,
        0x0000_00C0,
        0x0000_0100,
        0x0000_0600,
        0x0000_1800,
        0x0000_6000,
        0x0001_8000,
        0x0006_0000,
        0x0018_0000,
        0x0060_0000,
        0x0180_0000,
        0x0600_0000,
        0x1800_0000,
        0x6000_0000,
        0x0_8000_0000,
        0x6_0000_0000,
        0x8_0000_0000,
        0x10_0000_0000,
        0x20_0000_0000,
    ];
    let mut expanded = 0u64;
    for (index, expansion) in EXPANSIONS.iter().enumerate() {
        if compact & (1u32 << index) != 0 {
            expanded |= expansion;
        }
    }
    expanded
}

fn bit(value: u64, index: u32) -> u32 {
    ((value >> index) & 1) as u32
}

fn layout_attenuation_db(compact: u32) -> f64 {
    let expanded = expand_speaker_bitfield(compact);
    let height_channels = 2
        * (bit(expanded, 13)
            + bit(expanded, 15)
            + bit(expanded, 17)
            + bit(expanded, 19)
            + bit(expanded, 21));
    let floor_channels = bit(expanded, 8)
        + 2 * (bit(expanded, 31)
            + bit(expanded, 4)
            + bit(expanded, 6)
            + bit(expanded, 11)
            + bit(expanded, 25)
            + bit(expanded, 27)
            + bit(expanded, 29)
            + bit(expanded, 33));
    let height_factor = (f64::from(height_channels) / 4.0).min(1.0);
    let floor_factor = (f64::from(floor_channels) / 4.0).min(1.0);
    -(4.5 - 1.5 * height_factor - 3.0 * floor_factor).max(0.0)
}

fn floor_y_exponent(compact: u32) -> i32 {
    let low = expand_speaker_bitfield(compact) as u32;
    if low & 0x130 != 0 && low & 0x18C0 == 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout_by_name;

    const LEGACY_OBJECTS: usize = 15;

    fn frame(object_count: usize) -> (Vec<f32>, Vec<f64>) {
        let input_channels = object_count + 1;
        let mut input = vec![0.0f32; BLOCK_SAMPLES * input_channels];
        for sample in 0..BLOCK_SAMPLES {
            input[sample * input_channels + 1] = 1.0;
        }
        let layout = layout_by_name("7.1.4").unwrap();
        let output = vec![0.0; BLOCK_SAMPLES * layout.channel_count()];
        (input, output)
    }

    fn left_position() -> [u16; 3] {
        [0, 0, 0]
    }

    fn positions(object_count: usize, object: usize, position: [u16; 3]) -> Vec<[u16; 3]> {
        let mut positions = vec![[16383, 0, 0]; object_count];
        positions[object] = position;
        positions
    }

    #[test]
    fn an_immediate_left_event_lands_on_the_left_channel() {
        let layout = layout_by_name("7.1.4").unwrap();
        let mut renderer = SpeakerRenderer::new(layout, LEGACY_OBJECTS);
        let (input, mut output) = frame(LEGACY_OBJECTS);
        let positions = positions(LEGACY_OBJECTS, 0, left_position());
        renderer
            .process(
                &input,
                BLOCK_SAMPLES,
                &[SpeakerMetadata::new(0, 0, &positions)],
                &mut output,
            )
            .unwrap();
        let channels = layout.channel_count();
        for sample in 0..BLOCK_SAMPLES {
            assert!((output[sample * channels] - 1.0).abs() < 1e-12);
            assert_eq!(output[sample * channels + 1], 0.0);
        }
    }

    #[test]
    fn a_ramp_reaches_the_target_after_the_declared_samples() {
        let layout = layout_by_name("7.1.4").unwrap();
        let mut renderer = SpeakerRenderer::new(layout, LEGACY_OBJECTS);
        let frames = 2usize;
        let input_channels = LEGACY_OBJECTS + 1;
        let mut input = vec![0.0f32; frames * BLOCK_SAMPLES * input_channels];
        for sample in 0..frames * BLOCK_SAMPLES {
            input[sample * input_channels + 1] = 1.0;
        }
        let mut output = vec![0.0; frames * BLOCK_SAMPLES * layout.channel_count()];
        let positions = positions(LEGACY_OBJECTS, 0, left_position());
        let event = SpeakerMetadata::new(0, 64, &positions);
        renderer
            .process(&input, frames * BLOCK_SAMPLES, &[event], &mut output)
            .unwrap();
        let channels = layout.channel_count();
        let last = (frames * BLOCK_SAMPLES - 1) * channels;
        assert!(output[last] > 0.9);
        // The next call without an event keeps the completed target gain.
        let mut tail = vec![0.0; BLOCK_SAMPLES * channels];
        renderer
            .process(
                &input[..BLOCK_SAMPLES * input_channels],
                BLOCK_SAMPLES,
                &[],
                &mut tail,
            )
            .unwrap();
        assert!((tail[(BLOCK_SAMPLES - 1) * channels] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn lfe_is_copied_to_the_lfe_channel() {
        let layout = layout_by_name("5.1").unwrap();
        let object_count = 0;
        let mut renderer = SpeakerRenderer::new(layout, object_count);
        let mut input = vec![0.0f32; BLOCK_SAMPLES * (object_count + 1)];
        for sample in 0..BLOCK_SAMPLES {
            input[sample * (object_count + 1)] = 0.25;
        }
        let mut output = vec![0.0; BLOCK_SAMPLES * layout.channel_count()];
        renderer
            .process(&input, BLOCK_SAMPLES, &[], &mut output)
            .unwrap();
        assert_eq!(output[3], 0.25);
    }

    #[test]
    fn rejects_misaligned_blocks() {
        let layout = layout_by_name("5.1").unwrap();
        let mut renderer = SpeakerRenderer::new(layout, LEGACY_OBJECTS);
        let mut output = vec![0.0; 10 * layout.channel_count()];
        assert_eq!(
            renderer.process(&[], 10, &[], &mut output),
            Err(RenderError::BlockAlignment)
        );
    }

    #[test]
    fn renders_every_dynamic_object_count() {
        let layout = layout_by_name("7.1.4").unwrap();
        for object_count in [0usize, 1, 12, 14, 15, 128, 256] {
            let mut renderer = SpeakerRenderer::new(layout, object_count);
            let input_channels = object_count + 1;
            let mut input = vec![0.0f32; BLOCK_SAMPLES * input_channels];
            for object in 0..object_count {
                input[object + 1] = 0.001 * (object as f32 + 1.0);
            }
            let mut output = vec![0.0; BLOCK_SAMPLES * layout.channel_count()];
            let positions = if object_count == 0 {
                Vec::new()
            } else {
                positions(object_count, object_count / 2, [16383, 0, 0])
            };
            if object_count == 0 {
                renderer
                    .process(&input, BLOCK_SAMPLES, &[], &mut output)
                    .unwrap();
            } else {
                let metadata = [SpeakerMetadata::new(0, 0, &positions)];
                renderer
                    .process(&input, BLOCK_SAMPLES, &metadata, &mut output)
                    .unwrap();
            }
            assert!(output.iter().all(|value| value.is_finite()));
        }
    }

    #[test]
    fn rejects_short_metadata_slices() {
        let layout = layout_by_name("7.1.4").unwrap();
        let mut renderer = SpeakerRenderer::new(layout, 4);
        let input = vec![0.0f32; BLOCK_SAMPLES * 5];
        let mut output = vec![0.0; BLOCK_SAMPLES * layout.channel_count()];
        let positions = vec![[0u16; 3]; 3];
        let event = SpeakerMetadata::new(0, 0, &positions);
        assert_eq!(
            renderer.process(&input, BLOCK_SAMPLES, &[event], &mut output),
            Err(RenderError::ObjectCount {
                expected: 4,
                actual: 3,
            })
        );
    }
}
