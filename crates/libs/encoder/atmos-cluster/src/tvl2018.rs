// SPDX-License-Identifier: MPL-2.0
//! The subset of the TVL 2018 loudness model used by the clustering criterion.
//!
//! Ported from the vendored `tvl2018.py` (BSD-2-Clause, Copyright (c) 2024
//! Malcolm Slaney). Only the excitation/loudness helpers are needed; the
//! waveform-processing functions are intentionally omitted.

/// Excitation threshold table (Hz -> dB).
const THRESHOLD_X: [f64; 15] = [
    50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 750.0, 800.0,
    1000.0,
];
const THRESHOLD_Y: [f64; 15] = [
    28.18, 23.90, 19.20, 15.68, 12.67, 10.09, 8.08, 6.30, 5.30, 4.50, 3.63, 3.63, 3.63, 3.63, 3.63,
];

/// Low-level gain to the A parameter.
pub const TABLE_G_TO_A_X: [f64; 90] = [
    -24.545310, -23.783970, -22.781690, -21.768540, -20.744420, -19.783050, -18.904310, -18.016050,
    -17.118160, -16.210550, -15.323750, -14.593410, -13.917270, -13.297260, -12.735370, -12.233640,
    -11.752550, -11.238660, -10.751360, -10.291640, -9.860510, -9.459020, -9.088230, -8.721910,
    -8.357150, -8.011990, -7.687150, -7.383380, -7.101450, -6.842130, -6.606230, -6.394580,
    -6.145890, -5.893920, -5.650710, -5.416610, -5.191980, -4.977180, -4.772580, -4.578570,
    -4.395550, -4.201480, -4.005380, -3.814420, -3.628820, -3.274540, -3.106330, -2.944380,
    -2.788940, -2.640270, -2.500420, -2.370150, -2.248200, -2.134870, -2.030460, -1.935310,
    -1.849730, -1.774050, -1.708630, -1.653820, -1.609970, -1.577450, -1.517860, -1.445220,
    -1.374660, -1.306240, -1.240060, -1.176210, -1.114780, -1.055870, -0.999560, -0.945960,
    -0.895180, -0.847310, -0.802460, -0.756630, -0.698340, -0.640290, -0.582510, -0.525010,
    -0.467830, -0.410990, -0.354510, -0.298420, -0.242740, -0.187500, -0.132730, -0.078450,
    -0.024700, 0.000000,
];
pub const TABLE_G_TO_A_Y: [f64; 90] = [
    8.852000, 8.631500, 8.358400, 8.101200, 7.858500, 7.652580, 7.491240, 7.328530, 7.164580,
    6.999540, 6.839570, 6.705590, 6.581150, 6.468690, 6.368130, 6.279700, 6.195830, 6.107190,
    6.024040, 5.946400, 5.878760, 5.824900, 5.775510, 5.727190, 5.679390, 5.634430, 5.592360,
    5.553220, 5.517080, 5.483990, 5.454020, 5.427230, 5.395880, 5.364250, 5.333860, 5.304720,
    5.276880, 5.250640, 5.228000, 5.206670, 5.186600, 5.165390, 5.144010, 5.123260, 5.103140,
    5.064900, 5.046810, 5.029440, 5.012800, 4.996930, 4.982030, 4.968180, 4.955240, 4.943240,
    4.932190, 4.922150, 4.913120, 4.905150, 4.898270, 4.892510, 4.887900, 4.884490, 4.878240,
    4.870630, 4.863240, 4.856090, 4.849180, 4.842520, 4.836110, 4.829980, 4.824120, 4.818540,
    4.813270, 4.808300, 4.803650, 4.798900, 4.792860, 4.786850, 4.780880, 4.774940, 4.769040,
    4.763180, 4.757360, 4.751590, 4.745860, 4.740190, 4.734560, 4.728990, 4.723490, 4.720960,
];

const TABLE_G_TO_ALPHA_X: [f64; 6] = [-25.0, -20.0, -15.0, -10.0, -5.0, 0.0];
const TABLE_G_TO_ALPHA_Y: [f64; 6] = [0.26692, 0.25016, 0.23679, 0.22228, 0.21055, 0.20000];

