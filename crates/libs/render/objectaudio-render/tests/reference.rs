//! Opt-in comparison against a reference speaker-renderer dump.
//!
//! Set `SALSA_SPEAKER_REFERENCE` to a reference dump file. The format is
//! intentionally simple so the test has no third-party binary dependency in the
//! default `cargo test` run.
//!
//! The dump must come from a reference renderer with the same programme-control
//! model: warp 0 leaves the floor Y alone, warp 1 doubles it, and layouts
//! without OAMD trim semantics use disabled trim. The per-layout control
//! golden values live in the `objectaudio-render` unit tests; this file only
//! exercises the default controls.

use std::fs;

use objectaudio_render::{
    SpeakerControls, SpeakerMetadata, SpeakerRenderer, layout_by_speaker_bitfield,
};

/// The reference dump uses a 15-object layout.
const REFERENCE_OBJECTS: usize = 15;

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn take(&mut self, length: usize) -> &'a [u8] {
        let end = self.position + length;
        let slice = &self.data[self.position..end];
        self.position = end;
        slice
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take(2).try_into().unwrap())
    }

    fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take(4).try_into().unwrap())
    }
}

#[test]
fn matches_reference_speaker_renderer() {
    let Some(path) = std::env::var_os("SALSA_SPEAKER_REFERENCE") else {
        eprintln!("SALSA_SPEAKER_REFERENCE is unset; skipping the reference comparison");
        return;
    };
    let data = fs::read(path).expect("read reference");
    let mut reader = Reader::new(&data);
    assert_eq!(reader.take(8), b"SALSAOR1");
    let layout_count = reader.u32();
    let frames = reader.u32();
    let samples = reader.u32() as usize;
    assert_eq!(samples, 1536);

    let mut worst_snr = f64::INFINITY;
    let mut worst_max = 0.0f64;
    for _ in 0..layout_count {
        let bitfield = reader.u32();
        let channels = reader.u32() as usize;
        let layout = layout_by_speaker_bitfield(bitfield).expect("known layout bitfield");
        assert_eq!(layout.channel_count(), channels);
        let mut renderer = SpeakerRenderer::new(layout, REFERENCE_OBJECTS);

        let mut input = vec![0.0f32; samples * (REFERENCE_OBJECTS + 1)];
        let mut output = vec![0.0f64; samples * channels];
        for _ in 0..frames {
            let offset = reader.u32();
            let ramp_samples = reader.u32();
            let mut positions = vec![[0u16; 3]; REFERENCE_OBJECTS];
            for position in &mut positions {
                for coordinate in position {
                    *coordinate = reader.u16();
                }
            }
            for sample in &mut input {
                *sample = reader.f32();
            }
            let expected: Vec<f32> = (0..samples * channels).map(|_| reader.f32()).collect();
            let metadata = SpeakerMetadata::new(offset, ramp_samples, &positions);
            let controls = SpeakerControls::default_for(layout);
            renderer
                .process(&input, samples, &[metadata], &controls, &mut output)
                .expect("renderer process");

            let mut max_error = 0.0f64;
            let mut signal = 0.0f64;
            let mut noise = 0.0f64;
            for (actual, expected) in output.iter().zip(&expected) {
                let expected = f64::from(*expected);
                let difference = actual - expected;
                max_error = max_error.max(difference.abs());
                signal += expected * expected;
                noise += difference * difference;
            }
            let snr = 10.0 * (signal / noise.max(1e-30)).log10();
            worst_snr = worst_snr.min(snr);
            worst_max = worst_max.max(max_error);
            assert!(
                snr >= 100.0 || max_error <= 1.0e-4,
                "layout {} bitfield {bitfield:#x}: SNR {snr:.2} dB, max {max_error:.3e}",
                layout.name
            );
        }
    }
    println!("worst speaker SNR {worst_snr:.2} dB, max abs error {worst_max:.3e}");
}
