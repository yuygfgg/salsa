//! Unit tests for the evaluation error metric used by the `evaluate` example.
//!
//! The helper is duplicated here deliberately; the example is a binary target
//! and this keeps the metric covered by `cargo test`.

const SILENCE_FLOOR_DB: f64 = -60.0;
const SILENCE_RMS: f64 = 1.0e-6;

fn normalized_error_from_sums(
    reference_energy: f64,
    difference_energy: f64,
    samples: usize,
) -> f64 {
    if samples == 0 {
        return SILENCE_FLOOR_DB;
    }
    let mean_square = reference_energy / samples as f64;
    if !mean_square.is_finite() || mean_square <= SILENCE_RMS * SILENCE_RMS {
        return SILENCE_FLOOR_DB;
    }
    let ratio = (difference_energy / reference_energy).sqrt();
    (20.0 * ratio.log10()).max(SILENCE_FLOOR_DB)
}

fn normalized_error_db(reference: &[f64], candidate: &[f64]) -> f64 {
    assert_eq!(reference.len(), candidate.len());
    let mut reference_energy = 0.0;
    let mut difference_energy = 0.0;
    for (reference_value, candidate_value) in reference.iter().zip(candidate) {
        reference_energy += reference_value * reference_value;
        let difference = candidate_value - reference_value;
        difference_energy += difference * difference;
    }
    normalized_error_from_sums(reference_energy, difference_energy, reference.len())
}

#[test]
fn identical_signals_hit_the_silence_floor() {
    let signal: Vec<f64> = (0..1024).map(|index| (index as f64 * 0.01).sin()).collect();
    assert_eq!(normalized_error_db(&signal, &signal), SILENCE_FLOOR_DB);
}

#[test]
fn silent_reference_hits_the_silence_floor() {
    let reference = vec![0.0; 256];
    let candidate = vec![0.0; 256];
    assert_eq!(
        normalized_error_db(&reference, &candidate),
        SILENCE_FLOOR_DB
    );
}

#[test]
fn half_amplitude_error_is_minus_six_db() {
    let reference = vec![1.0; 512];
    let candidate = vec![0.5; 512];
    let error = normalized_error_db(&reference, &candidate);
    assert!((error - (-6.020_599_913_279_624)).abs() < 1e-12, "{error}");
}

#[test]
fn ten_percent_error_is_minus_twenty_db() {
    let reference = vec![1.0; 512];
    let candidate = vec![1.1; 512];
    let error = normalized_error_db(&reference, &candidate);
    assert!((error - (-20.0)).abs() < 1e-12, "{error}");
}

#[test]
fn energy_sums_are_aggregated_before_the_ratio() {
    // Two equal-energy objects: one exact, one with a 0.5 ratio error.
    // Aggregate RMS ratio = sqrt(0.25/2) = 0.3535..., about -9.03 dB.
    let reference_energy = 2.0;
    let difference_energy = 0.25;
    let error = normalized_error_from_sums(reference_energy, difference_energy, 2);
    assert!((error - (-9.030_899_869_919_435)).abs() < 1e-12, "{error}");
}

#[test]
fn empty_program_hits_the_silence_floor() {
    assert_eq!(normalized_error_from_sums(0.0, 0.0, 0), SILENCE_FLOOR_DB);
}
