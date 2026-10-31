//! Deterministic filterbank vectors plus bank-selection tests.

use num_complex::Complex64;
use objectaudio_hrtf::{
    Filterbank, FilterbankKind, PUBLIC_LATENCY, ROSELLA_LATENCY, RosellaFilterbank, create,
};

#[path = "data/rosella_vectors.rs"]
mod rosella_vectors;
use rosella_vectors as vectors;

fn run_analysis(
    bank: &mut dyn Filterbank,
    input: &[f64],
    slots: usize,
    channels: usize,
) -> (Vec<Complex64>, Vec<Complex64>) {
    let mut qmf = Vec::new();
    let mut hybrid = Vec::new();
    bank.analyze_qmf(input, slots, &mut qmf);
    bank.analyze_hybrid(&qmf, slots, &mut hybrid);
    let _ = channels;
    (qmf, hybrid)
}

#[test]
fn captured_bank_matches_the_reference_vectors() {
    let slots = 24usize;
    let channels = 2usize;
    let mut input = vec![0.0f64; slots * channels * 64];
    for (index, value) in input.iter_mut().enumerate() {
        *value = (((index * 37 + 11) % 257) as i32 - 128) as f64 / 128.0;
    }
    let mut bank = RosellaFilterbank::new(channels);
    let (qmf, hybrid) = run_analysis(&mut bank, &input, slots, channels);
    for (index, expected) in vectors::ANALYSIS {
        let actual = qmf[index];
        let error = (actual.re - expected.0)
            .abs()
            .max((actual.im - expected.1).abs());
        assert!(error < 1e-10, "analysis[{index}] error {error}");
    }
    for (index, expected) in vectors::HYBRID {
        let actual = hybrid[index];
        let error = (actual.re - expected.0)
            .abs()
            .max((actual.im - expected.1).abs());
        assert!(error < 1e-10, "hybrid[{index}] error {error}");
    }

    let mut synthetic = vec![Complex64::new(0.0, 0.0); slots * channels * 77];
    for (index, value) in synthetic.iter_mut().enumerate() {
        *value = Complex64::new(
            (((index * 17 + 3) % 101) as i32 - 50) as f64 / 64.0,
            (((index * 13 + 7) % 97) as i32 - 48) as f64 / 64.0,
        );
    }
    let mut folded = Vec::new();
    let mut time = Vec::new();
    bank.synthesize_hybrid(&synthetic, slots, &mut folded);
    bank.synthesize_qmf(&folded, slots, &mut time);
    for (index, expected) in vectors::FOLD {
        let actual = folded[index];
        let error = (actual.re - expected.0)
            .abs()
            .max((actual.im - expected.1).abs());
        assert!(error < 1e-10, "fold[{index}] error {error}");
    }
    for (index, expected) in vectors::SYNTHESIS {
        let error = (time[index] - expected).abs();
        assert!(error < 1e-9, "synthesis[{index}] error {error}");
    }
}

#[test]
fn captured_bank_is_chunk_invariant_and_resettable() {
    let slots = 24usize;
    let channels = 2usize;
    let mut input = vec![0.0f64; slots * channels * 64];
    for (index, value) in input.iter_mut().enumerate() {
        *value = (((index * 37 + 11) % 257) as i32 - 128) as f64 / 128.0;
    }
    let mut bank = RosellaFilterbank::new(channels);
    let (qmf, hybrid) = run_analysis(&mut bank, &input, slots, channels);
    for _ in 0..2 {
        bank.reset();
        let mut joined_qmf = Vec::new();
        let mut joined_hybrid = Vec::new();
        let mut first = 0usize;
        for count in [0usize, 1, 7, 0, 3, 13] {
            let part = &input[first * channels * 64..(first + count) * channels * 64];
            let (part_qmf, part_hybrid) = run_analysis(&mut bank, part, count, channels);
            joined_qmf.extend(part_qmf);
            joined_hybrid.extend(part_hybrid);
            first += count;
        }
        assert_eq!(joined_qmf, qmf);
        assert_eq!(joined_hybrid, hybrid);
    }
}

#[test]
fn both_banks_report_their_own_latency() {
    assert_eq!(FilterbankKind::Public.latency(), PUBLIC_LATENCY);
    assert_eq!(FilterbankKind::Captured.latency(), ROSELLA_LATENCY);
    let public = create(FilterbankKind::Public, 1);
    let captured = create(FilterbankKind::Captured, 1);
    assert_eq!(public.kind(), FilterbankKind::Public);
    assert_eq!(captured.kind(), FilterbankKind::Captured);
    assert_eq!(public.latency(), 961);
    assert_eq!(captured.latency(), 448);
    assert_eq!(
        FilterbankKind::parse("public"),
        Some(FilterbankKind::Public)
    );
    assert_eq!(FilterbankKind::parse("2"), Some(FilterbankKind::Captured));
    assert_eq!(FilterbankKind::parse("nope"), None);
}

fn impulse_latency(bank: &mut dyn Filterbank) -> usize {
    let slots = 64usize;
    let mut impulse = vec![0.0f64; slots * 64];
    impulse[0] = 1.0;
    let mut qmf = Vec::new();
    let mut hybrid = Vec::new();
    let mut folded = Vec::new();
    let mut output = Vec::new();
    bank.analyze_qmf(&impulse, slots, &mut qmf);
    bank.analyze_hybrid(&qmf, slots, &mut hybrid);
    bank.synthesize_hybrid(&hybrid, slots, &mut folded);
    bank.synthesize_qmf(&folded, slots, &mut output);
    output
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.abs().partial_cmp(&right.1.abs()).unwrap())
        .map(|(index, _)| index)
        .unwrap()
}

#[test]
fn both_banks_have_the_reference_roundtrip_latency() {
    let mut public = create(FilterbankKind::Public, 1);
    let mut captured = create(FilterbankKind::Captured, 1);
    assert_eq!(impulse_latency(public.as_mut()), PUBLIC_LATENCY);
    assert_eq!(impulse_latency(captured.as_mut()), ROSELLA_LATENCY);
}
