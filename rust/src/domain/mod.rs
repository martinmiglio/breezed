//! Innermost layer: vocabulary, validation rules, and ports. No I/O.

pub mod curve;
pub mod events;
pub mod ports;
pub mod settings;
pub mod types;

#[cfg(test)]
pub mod test_support {
    //! Shared trusted fixtures for in-crate unit tests.

    use crate::domain::curve::{Curve, CurvePoint};
    use crate::domain::settings::Settings;
    use crate::domain::types::{FanPercent, TempC};

    /// Build one curve point.
    pub fn point(temp_c: i32, fan_pct: i32) -> CurvePoint {
        CurvePoint {
            temp_c: TempC(temp_c),
            fan_pct: FanPercent::from_validated(fan_pct),
        }
    }

    /// Build a curve from `(temp_c, fan_pct)` pairs.
    pub fn curve(points: &[(i32, i32)]) -> Curve {
        Curve::new(points.iter().map(|&(t, p)| point(t, p))).expect("valid test curve")
    }

    /// The shipped default curve.
    pub fn default_curve() -> Curve {
        super::settings::default_curve()
    }

    /// Valid settings carrying `curve` and otherwise-default fields.
    pub fn settings_with_curve(curve: Curve) -> Settings {
        Settings::for_tests(curve)
    }

    /// Valid settings with the shipped default curve.
    pub fn settings() -> Settings {
        settings_with_curve(default_curve())
    }
}
