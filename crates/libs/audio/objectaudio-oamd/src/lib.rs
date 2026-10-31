#![forbid(unsafe_code)]
#![doc = "OAMD parser/writer shared by the salsa codec decoders and the Dolby adapter."]

mod bed;
mod bitstream;
mod encode;
mod error;
mod frame;
mod payload;

pub use bed::{BED_CHANNEL_COUNT, BED_CHANNELS, BedChannel, BedLayout};
pub use encode::encode;
pub use error::OamdError;
pub use frame::{
    BedPolicy, OamdFrame, OamdLimits, OamdMapping, OamdObject, OamdProgram, oamd_to_ir_events,
};
pub use objectaudio_ir::SpeakerPosition;

/// EMDF payload id carried by an OAMD update.
pub const OAMD_PAYLOAD_ID: u8 = 11;

/// The wire-level payload parser, merged from `truehdd/oamd` 0.1.0.
pub use payload::*;

/// Parse one OAMD payload with the default limits.
pub fn parse(bytes: &[u8]) -> Result<OamdFrame, OamdError> {
    parse_with_limits(bytes, OamdLimits::default())
}

/// Parse one OAMD payload with explicit limits.
pub fn parse_with_limits(bytes: &[u8], limits: OamdLimits) -> Result<OamdFrame, OamdError> {
    let payload = ObjectAudioMetadataPayload::read(bytes).map_err(OamdError::Parse)?;
    OamdFrame::from_payload(&payload, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objectaudio_ir::SpatialPayload;
    use proptest::prelude::*;

    #[test]
    fn parses_the_upstream_example() {
        let frame = parse(EXAMPLE_DATA).unwrap();
        assert_eq!(frame.version, 0);
        assert_eq!(frame.object_count, 16);
        assert_eq!(frame.objects.len(), 16);
        assert_eq!(frame.object_states().len(), 15);

        assert!(frame.bed.present);
        assert_eq!(frame.bed.mask, BedChannel::Lfe.bed_bit());
        assert_eq!(frame.bed.name, None);
        assert!(!frame.bed.bed_only);

        assert_eq!(frame.program.num_bed_objects, 1);
        assert_eq!(frame.program.num_dynamic_objects, 15);
        assert!(frame.program.dynamic_object_only);
        assert!(frame.program.content_bed);
        assert!(!frame.program.content_isf);
        assert!(frame.program.content_dynamic);

        let first = &frame.object_states()[0];
        assert_eq!(first.id, 1);
        assert_eq!(first.position, [-1.0, 1.0, 0.0]);
        assert_eq!(first.gain, 1.0);
        assert!(first.enabled);
        assert!(frame.events.is_empty());
    }

    #[test]
    fn rejects_non_zero_versions() {
        let mut bytes = EXAMPLE_DATA.to_vec();
        bytes[0] = (bytes[0] & 0x3f) | 0x40;
        assert!(matches!(parse(&bytes), Err(OamdError::Parse(_))));
    }

    #[test]
    fn rejects_empty_and_truncated_payloads() {
        assert!(parse(&[]).is_err());
        assert!(parse(&EXAMPLE_DATA[..1]).is_err());

        for end in 0..=EXAMPLE_DATA.len() {
            let _ = parse(&EXAMPLE_DATA[..end]);
        }
    }

    #[test]
    fn enforces_the_object_limit() {
        let limits = OamdLimits {
            max_objects: 8,
            ..OamdLimits::default()
        };
        assert!(matches!(
            parse_with_limits(EXAMPLE_DATA, limits),
            Err(OamdError::TooManyObjects {
                count: 16,
                limit: 8
            })
        ));
    }

    #[test]
    fn bed_layout_is_derived_from_assignments() {
        let frame = parse(EXAMPLE_DATA).unwrap();
        let lfe = frame.bed.iter().collect::<Vec<_>>();
        assert!(lfe.is_empty());
        assert!(frame.objects[0].in_bed_or_isf);
        assert!(!frame.objects[1].in_bed_or_isf);
    }

    #[test]
    fn encodes_and_reparses_a_spatial_payload() {
        let source = parse(EXAMPLE_DATA)
            .unwrap()
            .to_spatial_payload(48_000, BedPolicy::Expand);
        let bytes = encode(&source).unwrap();
        let reparsed = parse(&bytes).unwrap();
        assert_eq!(reparsed.object_count, 16);
        assert_eq!(reparsed.dynamic_objects().count(), 15);
        let round = reparsed.to_spatial_payload(48_000, BedPolicy::Expand);
        assert_eq!(round.version, SpatialPayload::VERSION);
        assert_eq!(round.sample_rate, 48_000);
        assert_eq!(round.objects.len(), source.objects.len());
        for (a, b) in source.objects.iter().zip(&round.objects) {
            assert_eq!(a.id, b.id);
            for axis in 0..3 {
                assert!(
                    (a.position[axis] - b.position[axis]).abs() <= 2.0 / 62.0 + 1e-6,
                    "position {axis} drifted: {} vs {}",
                    a.position[axis],
                    b.position[axis]
                );
            }
        }
    }

    #[test]
    fn encoder_roundtrips_events_and_inactive_objects() {
        let mut payload = SpatialPayload::new(
            48_000,
            vec![
                objectaudio_ir::ObjectState::new(1, [0.0, 1.0, 0.0]),
                objectaudio_ir::ObjectState::new(2, [1.0, 1.0, 0.0]),
            ],
            vec![objectaudio_ir::ObjectEvent {
                offset: 320,
                ramp_samples: 512,
                object: 1,
                state: objectaudio_ir::ObjectState {
                    enabled: false,
                    gain: 0.0,
                    ..objectaudio_ir::ObjectState::new(1, [0.5, 0.0, 0.0])
                },
            }],
        );
        payload.objects[1].enabled = false;
        let bytes = encode(&payload).unwrap();
        let frame = parse(&bytes).unwrap();
        assert_eq!(frame.dynamic_objects().count(), 2);
        assert_eq!(frame.events.len(), 1);
        assert_eq!(frame.events[0].offset, 320);
        assert_eq!(frame.events[0].ramp_samples, 512);
        assert!(!frame.events[0].state.enabled);
    }

    proptest! {
        #[test]
        fn arbitrary_payloads_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse(&bytes);
        }

        #[test]
        fn every_prefix_never_panics(end in 0usize..=EXAMPLE_DATA.len()) {
            let _ = parse(&EXAMPLE_DATA[..end]);
        }
    }
}
