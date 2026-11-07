use objectaudio_ir::{ObjectEvent, ObjectState, SpatialParser, Transition};
use salsa_spatial::opus_ext::{append_extension, strip_extension};
use salsa_spatial::{
    CorePacket, EnhancementTile, IamfReader, IamfWriter, OPUS_FRAME_SAMPLES, SAMPLE_RATE,
    SalsaSpatialFrame, SalsaSpatialParser, SalsaSpatialStreamConfig, TILE_SAMPLES, TrajectoryKnot,
};

fn config() -> SalsaSpatialStreamConfig {
    SalsaSpatialStreamConfig {
        object_count: 3,
        headroom_gain: std::f64::consts::FRAC_1_SQRT_2,
        initial_objects: vec![
            ObjectState::new(10, [0.1, 0.2, 0.3]),
            ObjectState {
                id: 11,
                position: [-0.4, 0.5, 0.0],
                gain: 0.75,
                enabled: true,
                size: Some(0.25),
                spread: Some(0.5),
            },
            ObjectState {
                id: 12,
                position: [0.0, 1.0, -0.2],
                gain: 0.0,
                enabled: false,
                size: None,
                spread: None,
            },
        ],
        initial_trajectories: vec![
            vec![
                TrajectoryKnot {
                    sample: 0,
                    position: [0.1, 0.2, 0.3],
                },
                TrajectoryKnot {
                    sample: 960,
                    position: [0.2, 0.3, 0.4],
                },
            ],
            vec![TrajectoryKnot {
                sample: 0,
                position: [-0.4, 0.5, 0.0],
            }],
            Vec::new(),
        ],
        core_bitrates: [160_000, 128_000, 160_000, 112_000, 16_000],
        beta: 1.0e-6,
    }
}

fn opus_packet(tag: u8) -> Vec<u8> {
    // Code 0, CELT fullband 20 ms mono, followed by arbitrary frame bytes.
    let mut packet = vec![(31 << 3) | 0b000];
    packet.extend_from_slice(&[tag; 7]);
    packet
}

fn frame(index: u64) -> SalsaSpatialFrame {
    let start_sample = index * TILE_SAMPLES as u64;
    let packets = (0..5u8)
        .map(|packet_index| CorePacket {
            front: opus_packet(0x10 + packet_index),
            surround: opus_packet(0x20 + packet_index),
            height: opus_packet(0x30 + packet_index),
            center: opus_packet(0x40 + packet_index),
            lfe: opus_packet(0x50 + packet_index),
        })
        .collect();
    let enhancement_bytes: Vec<u8> = (0..(37 + index as usize * 11))
        .map(|value| (value as u8).wrapping_mul(17))
        .collect();
    let objects = vec![
        ObjectState::new(10, [0.1 + index as f32 * 0.01, 0.2, 0.3]),
        ObjectState {
            id: 11,
            position: [-0.4, 0.5, 0.0],
            gain: 0.75,
            enabled: true,
            size: Some(0.25),
            spread: Some(0.5),
        },
    ];
    let events = vec![ObjectEvent {
        offset: 240,
        transition: Transition::Position(480),
        state: ObjectState::new(11, [-0.2, 0.6, 0.1]),
    }];
    SalsaSpatialFrame {
        sample_rate: SAMPLE_RATE,
        start_sample,
        frames: TILE_SAMPLES,
        packets,
        enhancement: EnhancementTile {
            start_sample,
            frames: TILE_SAMPLES,
            bytes: enhancement_bytes,
            snapshot: index.is_multiple_of(10),
        },
        objects,
        events,
        snapshot: index == 0,
        parameters: None,
    }
}

/// Ordinary tiles omit the repeated scene declaration; the first tile and
/// one-second snapshots carry it for random access. Compare the fields that
/// the transport actually transmits.
fn assert_transport_eq(actual: &SalsaSpatialFrame, expected: &SalsaSpatialFrame) {
    assert_eq!(actual.sample_rate, expected.sample_rate);
    assert_eq!(actual.start_sample, expected.start_sample);
    assert_eq!(actual.frames, expected.frames);
    assert_eq!(actual.packets, expected.packets);
    assert_eq!(actual.enhancement, expected.enhancement);
    assert_eq!(actual.snapshot, expected.snapshot);
    if expected.snapshot || expected.start_sample == 0 {
        assert_eq!(actual.objects, expected.objects);
        assert_eq!(actual.events, expected.events);
    } else {
        assert!(actual.objects.is_empty());
        assert!(actual.events.is_empty());
    }
}

