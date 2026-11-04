use crate::payload::{ObjectAudioMetadataPayload, Trim, TrimElement};
use objectaudio_ir::{
    ConversionReport, ObjectEvent, ObjectState, ProgramCalibration, SpatialPayload, TrimRecord,
};

use crate::bed::BedLayout;
use objectaudio_ir::SpeakerPosition;

use crate::OamdError;

/// Bounds applied before the payload is expanded into IR objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OamdLimits {
    pub max_objects: usize,
    pub max_blocks: usize,
}

impl Default for OamdLimits {
    fn default() -> Self {
        Self {
            max_objects: crate::payload::MAX_OBJECT_COUNT,
            max_blocks: crate::payload::MAX_OBJ_INFO_BLOCKS,
        }
    }
}

/// The program declaration carried by one OAMD payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OamdProgram {
    pub dynamic_object_only: bool,
    pub content_bed: bool,
    pub content_isf: bool,
    pub content_dynamic: bool,
    pub num_bed_objects: usize,
    pub num_isf_objects: usize,
    pub num_dynamic_objects: usize,
}

/// One object slot from an OAMD object element.
#[derive(Debug, Clone, PartialEq)]
pub struct OamdObject {
    /// Position in the OAMD object list. Bed objects occupy the first slots.
    pub slot: usize,
    /// This slot is part of the bed or an ISF, not a dynamic object.
    pub in_bed_or_isf: bool,
    pub active: bool,
    /// Object gain in dB, or `None` for the OAMD minus-infinity code.
    pub gain_db: Option<f32>,
    /// Linear gain derived from `gain_db`.
    pub gain: f32,
    pub priority: f32,
    /// ADM normalized Cartesian position.
    pub position: [f32; 3],
    /// Object size in normalized units, when the payload specifies one.
    pub size: Option<[f32; 3]>,
    /// Distance in metres, or infinity for an object at infinity.
    pub distance: Option<f32>,
    pub snap: bool,
}

impl OamdObject {
    pub fn to_state(&self) -> ObjectState {
        let Self {
            slot,
            in_bed_or_isf: _,
            active,
            gain_db: _,
            gain,
            priority: _,
            position,
            size,
            distance: _,
            snap: _,
        } = *self;
        ObjectState {
            id: slot as u32,
            position,
            gain,
            enabled: active,
            size: size
                .map(|size| size.into_iter().fold(0.0, f32::max))
                .filter(|size| *size > 0.0),
            spread: None,
        }
    }
    fn report_conversion(&self, report: &mut objectaudio_ir::ConversionReport) {
        if self.priority != 1.0 {
            report.record("priority", "not represented by the spatial IR");
        }
        if self.distance.is_some() {
            report.record("distance", "not represented by the spatial IR");
        }
        if self.snap {
            report.record("snap", "not represented by the spatial IR");
        }
        if self
            .size
            .is_some_and(|size| size[0] != size[1] || size[1] != size[2])
        {
            report.record("size", "three dimensions reduced to their maximum");
        }
    }
}

/// A parsed OAMD payload in salsa's codec-independent terms.
#[derive(Debug, Clone, PartialEq)]
pub struct OamdFrame {
    pub report: objectaudio_ir::ConversionReport,
    pub version: u8,
    /// Number of object slots in the payload, including bed objects.
    pub object_count: usize,
    pub program: OamdProgram,
    pub bed: BedLayout,
    /// All slots in the payload, including bed/ISF objects.
    pub objects: Vec<OamdObject>,
    /// State changes after the first object-info block, ordered by offset.
    pub events: Vec<ObjectEvent>,
    /// Offset of the first position block, in samples.
    pub sample_offset: u32,
    /// Ramp duration of the first position block, in samples.
    pub ramp_samples: u32,
    /// Programme trim element, when this payload carried one.
    pub trim: Option<TrimElement>,
}

/// How the OAMD bed is treated when mapping to the object-only IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedPolicy {
    /// Do not append bed channels.
    Ignore,
    /// Append the non-LFE bed channels declared by this payload.
    Expand,
}

