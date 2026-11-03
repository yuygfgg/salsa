//! Programme speaker controls selected for one output layout.
//!
//! The IR carries the nine OAMD trim configurations; only the renderer knows
//! the target layout, so it selects the record here. The calculations mirror
//! the reference implementation: configuration index, automatic/explicit trim,
//! front-back balance and per-object bypass.

use objectaudio_ir::{ProgramCalibration, TrimRecord, TrimRecordKind};

use crate::SpeakerLayout;

/// Front-back balance keeps a five percent wall band at each end unmapped.
const BALANCE_WALL: f64 = 0.05;
/// `bal3d_y` is coded on a 1/16 grid.
const BALANCE_STEPS: f64 = 16.0;

/// Where a speaker's trim gain comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimMode {
    /// The renderer's own layout position compensation.
    Automatic,
    /// Unity gain.
    Disabled,
    /// The centre/surround/height levels of the selected configuration.
    Explicit,
}

/// Render-ready speaker controls for one target layout.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerControls {
    pub warp_mode: u8,
    pub trim_mode: TrimMode,
    pub centre_db: f64,
    pub surround_db: f64,
    pub height_db: f64,
    pub listener_balance: f64,
    pub overhead_balance: f64,
    /// Per IR object index; missing entries mean "not bypassed".
    pub bypass: Vec<bool>,
}

impl SpeakerControls {
    /// Defaults used before a stream declares calibration: warp 0, automatic
    /// trim for the seven trimmed layouts and disabled trim elsewhere.
    pub fn default_for(layout: &SpeakerLayout) -> Self {
        Self {
            warp_mode: 0,
            trim_mode: if layout.carries_trim() {
                TrimMode::Automatic
            } else {
                TrimMode::Disabled
            },
            centre_db: 0.0,
            surround_db: 0.0,
            height_db: 0.0,
            listener_balance: 0.0,
            overhead_balance: 0.0,
            bypass: Vec::new(),
        }
    }