fn write_stream(frames: &[SalsaSpatialFrame], config: &SalsaSpatialStreamConfig) -> Vec<u8> {
    let mut writer = IamfWriter::new(config).unwrap();
    let mut stream = Vec::new();
    for frame in frames {
        stream.extend(writer.write_frame(frame).unwrap());
    }
    stream.extend(writer.finish().unwrap());
    stream
}

#[test]
fn sequence_header_probe_uses_obu_framing() {
    use salsa_spatial::iamf::has_sequence_header;

    let stream = write_stream(&[frame(0)], &config());
    assert!(has_sequence_header(&stream));
    for end in 0..8 {
        assert!(!has_sequence_header(&stream[..end]));
    }
    assert!(!has_sequence_header(b"\xf8\x06other!"));
    assert!(!has_sequence_header(b"\x00\x06iamf\x01\x01"));
    assert!(has_sequence_header(b"\xf8\x86\x00iamf\x01\x01"));
    assert!(has_sequence_header(b"\xf9\x08\x01\xffiamf\x01\x01"));
}

#[test]
fn writer_reader_round_trip_three_tiles() {
    let config = config();
    let frames: Vec<_> = (0..3).map(frame).collect();
    let stream = write_stream(&frames, &config);

    let mut reader = IamfReader::new();
    let mut parsed = reader.push_bytes(&stream).unwrap();
    parsed.extend(reader.flush().unwrap());

    assert_eq!(reader.config(), Some(&config));
    assert_eq!(parsed.len(), frames.len());
    for (expected, actual) in frames.iter().zip(parsed.iter()) {
        assert_transport_eq(actual, expected);
        assert_eq!(actual.packets.len(), 5);
    }
}

#[test]
fn fragmented_pushes_produce_the_same_frames() {
    let config = config();
    let frames: Vec<_> = (0..3).map(frame).collect();
    let stream = write_stream(&frames, &config);

    let mut reader = IamfReader::new();
    let mut parsed = Vec::new();
    for byte in &stream {
        parsed.extend(reader.push_bytes(std::slice::from_ref(byte)).unwrap());
    }
    parsed.extend(reader.flush().unwrap());

    assert_eq!(reader.config(), Some(&config));
    assert_eq!(parsed.len(), frames.len());
    for (expected, actual) in frames.iter().zip(parsed.iter()) {
        assert_transport_eq(actual, expected);
    }
}

#[test]
fn truncated_stream_returns_an_error_instead_of_panicking() {
    let config = config();
    let frames: Vec<_> = (0..2).map(frame).collect();
    let stream = write_stream(&frames, &config);
    let truncated = &stream[..stream.len() - 1];

    let mut reader = IamfReader::new();
    let _ = reader.push_bytes(truncated);
    assert!(reader.flush().is_err());
}

#[test]
fn opus_extension_round_trips_and_leaves_plain_packets_unchanged() {
    let packet = opus_packet(0xaa);
    let payload = b"opaque salsa enhancement bytes".to_vec();
    let framed = append_extension(&packet, &payload).unwrap();
    let (stripped, extracted) = strip_extension(&framed).unwrap();
    assert_eq!(stripped, packet);
    assert_eq!(extracted, Some(payload));

    let (unchanged, none) = strip_extension(&packet).unwrap();
    assert_eq!(unchanged, packet);
    assert_eq!(none, None);
}

#[test]
fn parser_implements_spatial_parser_and_flushes() {
    fn require_spatial_parser<P: SpatialParser<Frame = SalsaSpatialFrame>>() {}
    require_spatial_parser::<SalsaSpatialParser>();

    let config = config();
    let frames: Vec<_> = (0..3).map(frame).collect();
    let stream = write_stream(&frames, &config);

    let mut parser = SalsaSpatialParser::new();
    let mut parsed = parser.push_bytes(&stream).unwrap();
    parsed.extend(parser.flush().unwrap());

    assert_eq!(parsed.len(), frames.len());
    for (expected, actual) in frames.iter().zip(parsed.iter()) {
        assert_transport_eq(actual, expected);
    }
    assert_eq!(parser.config(), Some(&config));
}

#[test]
fn final_shorter_tile_is_emitted_by_flush() {
    let config = config();
    let mut whole = frame(0);
    whole.packets.truncate(2);
    whole.frames = 2 * OPUS_FRAME_SAMPLES;
    whole.enhancement.frames = whole.frames;
    // The enhancement covers the same two 20 ms intervals.
    let stream = write_stream(&[whole.clone()], &config);

    let mut reader = IamfReader::new();
    let mut parsed = reader.push_bytes(&stream).unwrap();
    parsed.extend(reader.flush().unwrap());
    assert_eq!(parsed, vec![whole]);
}