/// A payload mapped onto the IR, including the offset accumulation rule.
#[derive(Debug, Clone, PartialEq)]
pub struct OamdMapping {
    pub report: objectaudio_ir::ConversionReport,
    /// Dynamic objects rebased to `1..=N`, followed by any appended bed objects.
    pub objects: Vec<ObjectState>,
    /// Events with rebased ids and `base_offset` already added.
    pub events: Vec<ObjectEvent>,
    /// Number of dynamic objects before bed expansion.
    pub dynamic_count: usize,
    /// `base_offset + OAMD sample_offset`.
    pub snapshot_offset: u32,
    /// Ramp duration of the first object-info block, in samples.
    pub snapshot_ramp_samples: u32,
    /// Programme calibration derived from the trim element, with bypass flags
    /// already rebased to IR object indices.
    pub calibration: Option<ProgramCalibration>,
}

impl OamdFrame {
    pub fn from_payload(
        payload: &ObjectAudioMetadataPayload,
        limits: OamdLimits,
    ) -> Result<Self, OamdError> {
        if payload.object_count > limits.max_objects {
            return Err(OamdError::TooManyObjects {
                count: payload.object_count,
                limit: limits.max_objects,
            });
        }

        let object_element = payload.object_element.as_ref();
        let object_element_count = payload
            .oa_element_md
            .iter()
            .filter(|element| element.oa_element_id_idx == 1)
            .count();
        if object_element_count > 1 {
            return Err(OamdError::MultipleObjectElements {
                count: object_element_count,
            });
        }

        let program = &payload.program_assignment;
        if object_element.is_none() && payload.object_count > program.beds_or_isf_count() {
            return Err(OamdError::MissingObjectElement {
                object_count: payload.object_count,
            });
        }

        let md_update = object_element.map(|element| &element.md_update_info);
        if let Some(update) = md_update
            && update.num_obj_info_blocks > limits.max_blocks
        {
            return Err(OamdError::TooManyBlocks {
                count: update.num_obj_info_blocks,
                limit: limits.max_blocks,
            });
        }

        let content_bed = !program.bed_assignment.is_empty();
        let content_isf = program.num_isf_objects > 0;
        let content_dynamic = program.num_dynamic_objects > 0;
        let bed_only = content_bed && !content_isf && !content_dynamic;
        let lfe_only_bed = !program.bed_assignment.is_empty()
            && program
                .bed_assignment
                .iter()
                .all(|assignment| assignment.is_lfe_only());
        let dynamic_object_only = program.num_isf_objects == 0
            && program.num_dynamic_objects > 0
            && (program.bed_assignment.is_empty() || lfe_only_bed);

        let mut bed_mask = 0u32;
        for assignment in &program.bed_assignment {
            for (index, present) in assignment.0.iter().enumerate() {
                if *present && let Some(channel) = SpeakerPosition::from_bed_index(index) {
                    bed_mask |= channel.bed_bit();
                }
            }
        }
        let bed = BedLayout::from_mask(bed_mask, bed_only);
        if bed.contains(SpeakerPosition::Lfe2) {
            return Err(OamdError::UnsupportedLfe2);
        }

        let positions = payload.get_damf_pos();
        let object_data = object_element.map_or(&[][..], |element| &element.object_data[..]);
        let mut report = ConversionReport::default();
        let mut objects = Vec::with_capacity(payload.object_count);
        for slot in 0..payload.object_count {
            let blocks = object_data.get(slot).map_or(&[][..], Vec::as_slice);
            let first = blocks.first();
            let in_bed_or_isf = first
                .map(|block| block.b_object_in_bed_or_isf)
                .unwrap_or(slot < program.beds_or_isf_count());
            let position = positions
                .get(slot)
                .and_then(|positions| positions.first())
                .map_or([0.0, 0.0, 0.0], |position| adm_to_f32(*position));
            let object = match first {
                Some(block) => object_at(slot, block, position, &mut report),
                None => OamdObject {
                    slot,
                    in_bed_or_isf,
                    active: false,
                    gain_db: None,
                    gain: 0.0,
                    priority: 0.0,
                    position,
                    size: None,
                    distance: None,
                    snap: false,
                },
            };
            objects.push(object);
        }

        for object in &objects {
            object.report_conversion(&mut report);
        }
        let mut events = Vec::new();
        if let (Some(element), Some(update)) = (object_element, md_update) {
            for (slot, blocks) in element.object_data.iter().enumerate() {
                if objects
                    .get(slot)
                    .map(|object| object.in_bed_or_isf)
                    .unwrap_or(true)
                {
                    continue;
                }
                for (block_index, block) in blocks.iter().enumerate().skip(1) {
                    let block_update = update.block_update_info.get(block_index);
                    let offset = update.sample_offset as u32
                        + block_update
                            .map_or(0, |info| u32::from(info.block_offset_factor_bits) * 32);
                    let ramp = block_update.map_or(0, |info| u32::from(info.ramp_duration));
                    let position = positions
                        .get(slot)
                        .and_then(|positions| positions.get(block_index))
                        .map_or_else(
                            || adm_position(block.object_render_info.pos3d),
                            |position| adm_to_f32(*position),
                        );
                    let object = object_at(slot, block, position, &mut report);
                    object.report_conversion(&mut report);
                    let state = object.to_state();
                    // A repeated target can restart a transition with a new
                    // duration. Do not infer time semantics from state equality.
                    events.push(ObjectEvent {
                        offset: i64::from(offset),
                        transition: objectaudio_ir::Transition::All(ramp),
                        state,
                    });
                }
            }
            events.sort_by_key(|event| event.offset);
        }

        let sample_offset = md_update.map_or(0, |update| {
            update.sample_offset as u32
                + update
                    .block_update_info
                    .first()
                    .map_or(0, |info| u32::from(info.block_offset_factor_bits) * 32)
        });
        let ramp_samples = md_update
            .and_then(|update| update.block_update_info.first())
            .map_or(0, |info| u32::from(info.ramp_duration));

        Ok(Self {
            report,
            version: payload.oamd_version,
            object_count: payload.object_count,
            program: OamdProgram {
                dynamic_object_only,
                content_bed,
                content_isf,
                content_dynamic,
                num_bed_objects: program.num_bed_objects,
                num_isf_objects: program.num_isf_objects,
                num_dynamic_objects: program.num_dynamic_objects,
            },
            bed,
            objects,
            events,
            sample_offset,
            ramp_samples,
            trim: payload.trim_element.clone(),
        })
    }

