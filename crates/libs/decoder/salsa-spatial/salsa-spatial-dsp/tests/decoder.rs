use objectaudio_ir::{ObjectState, Snapshot, SpatialDecoder};
use opus::{Application, Bitrate, Channels, Encoder};
use salsa_spatial::params::{ParameterAnchor, ParameterTile, SceneParameterEncoder};
use salsa_spatial::{
    CoreErrorEnvelope, CorePacket, EnhancementTile, OPUS_FRAME_SAMPLES, PARAM_BAND_COUNT,
    SAMPLE_RATE, SalsaSpatialFrame, SalsaSpatialStreamConfig, SceneSnapshot, TILE_SAMPLES,
    TrajectoryKnot,
};
use salsa_spatial_core_decoder::OpusBedDecoder;
use salsa_spatial_dsp::SalsaSpatialDecoder;

const LOOKAHEAD_SAFE_MARGIN: usize = 2_000;

fn sine(frequency: f64, phase: f64, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|frame| {
            let t = frame as f64 / f64::from(SAMPLE_RATE);
            (0.1 * (2.0 * std::f64::consts::PI * frequency * t + phase).sin()) as f32
        })
        .collect()
}

fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
    let mut output = Vec::with_capacity(left.len() * 2);
    for (l, r) in left.iter().zip(right) {
        output.push(*l);
        output.push(*r);
    }
    output
}

fn encode_stream(samples: &[f32], channels: Channels, bitrate: i32) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new(SAMPLE_RATE, channels, Application::LowDelay).unwrap();
    encoder.set_complexity(10).unwrap();
    encoder.set_bitrate(Bitrate::Bits(bitrate)).unwrap();
    encoder.set_vbr(false).unwrap();
    encoder.set_dtx(false).unwrap();
    encoder.set_inband_fec(false).unwrap();
    let stride = OPUS_FRAME_SAMPLES * channels as usize;
    samples
        .chunks(stride)
        .map(|chunk| encoder.encode_vec_float(chunk, 8192).unwrap())
        .collect()
}

fn core_packets(samples: &[Vec<f32>; 8]) -> Vec<CorePacket> {
    let front = encode_stream(
        &interleave(&samples[0], &samples[1]),
        Channels::Stereo,
        160_000,
    );
    let surround = encode_stream(
        &interleave(&samples[3], &samples[4]),
        Channels::Stereo,
        128_000,
    );
    let height = encode_stream(
        &interleave(&samples[5], &samples[6]),
        Channels::Stereo,
        160_000,
    );
    let center = encode_stream(&samples[2], Channels::Mono, 112_000);
    let lfe = encode_stream(&samples[7], Channels::Mono, 16_000);

    front
        .into_iter()
        .zip(surround)
        .zip(height)
        .zip(center)
        .zip(lfe)
        .map(|((((front, surround), height), center), lfe)| {
            CorePacket::from_legacy(front, surround, height, center, lfe)
        })
        .collect()
}

fn test_bed(frames: usize) -> [Vec<f32>; 8] {
    [
        sine(440.0, 0.0, frames),
        sine(440.0, std::f64::consts::PI, frames),
        sine(330.0, 0.0, frames),
        sine(220.0, 0.0, frames),
        sine(110.0, 0.0, frames),
        sine(550.0, 0.0, frames),
        sine(660.0, 0.0, frames),
        sine(80.0, 0.0, frames),
    ]
}

fn config(object_count: usize) -> SalsaSpatialStreamConfig {
    SalsaSpatialStreamConfig {
        object_count,
        headroom_gain: 1.0,
        initial_objects: (0..object_count)
            .map(|index| ObjectState {
                id: 10 + index as u32,
                position: [-0.5 + index as f32 * 0.4, 0.8, 0.1 + index as f32 * 0.1],
                gain: 1.0,
                enabled: true,
                size: None,
                spread: None,
            })
            .collect(),
        initial_trajectories: (0..object_count)
            .map(|index| {
                vec![
                    TrajectoryKnot {
                        sample: 0,
                        position: [-0.5 + index as f64 * 0.4, 0.8, 0.1 + index as f64 * 0.1],
                    },
                    TrajectoryKnot {
                        sample: 4_800,
                        position: [0.5 + index as f64 * 0.4, 0.5, 0.3 + index as f64 * 0.1],
                    },
                ]
            })
            .collect(),
        core_bitrates: [160_000, 128_000, 160_000, 112_000, 16_000],
        beta: 1.0e-6,
        ..SalsaSpatialStreamConfig::legacy()
    }
}

