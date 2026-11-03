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
    let mut effective = input.to_vec();
    for (offset, row) in effective.chunks_exact_mut(channels).enumerate() {
        for (object, value) in row[1..].iter_mut().enumerate() {
            *value *= scene
                .state_at(object, start + offset as i64)
                .effective_gain();
        }
    }
    let positions: Vec<_> = (0..samples)
        .step_by(BLOCK_SAMPLES)
        .flat_map(|offset| {
            [offset, offset + BLOCK_SAMPLES - 1].map(|sample| {
                scene
                    .states_at(start + sample as i64)
                    .into_iter()
                    .map(|state| adm_to_q15(state.position))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let metadata: Vec<_> = positions
        .iter()
        .enumerate()
        .map(|(index, positions)| {
            SpeakerMetadata::new(
                (index / 2 * BLOCK_SAMPLES) as u32,
                if index % 2 == 0 {
                    0
                } else {
                    BLOCK_SAMPLES as u32
                },
                positions,
            )
        })
        .collect();
    let controls = match scene.calibration_at(start) {
        Some(calibration) => controls_from_calibration(calibration, renderer.layout())?,
        None => SpeakerControls::default_for(renderer.layout()),
    };
    renderer.process(&effective, samples, &metadata, &controls, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout_by_name;
    use crate::{SpeakerRenderer, render_scene};
    use objectaudio_ir::{ObjectState, ProgramCalibration, SceneTimeline, SpatialPayload};

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
