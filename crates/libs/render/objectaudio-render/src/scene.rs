//! Project the shared scene onto the speaker renderer's fixed control grid.

use objectaudio_ir::{SceneTimeline, adm_to_q15};

use crate::{BLOCK_SAMPLES, RenderError, SpeakerMetadata, SpeakerRenderer};

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
    renderer.process(&effective, samples, &metadata, output)
}