    pub fn is_bypassed(&self, object: usize) -> bool {
        self.bypass.get(object).copied().unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ControlError {
    #[error("unsupported speaker warp mode {0}")]
    UnsupportedWarpMode(u8),
    #[error("unsupported speaker global trim mode {0}")]
    UnsupportedTrimMode(u8),
    #[error("trim level is not finite")]
    NonFiniteTrimLevel,
    #[error("balance {0} is not finite or outside [-1, 1]")]
    InvalidBalance(f64),
}

/// Trim configuration a layout selects, from its surround and height counts:
///
/// ```text
/// cfg = kappa_f(surrounds) + 3 * kappa_h(heights)
/// kappa_f: 0 -> 0, 2..3 -> 1, otherwise 2
/// kappa_h: 0 -> 0, 2 -> 1, otherwise 2
/// ```
pub fn trim_config_index(layout: &SpeakerLayout) -> usize {
    fn floor_class(count: u32) -> usize {
        match count {
            0 => 0,
            2 | 3 => 1,
            _ => 2,
        }
    }
    fn height_class(count: u32) -> usize {
        match count {
            0 => 0,
            2 => 1,
            _ => 2,
        }
    }
    floor_class(layout.surround_channels()) + 3 * height_class(layout.height_channels())
}

/// Select the calibration record for `layout` and turn it into renderer
/// controls. A layout without trim semantics always ends up disabled.
pub fn controls_from_calibration(
    calibration: &ProgramCalibration,
    layout: &SpeakerLayout,
) -> Result<SpeakerControls, ControlError> {
    if calibration.warp_mode > 3 {
        return Err(ControlError::UnsupportedWarpMode(calibration.warp_mode));
    }
    let mut controls = SpeakerControls {
        warp_mode: calibration.warp_mode,
        trim_mode: TrimMode::Automatic,
        centre_db: 0.0,
        surround_db: 0.0,
        height_db: 0.0,
        listener_balance: 0.0,
        overhead_balance: 0.0,
        bypass: calibration.bypass.clone(),
    };
    if !layout.carries_trim() {
        controls.trim_mode = TrimMode::Disabled;
        return Ok(controls);
    }
    match calibration.global_trim_mode {
        0 => {}
        1 => controls.trim_mode = TrimMode::Disabled,
        2 => {
            let record = &calibration.configs[trim_config_index(layout)];
            match record.kind {
                TrimRecordKind::Absent | TrimRecordKind::Default => {
                    controls.trim_mode = TrimMode::Automatic;
                }
                TrimRecordKind::Disabled => controls.trim_mode = TrimMode::Disabled,
                TrimRecordKind::Explicit => {
                    apply_explicit(&mut controls, record)?;
                }
            }
        }
        mode => return Err(ControlError::UnsupportedTrimMode(mode)),
    }
    Ok(controls)
}

fn apply_explicit(controls: &mut SpeakerControls, record: &TrimRecord) -> Result<(), ControlError> {
    let levels = [record.centre_db(), record.surround_db(), record.height_db()];
    if levels.iter().any(|level| !level.is_finite()) {
        return Err(ControlError::NonFiniteTrimLevel);
    }
    let listener = record.listener_balance();
    let overhead = record.overhead_balance();
    for balance in [listener, overhead] {
        if !balance.is_finite() || !(-1.0..=1.0).contains(&balance) {
            return Err(ControlError::InvalidBalance(balance));
        }
    }
    controls.trim_mode = TrimMode::Explicit;
    controls.centre_db = levels[0];
    // Surround and height are attenuation-only; the centre level keeps its
    // sign.
    controls.surround_db = levels[1].min(0.0);
    controls.height_db = levels[2].min(0.0);
    controls.listener_balance = listener;
    controls.overhead_balance = overhead;
    Ok(())
}

/// Reparameterise a front-back coordinate in `[0, 1]` by `balance`: 0 is the
/// front wall and 1 the back wall, and the two wall bands pass through
/// unchanged.
pub(crate) fn front_back_balance(position: f64, balance: f64) -> f64 {
    // `nearbyint` in the reference uses round-half-to-even.
    let steps = (balance.abs() * BALANCE_STEPS).round_ties_even();
    if steps == 0.0 {
        return position;
    }
    let slope = steps / 18.0;
    if position <= BALANCE_WALL || position >= 1.0 - BALANCE_WALL {
        return position;
    }
    if position < 0.5 {
        if balance > 0.0 {
            (1.0 + slope) * position - BALANCE_WALL * slope
        } else {
            (1.0 - slope) * position + BALANCE_WALL * slope
        }
    } else if balance > 0.0 {
        (1.0 - slope) * (position - 0.5) + 0.5 + 0.45 * slope
    } else {
        (1.0 + slope) * (position - 0.5) + 0.5 - 0.45 * slope
    }
}

/// Blend the listener-plane and overhead balance by height: 0 at the listener
/// plane, 1 at the top layer.
pub(crate) fn apply_balance(position: f64, height: f64, listener: f64, overhead: f64) -> f64 {
    if listener == 0.0 && overhead == 0.0 {
        return position;
    }
    let low = front_back_balance(position, listener);
    let high = front_back_balance(position, overhead);
    (1.0 - height) * low + height * high
}

/// Position-dependent explicit trim. Surround fades in with Y and out towards
/// Z=1, height fades in with Z, and the centre trim is local to the front
/// centre region.
pub(crate) fn explicit_position_gain(controls: &SpeakerControls, x: f64, y: f64, z: f64) -> f64 {
    let surround = (2.0 * y).clamp(0.0, 1.0) * (1.0 - z);
    let centre = 1.0
        - (10.0
            * (z.abs() - BALANCE_WALL)
                .max(y - BALANCE_WALL)
                .max((x - 0.5).abs() - BALANCE_WALL)
                .max(0.0))
        .clamp(0.0, 1.0);
    let db = surround * controls.surround_db.min(0.0)
        + z * controls.height_db.min(0.0)
        + centre * controls.centre_db;
    10.0f64.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout_by_name;

    #[test]
    fn selects_the_configuration_per_layout() {
        for (name, expected) in [
            ("2.0", 0),
            ("3.1", 0),
            ("4.0", 1),
            ("5.1", 1),
            ("7.1", 2),
            ("5.1.2", 4),
            ("5.1.4", 7),
        ] {
            let layout = layout_by_name(name).unwrap();
            assert_eq!(trim_config_index(layout), expected, "{name}");
        }
    }

    #[test]
    fn defaults_follow_the_layout_trim_semantics() {
        let automatic = SpeakerControls::default_for(layout_by_name("5.1").unwrap());
        assert_eq!(automatic.trim_mode, TrimMode::Automatic);
        assert_eq!(automatic.warp_mode, 0);
        let disabled = SpeakerControls::default_for(layout_by_name("7.1.4").unwrap());
        assert_eq!(disabled.trim_mode, TrimMode::Disabled);
    }

    #[test]
    fn null_calibration_is_automatic_for_trimmed_layouts() {
        let calibration = ProgramCalibration::default();
        let controls =
            controls_from_calibration(&calibration, layout_by_name("5.1").unwrap()).unwrap();
        assert_eq!(controls.trim_mode, TrimMode::Automatic);
        let controls =
            controls_from_calibration(&calibration, layout_by_name("9.1.6").unwrap()).unwrap();
        assert_eq!(controls.trim_mode, TrimMode::Disabled);
    }

    #[test]
    fn selects_default_disabled_and_explicit_records() {
        let mut calibration = ProgramCalibration {
            global_trim_mode: 2,
            ..ProgramCalibration::default()
        };
        let layout = layout_by_name("5.1").unwrap();

        calibration.configs[1] = TrimRecord::default_trim();
        let automatic = controls_from_calibration(&calibration, layout).unwrap();
        assert_eq!(automatic.trim_mode, TrimMode::Automatic);

        calibration.configs[1] = TrimRecord::disabled();
        let disabled = controls_from_calibration(&calibration, layout).unwrap();
        assert_eq!(disabled.trim_mode, TrimMode::Disabled);

        calibration.configs[1] =
            TrimRecord::explicit(Some(1.5), Some(3.0), Some(-6.0), Some(-0.5), Some(0.25));
        let explicit = controls_from_calibration(&calibration, layout).unwrap();
        assert_eq!(explicit.trim_mode, TrimMode::Explicit);
        assert_eq!(explicit.centre_db, 1.5);
        assert_eq!(explicit.surround_db, 0.0);
        assert_eq!(explicit.height_db, -6.0);
        assert_eq!(explicit.listener_balance, -0.5);
        assert_eq!(explicit.overhead_balance, 0.25);
    }

    #[test]
    fn rejects_reserved_and_invalid_values() {
        let layout = layout_by_name("5.1").unwrap();
        let mut calibration = ProgramCalibration {
            warp_mode: 4,
            ..ProgramCalibration::default()
        };
        assert_eq!(
            controls_from_calibration(&calibration, layout),
            Err(ControlError::UnsupportedWarpMode(4))
        );
        calibration.warp_mode = 0;
        calibration.global_trim_mode = 3;
        assert_eq!(
            controls_from_calibration(&calibration, layout),
            Err(ControlError::UnsupportedTrimMode(3))
        );
    }

    #[test]
    fn balance_passes_the_wall_bands_through() {
        for balance in [-1.0, -0.5, 0.0, 0.5, 1.0] {
            assert_eq!(front_back_balance(0.05, balance), 0.05);
            assert_eq!(front_back_balance(0.95, balance), 0.95);
            assert_eq!(front_back_balance(0.01, balance), 0.01);
        }
    }
}