    /// States for the dynamic objects, excluding bed/ISF slots.
    pub fn object_states(&self) -> Vec<ObjectState> {
        self.dynamic_objects().map(OamdObject::to_state).collect()
    }

    /// Convert the payload into the object-only IR.
    ///
    /// Dynamic objects keep their OAMD order and are rebased to ids `1..=N`.
    /// `BedPolicy::Expand` appends the non-LFE declared bed channels as
    /// fixed-position objects; `Ignore` leaves the bed to the caller. A trim
    /// element becomes the payload's programme calibration.
    pub fn to_spatial_payload(&self, sample_rate: u32, bed: BedPolicy) -> SpatialPayload {
        let mapping = self.map_to_ir(0, bed);
        let payload = SpatialPayload::new(sample_rate, mapping.objects, mapping.events)
            .with_snapshot_ramp(mapping.snapshot_ramp_samples)
            .with_snapshot_offset(i64::from(mapping.snapshot_offset));
        match mapping.calibration {
            Some(calibration) => payload.with_calibration(calibration),
            None => payload,
        }
    }

    /// Map the payload onto the IR with an explicit base sample offset.
    ///
    /// `base_offset` is the container/EVO offset of this OAMD payload. It is
    /// added to both the event offsets and the snapshot offset, so callers must
    /// not add it again.
    pub fn map_to_ir(&self, base_offset: u32, bed: BedPolicy) -> OamdMapping {
        match bed {
            BedPolicy::Ignore => self.map_to_ir_with_bed(base_offset, &[]),
            BedPolicy::Expand => self.map_to_ir_with_bed(base_offset, &self.bed.channels),
        }
    }