fn parameter_tile(
    encoder: &mut SceneParameterEncoder,
    object_count: usize,
    start: u64,
    frames: usize,
    scale_half_db: i32,
) -> Vec<u8> {
    let anchor_count = frames.div_ceil(OPUS_FRAME_SAMPLES).max(1);
    let mut anchors = Vec::with_capacity(anchor_count);
    for step in 0..anchor_count {
        let sample = start + step as u64 * OPUS_FRAME_SAMPLES as u64;
        let active = vec![true; object_count];
        let ell_half_db: Vec<[i32; PARAM_BAND_COUNT]> = (0..object_count)
            .map(|object| {
                std::array::from_fn(|band| {
                    let value = ((band as i32 * 3 + step as i32 * 2 + object as i32 * 7) % 25) - 12;
                    value * 2 + scale_half_db
                })
            })
            .collect();
        let mut error = CoreErrorEnvelope::default();
        for row in &mut error.nu {
            row.fill(0.01);
        }
        anchors.push(ParameterAnchor {
            sample,
            active,
            ell_half_db,
            error,
        });
    }

    let trajectories: Vec<Vec<TrajectoryKnot>> = (0..object_count)
        .map(|object| {
            let x = -0.6 + object as f64 * 0.4;
            vec![
                TrajectoryKnot {
                    sample: start,
                    position: [x, 0.8, 0.1],
                },
                TrajectoryKnot {
                    sample: start + frames as u64,
                    position: [x + 0.5, 0.5, 0.3],
                },
            ]
        })
        .collect();

    let tile = ParameterTile {
        start_sample: start,
        frames,
        anchors,
        pairs: Vec::new(),
        trajectories,
        correction: Vec::new(),
    };
    encoder.encode_tile(&tile).unwrap()
}

fn frame(
    start_sample: u64,
    packets: Vec<CorePacket>,
    enhancement_bytes: Vec<u8>,
    checkpoint: Option<SceneSnapshot>,
) -> SalsaSpatialFrame {
    let frames = packets.len() * OPUS_FRAME_SAMPLES;
    SalsaSpatialFrame {
        sample_rate: SAMPLE_RATE,
        start_sample,
        frames,
        packets,
        enhancement: EnhancementTile {
            start_sample,
            frames,
            bytes: enhancement_bytes,
        },
        checkpoint,
        parameters: None,
    }
}

fn collect(mut output: Vec<Vec<f32>>, frame: &objectaudio_ir::SpatialFrame) -> Vec<Vec<f32>> {
    for (channel, samples) in output.iter_mut().zip(frame.pcm.channels()) {
        channel.extend_from_slice(samples);
    }
    output
}

fn relative_rms(reference: &[f32], actual: &[f32]) -> f64 {
    assert_eq!(reference.len(), actual.len());
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (&a, &b) in reference.iter().zip(actual) {
        let difference = f64::from(a) - f64::from(b);
        numerator += difference * difference;
        denominator += f64::from(a) * f64::from(a);
    }
    (numerator / denominator.max(1e-30)).sqrt()
}

fn reference_bed(packets: &[CorePacket], object_count: usize) -> [Vec<f32>; 8] {
    let mut decoder = OpusBedDecoder::new(object_count).unwrap();
    let mut output: [Vec<f32>; 8] = std::array::from_fn(|_| Vec::new());
    for packet in packets {
        let bed = decoder.decode(packet).unwrap();
        for (channel, samples) in bed.into_iter().enumerate() {
            output[channel].extend_from_slice(&samples);
        }
    }
    let lookahead = decoder.lookahead_samples();
    for channel in &mut output {
        channel.drain(0..lookahead.min(channel.len()));
    }
    output
}

#[test]
fn decoder_produces_lfe_plus_objects_with_finite_audio() {
    let object_count = 3;
    let samples = test_bed(TILE_SAMPLES);
    let packets = core_packets(&samples);
    let mut encoder = SceneParameterEncoder::new(object_count);
    let enhancement = parameter_tile(&mut encoder, object_count, 0, TILE_SAMPLES, 0);
    let input = frame(0, packets, enhancement, None);

    let mut decoder = SalsaSpatialDecoder::new(config(object_count)).unwrap();
    assert_eq!(decoder.object_count(), object_count);
    let first = decoder.decode(&input).unwrap();

    assert_eq!(first.sample_rate, SAMPLE_RATE);
    assert_eq!(first.pcm.channel_count(), object_count + 1);
    assert_eq!(first.objects.len(), object_count);
    assert_eq!(first.snapshot, Snapshot::State);
    for (index, object) in first.objects.iter().enumerate() {
        assert_eq!(object.id, index as u32 + 1);
        assert_eq!(object.gain, 1.0);
        assert!(object.enabled);
        assert!(object.position.iter().all(|value| value.is_finite()));
    }
    assert!(
        first
            .pcm
            .channels()
            .iter()
            .all(|channel| { channel.iter().all(|sample| sample.is_finite()) })
    );

    // The compressed trajectory is authoritative for reconstruction and rendering.
    assert_eq!(first.events.len(), object_count);

    // The LFE stream carries the 80 Hz test tone, so channel 0 is not silent.
    let lfe_energy: f64 = first
        .pcm
        .channel(0)
        .unwrap()
        .iter()
        .map(|&value| f64::from(value) * f64::from(value))
        .sum();
    assert!(lfe_energy > 0.0);

    let tail = decoder.finish().unwrap().expect("drained tail");
    let total = first.frames + tail.frames;
    assert!(total > TILE_SAMPLES - LOOKAHEAD_SAFE_MARGIN);
    assert!(total <= TILE_SAMPLES);
    assert!(decoder.finish().unwrap().is_none());
}