pub fn erb_number_to_frequency(erb: f64) -> f64 {
    (10.0_f64.powf(erb / 21.366) - 1.0) / 0.004368
}

fn linear(xx: &[f64], yy: &[f64], x: f64) -> f64 {
    let n = xx.len();
    if x <= xx[0] {
        let slope = (yy[1] - yy[0]) / (xx[1] - xx[0]);
        return yy[0] + slope * (x - xx[0]);
    }
    if x >= xx[n - 1] {
        let slope = (yy[n - 1] - yy[n - 2]) / (xx[n - 1] - xx[n - 2]);
        return yy[n - 1] + slope * (x - xx[n - 1]);
    }
    let mut lo = 0;
    let mut hi = n - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if xx[mid] <= x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let t = (x - xx[lo]) / (xx[hi] - xx[lo]);
    yy[lo] + (yy[hi] - yy[lo]) * t
}

/// Monotone cubic Hermite (PCHIP) evaluation with endpoint extrapolation.
fn pchip(xx: &[f64], yy: &[f64], x: f64) -> f64 {
    let n = xx.len();
    if n == 1 {
        return yy[0];
    }
    let mut h = vec![0.0; n - 1];
    let mut delta = vec![0.0; n - 1];
    for i in 0..n - 1 {
        h[i] = xx[i + 1] - xx[i];
        delta[i] = (yy[i + 1] - yy[i]) / h[i];
    }
    let mut slope = vec![0.0; n];
    slope[0] = delta[0];
    slope[n - 1] = delta[n - 2];
    for i in 1..n - 1 {
        if delta[i - 1] * delta[i] <= 0.0 {
            slope[i] = 0.0;
        } else {
            let w1 = 2.0 * h[i] + h[i - 1];
            let w2 = h[i] + 2.0 * h[i - 1];
            slope[i] = (w1 + w2) / (w1 / delta[i - 1] + w2 / delta[i]);
        }
    }

    let segment = if x <= xx[0] {
        0
    } else if x >= xx[n - 1] {
        n - 2
    } else {
        let mut lo = 0;
        let mut hi = n - 1;
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if xx[mid] <= x {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let t = (x - xx[segment]) / h[segment];
    let t2 = t * t;
    let t3 = t2 * t;
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let h11 = t3 - t2;
    h00 * yy[segment]
        + h10 * h[segment] * slope[segment]
        + h01 * yy[segment + 1]
        + h11 * h[segment] * slope[segment + 1]
}

pub fn excitation_threshold_tvl(f: f64) -> f64 {
    if f < 500.0 {
        linear(&THRESHOLD_X, &THRESHOLD_Y, f)
    } else {
        3.63
    }
}

pub fn get_g_tvl(f: f64) -> f64 {
    let threshold = excitation_threshold_tvl(f);
    10.0_f64.powf((3.63 - threshold) / 10.0)
}

pub fn get_erb(f: f64) -> f64 {
    24.673 * (0.004368 * f + 1.0)
}

pub fn get_a(f: f64) -> f64 {
    let g = 10.0 * get_g_tvl(f).log10();
    pchip(&TABLE_G_TO_A_X, &TABLE_G_TO_A_Y, g)
}

pub fn get_alpha(f: f64) -> f64 {
    let g = 10.0 * get_g_tvl(f).log10();
    linear(&TABLE_G_TO_ALPHA_X, &TABLE_G_TO_ALPHA_Y, g)
}

pub fn get_p(f: f64) -> f64 {
    4.0 * f / get_erb(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erb_frequency_roundtrip_is_reasonable() {
        let f = erb_number_to_frequency(10.0);
        assert!(f > 100.0 && f < 1000.0, "{f}");
    }

    #[test]
    fn g_and_a_are_finite_over_the_used_range() {
        for index in 0..=149 {
            let f = erb_number_to_frequency(1.75 + index as f64 * 0.25);
            assert!(get_g_tvl(f).is_finite());
            assert!(get_a(f).is_finite());
            assert!(get_alpha(f).is_finite());
            assert!(get_p(f).is_finite());
        }
    }
}