    /// [`Self::map_to_ir`] with an explicit bed-channel list.
    ///
    /// TrueHD uses this to append the presentation bed channels, which are not
    /// necessarily the bed declared inside OAMD.
    pub fn map_to_ir_with_bed(&self, base_offset: u32, bed: &[SpeakerPosition]) -> OamdMapping {
        let dynamic: Vec<&OamdObject> = self.dynamic_objects().collect();
        let mut objects = Vec::with_capacity(dynamic.len() + bed.len());
        let mut source_slots = Vec::with_capacity(dynamic.len() + bed.len());
        let mut slot_to_id = std::collections::HashMap::new();
        for (index, object) in dynamic.iter().enumerate() {
            let id = index as u32 + 1;
            slot_to_id.insert(object.slot as u32, id);
            let mut state = object.to_state();
            state.id = id;
            objects.push(state);
            source_slots.push(Some(object.slot));
        }
        for channel in bed {
            if channel.is_lfe() {
                continue;
            }
            let id = objects.len() as u32 + 1;
            objects.push(ObjectState::new(id, channel.adm_position()));
            // The declared slot order includes LFE. The presentation bed can
            // omit or reorder channels, so match speakers before copying trim.
            source_slots.push(
                crate::bed::BED_CHANNELS
                    .iter()
                    .filter(|speaker| self.bed.contains(**speaker))
                    .position(|speaker| speaker == channel),
            );
        }

        let mut events = Vec::new();
        for event in &self.events {
            let Some(id) = slot_to_id.get(&event.state.id) else {
                continue;
            };
            let mut state = event.state;
            state.id = *id;
            events.push(ObjectEvent {
                offset: event.offset.saturating_add(i64::from(base_offset)),
                transition: event.transition,
                state,
            });
        }
        events.sort_by_key(|event| event.offset);
        let calibration = self.trim.as_ref().map(|trim| {
            let bypass = source_slots
                .iter()
                .map(|slot| {
                    slot.and_then(|slot| trim.b_disable_trim.get(slot))
                        .copied()
                        .unwrap_or(false)
                })
                .collect();
            ProgramCalibration {
                warp_mode: trim.warp_mode,
                global_trim_mode: trim.global_trim_mode,
                configs: std::array::from_fn(|index| trim_record(trim.trims[index].as_ref())),
                bypass,
            }
        });
        OamdMapping {
            report: self.report.clone(),
            objects,
            events,
            dynamic_count: dynamic.len(),
            snapshot_offset: self.sample_offset.saturating_add(base_offset),
            snapshot_ramp_samples: self.ramp_samples,
            calibration,
        }
    }

    pub fn dynamic_objects(&self) -> impl Iterator<Item = &OamdObject> + '_ {
        self.objects.iter().filter(|object| !object.in_bed_or_isf)
    }
}

fn trim_record(trim: Option<&Trim>) -> TrimRecord {
    match trim {
        None => TrimRecord::default(),
        Some(trim) if trim.b_default_trim => TrimRecord::default_trim(),
        Some(trim) if trim.b_disable_trim => TrimRecord::disabled(),
        Some(trim) => TrimRecord::explicit(
            trim.trim_centre,
            trim.trim_surround,
            trim.trim_height,
            trim.bal3d_y_lis,
            trim.bal3d_y_tb,
        ),
    }
}

