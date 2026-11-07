use salsa_spatial::TrajectoryKnot;
use salsa_spatial::{CORE_CHANNEL_COUNT, FFT_BINS, PARAM_BAND_COUNT};
use salsa_spatial_dsp::{
    StatisticsAccumulator, StftConfig, Synthesis, analyze, core_gain_column,
    covariance_from_band_powers, estimate_objects, interpolated_core_columns, solve_regularized,
    synthesize,
};

#[test]
fn contract_exports_are_usable() {
    let column = core_gain_column([0.0, 1.0, 0.0]);
    let norm: f64 = column.iter().map(|value| value * value).sum();
    assert!((norm - 1.0).abs() < 1e-12);

    let knots = [
        TrajectoryKnot {
            sample: 0,
            position: [-1.0, 1.0, 0.0],
        },
        TrajectoryKnot {
            sample: 960,
            position: [1.0, 1.0, 0.0],
        },
    ];
    let columns = interpolated_core_columns(&knots, 0, 961);
    assert_eq!(columns.len(), 961);

    let signal: Vec<f32> = (0..4_800)
        .map(|index| (index as f64 * 0.07).sin() as f32)
        .collect();
    let spectra = analyze(&signal, StftConfig::default());
    let reconstructed = synthesize(&spectra, signal.len());
    assert_eq!(reconstructed.len(), signal.len());

    let mut synthesis = Synthesis::with_total(StftConfig::default(), signal.len());
    let mut streamed = Vec::new();
    for spectrum in &spectra {
        synthesis.push_spectrum(spectrum);
        streamed.extend(synthesis.drain());
    }
    streamed.extend(synthesis.finish(signal.len()));
    assert_eq!(streamed.len(), signal.len());

    let powers = vec![[0.5; PARAM_BAND_COUNT]; 2];
    let covariance = covariance_from_band_powers(&powers, &[]);
    let d = vec![
        [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    ];
    let nu = [0.0; CORE_CHANNEL_COUNT];
    let _ = solve_regularized(&d, &covariance, 0, &nu, 1e-3).unwrap();
    let core = vec![num_complex::Complex::new(0.0, 0.0); CORE_CHANNEL_COUNT * FFT_BINS];
    let nu_bands = [[0.0; CORE_CHANNEL_COUNT]; PARAM_BAND_COUNT];
    let unobservable = [false; PARAM_BAND_COUNT];
    let objects = estimate_objects(
        0,
        &core,
        &d,
        &covariance,
        &nu_bands,
        1e-3,
        true,
        &unobservable,
    )
    .unwrap();
    assert_eq!(objects.len(), 2);

    let mut accumulator = StatisticsAccumulator::new(2);
    accumulator.push_next_hop(
        &vec![num_complex::Complex::new(0.0, 0.0); 2 * FFT_BINS],
        &core,
        &core,
    );
    assert_eq!(accumulator.hop_count(), 1);
}