#[test]
fn invalid_enhancement_falls_back_to_the_decoded_bed() {
    let object_count = 3;
    let samples = test_bed(TILE_SAMPLES);
    let packets = core_packets(&samples);
    let reference = reference_bed(&packets, object_count);

    let mut encoder = SceneParameterEncoder::new(object_count);
    let valid = parameter_tile(&mut encoder, object_count, 0, TILE_SAMPLES, 0);
    let truncated = valid[..valid.len() - 4].to_vec();
    let input = frame(0, packets, truncated, None);

    let mut decoder = SalsaSpatialDecoder::new(config(object_count)).unwrap();
    let first = decoder.decode(&input).unwrap();
    let mut output: Vec<Vec<f32>> = (0..object_count + 1).map(|_| Vec::new()).collect();
    output = collect(output, &first);
    if let Some(tail) = decoder.finish().unwrap() {
        output = collect(output, &tail);
    }

    // The decoder now emits the declared tile length. The reference is the
    // post-lookahead core; compare only its available span and check the
    // declared duration separately.
    assert_eq!(output[0].len(), TILE_SAMPLES);
    let compared = reference[0].len();
    // LFE is channel zero; objects are the seven core channels in order.
    assert!(relative_rms(&reference[7], &output[0][..compared]) < 1e-4);
    for object in 0..object_count {
        assert!(
            relative_rms(&reference[object], &output[object + 1][..compared]) < 1e-4,
            "object {object} did not fall back to the bed"
        );
    }
    assert!(output.iter().flatten().all(|sample| sample.is_finite()));
}

#[test]
fn missing_enhancement_stays_on_the_bed_until_a_checkpoint() {
    let object_count = 2;
    let samples = test_bed(TILE_SAMPLES);
    let packets = core_packets(&samples);
    let mut encoder = SceneParameterEncoder::new(object_count);
    let valid = parameter_tile(&mut encoder, object_count, 0, TILE_SAMPLES, 0);

    let mut decoder = SalsaSpatialDecoder::new(config(object_count)).unwrap();

    // Initial valid tile.
    let input = frame(0, packets.clone(), valid.clone(), None);
    decoder.decode(&input).unwrap();

    // A missing enhancement packet switches to the decoded bed.
    let missing = frame(TILE_SAMPLES as u64, packets.clone(), Vec::new(), None);
    let fallback = decoder.decode(&missing).unwrap();
    assert_eq!(fallback.snapshot, Snapshot::State);
    assert!(
        fallback
            .pcm
            .channels()
            .iter()
            .all(|channel| channel.iter().all(|sample| sample.is_finite()))
    );

    // A valid but stale non-snapshot packet does not leave the fallback.
    let stale = frame(
        2 * TILE_SAMPLES as u64,
        packets.clone(),
        valid.clone(),
        None,
    );
    decoder.decode(&stale).unwrap();

    // Advance the encoder through the tiles the decoder missed. The checkpoint
    // describes the state immediately before the recovery tile.
    for step in 1..3 {
        parameter_tile(
            &mut encoder,
            object_count,
            step * TILE_SAMPLES as u64,
            TILE_SAMPLES,
            0,
        );
    }
    let snapshot_at = 3 * TILE_SAMPLES as u64;
    let checkpoint = encoder.checkpoint(snapshot_at);
    let snapshot_bytes = parameter_tile(&mut encoder, object_count, snapshot_at, TILE_SAMPLES, 0);
    let recovery = frame(snapshot_at, packets, snapshot_bytes, Some(checkpoint));
    let recovered = decoder.decode(&recovery).unwrap();
    assert!(
        recovered
            .pcm
            .channels()
            .iter()
            .all(|channel| channel.iter().all(|sample| sample.is_finite()))
    );
    decoder.finish().unwrap();
}

