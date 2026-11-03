//! Programme-level speaker calibration carried alongside the object metadata.
//!
//! The model mirrors the OAMD trim element without depending on the OAMD wire
//! format: the renderer chooses the record for its target layout, so the
//! payload must carry all nine configurations. Layout-independent code (the
//! binaural renderer) can ignore it. DAMF retains the per-object bypass flags.

use serde::{Deserialize, Serialize};

use crate::IrError;

/// A discrete calibration update relative to the containing buffer's start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationEvent {
    pub offset: i64,
    pub calibration: ProgramCalibration,
}

/// Number of layout classes the OAMD trim element defines.
pub const TRIM_CONFIG_COUNT: usize = 9;

/// How one trim configuration is declared on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrimRecordKind {
    /// The element did not carry this configuration.
    #[default]
    Absent,
    /// Use the renderer's own automatic position compensation.
    Default,
    /// Use unity gain.
    Disabled,
    /// Use the explicit levels and balances below.
    Explicit,
}

/// One of the nine per-configuration trim records.
///
/// `None` keeps the wire-level "value not present" state, so the record can be
/// written back without inventing zeros. Rendering treats a missing value as
/// zero dB / no balance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TrimRecord {
    pub kind: TrimRecordKind,
    pub centre_db: Option<f64>,
    pub surround_db: Option<f64>,
    pub height_db: Option<f64>,
    pub listener_balance: Option<f64>,
    pub overhead_balance: Option<f64>,
}

impl TrimRecord {
    pub fn default_trim() -> Self {
        Self {
            kind: TrimRecordKind::Default,
            ..Self::default()
        }
    }

    pub fn disabled() -> Self {
        Self {
            kind: TrimRecordKind::Disabled,
            ..Self::default()
        }
    }

    pub fn explicit(
        centre_db: Option<f64>,
        surround_db: Option<f64>,
        height_db: Option<f64>,
        listener_balance: Option<f64>,
        overhead_balance: Option<f64>,
    ) -> Self {
        Self {
            kind: TrimRecordKind::Explicit,
            centre_db,
            surround_db,
            height_db,
            listener_balance,
            overhead_balance,
        }
    }

    pub fn centre_db(&self) -> f64 {
        self.centre_db.unwrap_or(0.0)
    }

    pub fn surround_db(&self) -> f64 {
        self.surround_db.unwrap_or(0.0)
    }

    pub fn height_db(&self) -> f64 {
        self.height_db.unwrap_or(0.0)
    }

    pub fn listener_balance(&self) -> f64 {
        self.listener_balance.unwrap_or(0.0)
    }

    pub fn overhead_balance(&self) -> f64 {
        self.overhead_balance.unwrap_or(0.0)
    }
}

/// The programme's OAMD trim element in layout-independent form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgramCalibration {
    /// 0 normal, 1 floor-Y scaling, 2 PLIIx surround fold, 3 Lo/Ro fold.
    pub warp_mode: u8,
    /// 0 automatic, 1 disabled, 2 per-configuration records.
    pub global_trim_mode: u8,
    pub configs: [TrimRecord; TRIM_CONFIG_COUNT],
    /// Per-object trim bypass, already aligned to IR object indices.
    pub bypass: Vec<bool>,
}

impl Default for ProgramCalibration {
    fn default() -> Self {
        Self {
            warp_mode: 0,
            global_trim_mode: 0,
            configs: [TrimRecord::default(); TRIM_CONFIG_COUNT],
            bypass: Vec::new(),
        }
    }
}

impl ProgramCalibration {
    pub fn validate(&self) -> Result<(), IrError> {
        if self.warp_mode > 3 {
            return Err(IrError::InvalidCalibration {
                property: "warp_mode",
            });
        }
        if self.global_trim_mode > 2 {
            return Err(IrError::InvalidCalibration {
                property: "global_trim_mode",
            });
        }
        for (index, record) in self.configs.iter().enumerate() {
            let levels = [
                ("centre_db", record.centre_db),
                ("surround_db", record.surround_db),
                ("height_db", record.height_db),
            ];
            for (property, value) in levels {
                if value.is_some_and(|value| !value.is_finite()) {
                    return Err(IrError::InvalidTrimRecord { index, property });
                }
            }
            let balances = [
                ("listener_balance", record.listener_balance),
                ("overhead_balance", record.overhead_balance),
            ];
            for (property, value) in balances {
                if value.is_some_and(|value| !value.is_finite() || !(-1.0..=1.0).contains(&value)) {
                    return Err(IrError::InvalidTrimRecord { index, property });
                }
            }
        }
        Ok(())
    }

    /// Bypass flag for an object index, defaulting to `false`.
    pub fn is_bypassed(&self, object: usize) -> bool {
        self.bypass.get(object).copied().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_warp_zero_automatic_and_empty_bypass() {
        let calibration = ProgramCalibration::default();
        assert_eq!(calibration.warp_mode, 0);
        assert_eq!(calibration.global_trim_mode, 0);
        assert!(
            calibration
                .configs
                .iter()
                .all(|r| r.kind == TrimRecordKind::Absent)
        );
        assert!(calibration.bypass.is_empty());
        calibration.validate().unwrap();
    }

    #[test]
    fn missing_values_render_as_zero() {
        let record = TrimRecord::explicit(Some(1.5), None, None, None, Some(-0.5));
        assert_eq!(record.centre_db(), 1.5);
        assert_eq!(record.surround_db(), 0.0);
        assert_eq!(record.height_db(), 0.0);
        assert_eq!(record.listener_balance(), 0.0);
        assert_eq!(record.overhead_balance(), -0.5);
    }

    #[test]
    fn rejects_out_of_range_controls() {
        let mut calibration = ProgramCalibration {
            warp_mode: 4,
            ..ProgramCalibration::default()
        };
        assert!(calibration.validate().is_err());
        calibration.warp_mode = 0;
        calibration.global_trim_mode = 3;
        assert!(calibration.validate().is_err());
        calibration.global_trim_mode = 0;
        calibration.configs[2] = TrimRecord::explicit(None, None, None, Some(1.5), None);
        assert!(calibration.validate().is_err());
        calibration.configs[2] = TrimRecord::explicit(Some(f64::NAN), None, None, None, None);
        assert!(calibration.validate().is_err());
    }

    #[test]
    fn bypass_lookup_defaults_false() {
        let calibration = ProgramCalibration {
            bypass: vec![false, true],
            ..ProgramCalibration::default()
        };
        assert!(!calibration.is_bypassed(0));
        assert!(calibration.is_bypassed(1));
        assert!(!calibration.is_bypassed(2));
    }
}