fn object_at(
    slot: usize,
    block: &crate::payload::ObjectInfoBlock,
    position: [f32; 3],
    report: &mut ConversionReport,
) -> OamdObject {
    // Resolve wire fields in one place. New fields require an explicit mapping
    // or loss report here, before consumers receive the smaller spatial IR.
    let crate::payload::ObjectInfoBlock {
        b_object_not_active,
        object_basic_info:
            crate::payload::ObjectBasicInfo {
                object_gain,
                object_priority,
            },
        b_object_in_bed_or_isf,
        object_render_info:
            crate::payload::ObjectRenderInfo {
                b_differential_position_specified: _, // resolved by the parser
                pos3d: _,                             // converted to ADM in `position`
                b_object_distance_specified: _,       // represented by distance_factor
                distance_factor,
                zone_constraints_idx,
                b_enable_elevation,
                object_size: size,
                b_object_use_screen_ref,
                screen_factor,
                depth_factor,
                b_object_snap,
            },
    } = block;
    for (property, used) in [
        ("zones", *zone_constraints_idx != 0),
        ("elevation", !b_enable_elevation),
        ("screen reference", *b_object_use_screen_ref),
        ("screen factor", *screen_factor != 0.0),
        ("depth factor", *depth_factor != 0.25),
    ] {
        if used {
            report.record(property, "not represented by the spatial IR");
        }
    }
    let (gain_db, gain) = gain_from_code(*object_gain);
    OamdObject {
        slot,
        in_bed_or_isf: *b_object_in_bed_or_isf,
        active: !b_object_not_active,
        gain_db,
        gain,
        priority: *object_priority as f32,
        position,
        size: object_size(*size),
        distance: distance_factor.map(|value| value as f32),
        snap: *b_object_snap,
    }
}

fn gain_from_code(code: i8) -> (Option<f32>, f32) {
    if code == crate::payload::GAIN_MINUS_INFINITY {
        (None, 0.0)
    } else {
        let db = f32::from(code);
        (Some(db), 10.0f32.powf(db / 20.0))
    }
}

fn object_size(size: [f64; 3]) -> Option<[f32; 3]> {
    let size = size.map(|value| if value.is_finite() { value as f32 } else { 0.0 });
    size.iter().any(|value| *value > 0.0).then_some(size)
}

fn adm_to_f32(position: [f64; 3]) -> [f32; 3] {
    [position[0] as f32, position[1] as f32, position[2] as f32]
}