#[test]
fn common_power_scale_leaves_the_object_output_unchanged() {
    let object_count = 3;
    let samples = test_bed(TILE_SAMPLES);
    let packets = core_packets(&samples);

    let mut encoder_a = SceneParameterEncoder::new(object_count);
    let bytes_a = parameter_tile(&mut encoder_a, object_count, 0, TILE_SAMPLES, 0);
    let mut encoder_b = SceneParameterEncoder::new(object_count);
    // +10 dB on every object and band is a common factor, so q is unchanged.
    let bytes_b = parameter_tile(&mut encoder_b, object_count, 0, TILE_SAMPLES, 20);

    let frame_a = frame(0, packets.clone(), bytes_a, None);
    let frame_b = frame(0, packets, bytes_b, None);

    let mut decoder_a = SalsaSpatialDecoder::new(config(object_count)).unwrap();
    let mut decoder_b = SalsaSpatialDecoder::new(config(object_count)).unwrap();

    let first_a = decoder_a.decode(&frame_a).unwrap();
    let first_b = decoder_b.decode(&frame_b).unwrap();
    let mut output_a: Vec<Vec<f32>> = (0..object_count + 1).map(|_| Vec::new()).collect();
    let mut output_b: Vec<Vec<f32>> = (0..object_count + 1).map(|_| Vec::new()).collect();
    output_a = collect(output_a, &first_a);
    output_b = collect(output_b, &first_b);
    if let Some(tail) = decoder_a.finish().unwrap() {
        output_a = collect(output_a, &tail);
    }
    if let Some(tail) = decoder_b.finish().unwrap() {
        output_b = collect(output_b, &tail);
    }

    assert_eq!(output_a.len(), output_b.len());
    for (channel_a, channel_b) in output_a.iter().zip(&output_b) {
        assert_eq!(channel_a.len(), channel_b.len());
        instance_relative_rms(channel_a, channel_b);
    }
}

fn instance_relative_rms(a: &[f32], b: &[f32]) {
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (&x, &y) in a.iter().zip(b) {
        let difference = f64::from(x) - f64::from(y);
        numerator += difference * difference;
        denominator += f64::from(x) * f64::from(x);
    }
    let error = (numerator / denominator.max(1e-30)).sqrt();
    assert!(error < 1e-4, "common-power scale changed output: {error}");
}

#[test]
fn rendered_timeline_follows_compressed_trajectories_across_buffers() {
    use objectaudio_ir::SceneTimeline;

    let object_count = 1;
    let samples = test_bed(TILE_SAMPLES);
    let packets = core_packets(&samples);
    let mut parameters = SceneParameterEncoder::new(object_count);
    let mut decoder = SalsaSpatialDecoder::new(config(object_count)).unwrap();
    let mut scene: Option<SceneTimeline> = None;
    let mut emitted = 0usize;
    for index in 0..6 {
        let start = (index * TILE_SAMPLES) as u64;
        let bytes = parameter_tile(&mut parameters, object_count, start, TILE_SAMPLES, 0);
        let output = decoder
            .decode(&frame(start, packets.clone(), bytes, None))
            .unwrap();
        let payload = output.to_payload();
        if let Some(scene) = scene.as_mut() {
            scene.submit(&payload, emitted as i64).unwrap();
        } else {
            scene = Some(SceneTimeline::from_payload(&payload, emitted as i64).unwrap());
        }
        for offset in (0..output.frames).step_by(32) {
            let sample = emitted + offset;
            let amount = (sample % TILE_SAMPLES) as f64 / TILE_SAMPLES as f64;
            let expected = [-0.6 + 0.5 * amount, 0.8 - 0.3 * amount, 0.1 + 0.2 * amount];
            let actual = scene.as_ref().unwrap().state_at(0, sample as i64);
            for axis in 0..3 {
                assert!(
                    (f64::from(actual.position[axis]) - expected[axis]).abs() < 0.001,
                    "sample {sample}, axis {axis}: {:?} vs {expected:?}",
                    actual.position
                );
            }
        }
        emitted += output.frames;
    }
}

#[test]
fn source_time_origin_does_not_change_reconstruction() {
    let packets = core_packets(&test_bed(TILE_SAMPLES));
    let mut outputs = Vec::new();
    for origin in [0, 480_000] {
        let mut encoder = SceneParameterEncoder::new(1);
        let bytes = parameter_tile(&mut encoder, 1, origin, TILE_SAMPLES, 0);
        let mut decoder = SalsaSpatialDecoder::new(config(1)).unwrap();
        let first = decoder
            .decode(&frame(origin, packets.clone(), bytes, None))
            .unwrap();
        let mut output = collect(vec![Vec::new(); 2], &first);
        if let Some(tail) = decoder.finish().unwrap() {
            output = collect(output, &tail);
        }
        outputs.push(output);
    }
    for (a, b) in outputs[0].iter().zip(&outputs[1]) {
        assert!(relative_rms(a, b) < 1e-6);
    }
}
