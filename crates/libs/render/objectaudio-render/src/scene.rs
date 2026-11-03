//! Project the shared scene onto the speaker renderer's fixed control grid.

use objectaudio_ir::{SceneTimeline, adm_to_q15};

use crate::{
    BLOCK_SAMPLES, RenderError, SpeakerControls, SpeakerMetadata, SpeakerRenderer,
    controls_from_calibration,
};

/// Evaluate source amplitude at audio rate and position at control-block
/// endpoints. The low-level renderer interpolates speaker gains inside each
/// block. This approximation depends on the DSP grid, not input buffer sizes.
pub fn render_scene(
    renderer: &mut SpeakerRenderer,
    scene: &SceneTimeline,
    input: &[f32],
    start: i64,
    samples: usize,
    output: &mut [f64],
) -> Result<(), RenderError> {
    if scene.object_count() != renderer.object_count() {
        return Err(RenderError::ObjectCount {
            expected: renderer.object_count(),
            actual: scene.object_count(),
        });
    }
    let channels = scene.object_count() + 1;
    if input.len() != samples * channels {
        return Err(RenderError::InputSize);
    }
    if !samples.is_multiple_of(BLOCK_SAMPLES) {
        return Err(RenderError::BlockAlignment);
    }
    let output_channels = renderer.layout().channel_count();
    if output.len() < samples * output_channels {
        return Err(RenderError::OutputSize(output_channels));
    }
    let mut effective = input.to_vec();
    for (offset, row) in effective.chunks_exact_mut(channels).enumerate() {
        for (object, value) in row[1..].iter_mut().enumerate() {
            *value *= scene
                .state_at(object, start + offset as i64)
                .effective_gain();
        }
    }
    for offset in (0..samples).step_by(BLOCK_SAMPLES) {
        let sample = start + offset as i64;
        let positions = [sample, sample + BLOCK_SAMPLES as i64 - 1].map(|sample| {
            scene
                .states_at(sample)
                .into_iter()
                .map(|state| adm_to_q15(state.position))
                .collect::<Vec<_>>()
        });
        let metadata = [
            SpeakerMetadata::new(0, 0, &positions[0]),
            SpeakerMetadata::new(0, BLOCK_SAMPLES as u32, &positions[1]),
        ];
        // Calibration is discrete. Sample it at each control-block start;
        // neither position interpolation nor input buffering retimes it.
        let controls = match scene.calibration_at(sample) {
            Some(calibration) => controls_from_calibration(calibration, renderer.layout())?,
            None => SpeakerControls::default_for(renderer.layout()),
        };
        renderer.process(
            &effective[offset * channels..(offset + BLOCK_SAMPLES) * channels],
            BLOCK_SAMPLES,
            &metadata,
            &controls,
            &mut output[offset * output_channels..(offset + BLOCK_SAMPLES) * output_channels],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout_by_name;
    use crate::{SpeakerRenderer, render_scene};
    use objectaudio_ir::{ObjectState, ProgramCalibration, SceneTimeline, SpatialPayload};

    #[test]
    fn calibration_updates_do_not_depend_on_render_buffer_size() {
        let layout = layout_by_name("5.1").unwrap();
        let mut payload =
            SpatialPayload::new(48_000, vec![ObjectState::new(1, [-1.0, -1.0, 0.0])], vec![])
                .with_calibration(ProgramCalibration::default());
        let mut scene = SceneTimeline::from_payload(&payload, 0).unwrap();
        payload.snapshot = objectaudio_ir::Snapshot::Continue;
        let mut trimmed = ProgramCalibration {
            warp_mode: 1,
            global_trim_mode: 2,
            ..ProgramCalibration::default()
        };
        trimmed.configs.fill(objectaudio_ir::TrimRecord::explicit(
            None,
            Some(-6.0),
            None,
            None,
            None,
        ));
        for (at, calibration) in [
            (
                64,
                ProgramCalibration {
                    bypass: vec![true],
                    ..ProgramCalibration::default()
                },
            ),
            (96, trimmed),
            (129, ProgramCalibration::default()),
        ] {
            payload.calibration = Some(calibration);
            scene.submit(&payload, at).unwrap();
        }
        let samples = 192;
        let input = [0.0, 1.0].repeat(samples);
        let mut whole = vec![0.0; samples * layout.channel_count()];
        render_scene(
            &mut SpeakerRenderer::new(layout, 1),
            &scene,
            &input,
            0,
            samples,
            &mut whole,
        )
        .unwrap();
        for block in [32, 64, 96] {
            let mut renderer = SpeakerRenderer::new(layout, 1);
            let mut split = vec![0.0; whole.len()];
            for start in (0..samples).step_by(block) {
                render_scene(
                    &mut renderer,
                    &scene,
                    &input[start * 2..(start + block) * 2],
                    start as i64,
                    block,
                    &mut split
                        [start * layout.channel_count()..(start + block) * layout.channel_count()],
                )
                .unwrap();
            }
            assert_eq!(whole, split, "buffer size {block}");
        }
        let surround = |sample| whole[sample * layout.channel_count() + 4];
        assert!((surround(63) - 10.0f64.powf(-3.0 / 20.0)).abs() < 1e-6);
        assert_eq!(surround(64), 1.0);
        assert!((surround(96) - 10.0f64.powf(-6.0 / 20.0)).abs() < 1e-6);
        assert_eq!(surround(129), surround(128));
        assert!((surround(160) - surround(63)).abs() < 1e-6);
    }

    #[test]
    fn applies_the_scene_calibration_to_speaker_rendering() {
        let payload = SpatialPayload::new(
            48_000,
            vec![ObjectState::new(1, [-1.0, 0.75, 0.0])],
            Vec::new(),
        )
        .with_calibration(ProgramCalibration {
            warp_mode: 1,
            ..ProgramCalibration::default()
        });
        let scene = SceneTimeline::from_payload(&payload, 0).unwrap();
        let layout = layout_by_name("5.1").unwrap();
        let mut renderer = SpeakerRenderer::new(layout, 1);
        let mut input = vec![0.0f32; BLOCK_SAMPLES * 2];
        for sample in 0..BLOCK_SAMPLES {
            input[sample * 2 + 1] = 1.0;
        }
        let mut output = vec![0.0; BLOCK_SAMPLES * layout.channel_count()];
        render_scene(&mut renderer, &scene, &input, 0, BLOCK_SAMPLES, &mut output).unwrap();
        assert!((output[0] - 0.859_732_194_193_924_4).abs() <= 1e-13);
        assert!((output[4] - 0.356_124_806_249_951_6).abs() <= 1e-13);
    }
}