fn adm_position(position: [f64; 3]) -> [f32; 3] {
    [
        ((position[0].clamp(0.0, 1.0) - 0.5) * 2.0) as f32,
        ((0.5 - position[1].clamp(0.0, 1.0)) * 2.0) as f32,
        position[2].clamp(-1.0, 1.0) as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bed_payload(channels: &[SpeakerPosition]) -> ObjectAudioMetadataPayload {
        use crate::payload::{BedAssignment, ProgramAssignment};
        let mask = channels
            .iter()
            .fold(0, |mask, channel| mask | channel.bed_bit());
        ObjectAudioMetadataPayload {
            object_count: channels.len(),
            program_assignment: ProgramAssignment {
                num_bed_objects: channels.len(),
                bed_assignment: vec![BedAssignment::from_non_std(mask)],
                ..ProgramAssignment::default()
            },
            ..ObjectAudioMetadataPayload::default()
        }
    }

    #[test]
    fn expanded_beds_use_shared_source_positions_in_wire_order() {
        let channels = &objectaudio_ir::BED_CHANNELS[..16];
        let payload = bed_payload(channels);
        let frame = OamdFrame::from_payload(&payload, OamdLimits::default()).unwrap();
        let mapping = frame.map_to_ir(0, BedPolicy::Expand);
        let expected: Vec<_> = channels
            .iter()
            .filter(|channel| !channel.is_lfe())
            .collect();
        assert_eq!(mapping.objects.len(), expected.len());
        for (index, (object, channel)) in mapping.objects.iter().zip(expected).enumerate() {
            assert_eq!(object.id, index as u32 + 1);
            assert_eq!(object.position, channel.adm_position());
        }
    }

    #[test]
    fn second_lfe_is_rejected_before_bed_expansion() {
        for channels in [
            vec![SpeakerPosition::Lfe2],
            vec![SpeakerPosition::Lfe, SpeakerPosition::Lfe2],
        ] {
            assert!(matches!(
                OamdFrame::from_payload(&bed_payload(&channels), OamdLimits::default()),
                Err(OamdError::UnsupportedLfe2)
            ));
        }
    }

    #[test]
    fn expanded_bed_bypass_uses_declared_slots_including_lfe() {
        use crate::payload::{BedAssignment, ProgramAssignment};
        use SpeakerPosition::{Left, LeftSurround, Lfe, RightSurround};
        for speakers in [
            vec![Lfe, LeftSurround],
            vec![Left, Lfe, LeftSurround, RightSurround],
        ] {
            let mask = speakers
                .iter()
                .fold(0, |mask, speaker| mask | speaker.bed_bit());
            let declared: Vec<_> = crate::bed::BED_CHANNELS
                .iter()
                .filter(|speaker| mask & speaker.bed_bit() != 0)
                .copied()
                .collect();
            let payload = ObjectAudioMetadataPayload {
                object_count: declared.len(),
                program_assignment: ProgramAssignment {
                    num_bed_objects: declared.len(),
                    bed_assignment: vec![BedAssignment::from_non_std(mask)],
                    ..ProgramAssignment::default()
                },
                trim_element: Some(TrimElement {
                    b_disable_trim_per_obj: true,
                    b_disable_trim: declared
                        .iter()
                        .map(|speaker| *speaker == LeftSurround)
                        .collect(),
                    ..TrimElement::default()
                }),
                ..ObjectAudioMetadataPayload::default()
            };
            let frame = OamdFrame::from_payload(&payload, OamdLimits::default()).unwrap();
            let expanded = frame.to_spatial_payload(48_000, BedPolicy::Expand);
            let expected: Vec<_> = declared
                .iter()
                .filter(|speaker| !speaker.is_lfe())
                .map(|speaker| *speaker == LeftSurround)
                .collect();
            assert_eq!(expanded.calibration.unwrap().bypass, expected);
            // Presentation channels can be reordered or absent from the OAMD bed.
            let mapping = frame.map_to_ir_with_bed(
                0,
                &[
                    RightSurround,
                    Lfe,
                    LeftSurround,
                    SpeakerPosition::TopSideLeft,
                ],
            );
            assert_eq!(mapping.calibration.unwrap().bypass, [false, true, false]);
            assert!(
                frame
                    .map_to_ir(0, BedPolicy::Ignore)
                    .calibration
                    .unwrap()
                    .bypass
                    .is_empty()
            );
        }
    }

    #[test]
    fn mapping_applies_the_base_offset_once() {
        let mut frame = crate::parse(crate::EXAMPLE_DATA).unwrap();
        frame.sample_offset = 7;
        frame.events.push(ObjectEvent {
            offset: 100,
            transition: objectaudio_ir::Transition::All(0),
            state: ObjectState::new(1, [0.0, 1.0, 0.0]),
        });
        let mapping = frame.map_to_ir(1_000, BedPolicy::Ignore);
        assert_eq!(mapping.snapshot_offset, 1_007);
        assert_eq!(mapping.snapshot_ramp_samples, frame.ramp_samples);
        assert_eq!(mapping.events.last().unwrap().offset, 1_100);
    }

    #[test]
    fn gain_code_zero_is_unity() {
        assert_eq!(gain_from_code(0), (Some(0.0), 1.0));
    }

    #[test]
    fn minus_infinity_is_muted() {
        assert_eq!(
            gain_from_code(crate::payload::GAIN_MINUS_INFINITY),
            (None, 0.0)
        );
    }

    #[test]
    fn adm_position_flips_the_y_axis() {
        assert_eq!(adm_position([0.0, 0.0, 1.0]), [-1.0, 1.0, 1.0]);
        assert_eq!(adm_position([1.0, 1.0, -1.0]), [1.0, -1.0, -1.0]);
    }
}
