//! Parameter-band helpers.

use crate::constants::{FFT_BINS, PARAM_BAND_BOUNDARIES, PARAM_BAND_COUNT};

/// Parameter band containing a nonnegative FFT bin.
pub fn parameter_band_for_bin(bin: usize) -> Option<usize> {
    if bin >= FFT_BINS {
        return None;
    }
    let bin = bin as u16;
    Some(
        PARAM_BAND_BOUNDARIES
            .partition_point(|boundary| *boundary <= bin)
            .saturating_sub(1),
    )
}

/// Half-open range of nonnegative FFT bins in a parameter band.
pub fn parameter_band_range(band: usize) -> core::ops::Range<usize> {
    assert!(band < PARAM_BAND_COUNT, "parameter band out of range");
    let start = PARAM_BAND_BOUNDARIES[band] as usize;
    let end = PARAM_BAND_BOUNDARIES[band + 1] as usize;
    start..end
}

/// Statistical weight of a nonnegative FFT bin. DC and Nyquist occur once;
/// every interior positive-frequency bin occurs together with its conjugate.
pub fn bin_weight(bin: usize) -> f64 {
    if bin == 0 || bin == FFT_BINS - 1 {
        1.0
    } else {
        2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_are_strictly_increasing_and_cover_the_spectrum() {
        assert_eq!(PARAM_BAND_BOUNDARIES.len(), PARAM_BAND_COUNT + 1);
        assert_eq!(PARAM_BAND_BOUNDARIES[0], 0);
        assert_eq!(PARAM_BAND_BOUNDARIES[PARAM_BAND_COUNT] as usize, FFT_BINS);
        assert!(
            PARAM_BAND_BOUNDARIES
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
    }

    #[test]
    fn every_bin_maps_to_its_band() {
        for band in 0..PARAM_BAND_COUNT {
            for bin in parameter_band_range(band) {
                assert_eq!(parameter_band_for_bin(bin), Some(band));
            }
        }
        assert_eq!(parameter_band_for_bin(FFT_BINS), None);
    }

    #[test]
    fn endpoint_weights_are_one() {
        assert_eq!(bin_weight(0), 1.0);
        assert_eq!(bin_weight(FFT_BINS - 1), 1.0);
        assert_eq!(bin_weight(1), 2.0);
    }
}
