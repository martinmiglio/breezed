//! Domain vocabulary: typed values, validated constructors, errors, and enums.
//!
//! All business-rule validation lives here so adapters hold zero rules of their
//! own. This module performs no I/O and imports nothing outside `domain`.

use std::fmt;

use thiserror::Error;

/// A temperature in whole degrees Celsius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TempC(pub i32);

/// A fan duty cycle percentage in `1..=100`.
///
/// The field is private so the only safe constructor is [`make_fan_pct`]; this
/// makes an out-of-range percentage unrepresentable outside the domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FanPercent(i32);

impl FanPercent {
    /// The validated percentage as an integer.
    pub const fn get(self) -> i32 {
        self.0
    }

    /// Trusted construction used by the shipped default curve and unit tests.
    pub(crate) const fn from_validated(value: i32) -> Self {
        debug_assert!(value >= 1 && value <= 100);
        Self(value)
    }
}

/// Shared base for expected, user-facing failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct DomainError {
    message: String,
}

impl DomainError {
    /// Build a domain error from a human-readable, field-naming message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Raised by the IPMI adapter on subprocess / protocol failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct IpmiError {
    message: String,
}

impl IpmiError {
    /// Build an IPMI transport error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The error text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Validated constructor for fan percentages; rejects values outside `1..=100`.
///
/// # Errors
///
/// Returns [`DomainError`] when `value` is out of range.
pub fn make_fan_pct(value: i64) -> Result<FanPercent, DomainError> {
    if !(1..=100).contains(&value) {
        return Err(DomainError::new(format!(
            "fan_pct must be in 1..100, got {value}"
        )));
    }
    let narrowed = i32::try_from(value).expect("1..=100 fits in i32");
    Ok(FanPercent(narrowed))
}

/// Interval-style setting validation; rejects values `<= 0`.
///
/// # Errors
///
/// Returns [`DomainError`] naming `name` when `value` is not positive.
pub fn make_positive_int(name: &str, value: i64) -> Result<i64, DomainError> {
    if value <= 0 {
        return Err(DomainError::new(format!("{name} must be > 0, got {value}")));
    }
    Ok(value)
}

/// Operating mode; members serialize to lowercase strings in logs and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OperatingMode {
    /// No successful decision yet.
    #[default]
    Unknown,
    /// The iDRAC owns fan control.
    Auto,
    /// breezed owns fan control.
    Manual,
}

impl OperatingMode {
    /// Lowercase wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

impl fmt::Display for OperatingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Closed log-event vocabulary; consumers can match exhaustively on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    /// The control loop started.
    Startup,
    /// A temperature poll completed.
    Poll,
    /// The operating mode changed.
    ModeChange,
    /// A manual fan speed was applied.
    SpeedChange,
    /// A downward step is waiting out the hysteresis window.
    HysteresisWait,
    /// A hot-reloaded config was accepted.
    ConfigReload,
    /// A config read or validation failed.
    ConfigError,
    /// An IPMI read or write failed.
    IpmiError,
    /// The control loop is stopping.
    Shutdown,
}

impl EventType {
    /// Every member, in declaration order.
    pub const ALL: [Self; 9] = [
        Self::Startup,
        Self::Poll,
        Self::ModeChange,
        Self::SpeedChange,
        Self::HysteresisWait,
        Self::ConfigReload,
        Self::ConfigError,
        Self::IpmiError,
        Self::Shutdown,
    ];

    /// Lowercase wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Poll => "poll",
            Self::ModeChange => "mode_change",
            Self::SpeedChange => "speed_change",
            Self::HysteresisWait => "hysteresis_wait",
            Self::ConfigReload => "config_reload",
            Self::ConfigError => "config_error",
            Self::IpmiError => "ipmi_error",
            Self::Shutdown => "shutdown",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a mode or speed decision was taken; part of the log vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The temperature is below the curve's top point.
    TempUnderCurve,
    /// The temperature reached or passed the curve's top point.
    TempAboveCurve,
    /// Consecutive sensor reads failed past the configured limit.
    ReadFailures,
    /// The process is stopping.
    Shutdown,
    /// The controller just entered manual mode.
    ModeEnter,
    /// The temperature rose and the target increased.
    TempRise,
    /// The step-down hysteresis window elapsed.
    HysteresisElapsed,
}

impl Reason {
    /// Lowercase wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TempUnderCurve => "temp_under_curve",
            Self::TempAboveCurve => "temp_above_curve",
            Self::ReadFailures => "read_failures",
            Self::Shutdown => "shutdown",
            Self::ModeEnter => "mode_enter",
            Self::TempRise => "temp_rise",
            Self::HysteresisElapsed => "hysteresis_elapsed",
        }
    }
}

/// IPMI write operation names used in error events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Re-enable iDRAC automatic control.
    EnableAuto,
    /// Disable iDRAC automatic control.
    DisableAuto,
    /// Apply a fixed manual duty cycle.
    SetManualPct,
}

impl Operation {
    /// Lowercase wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnableAuto => "enable_auto",
            Self::DisableAuto => "disable_auto",
            Self::SetManualPct => "set_manual_pct",
        }
    }
}

/// A secret string that prints as `**********` in every diagnostic format.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the secret. Callers must not log the result.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretStr('**********')")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("**********")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_pct_boundaries() {
        assert!(make_fan_pct(0).is_err());
        assert!(make_fan_pct(101).is_err());
        assert_eq!(make_fan_pct(1).map(FanPercent::get), Ok(1));
        assert_eq!(make_fan_pct(100).map(FanPercent::get), Ok(100));
    }

    #[test]
    fn fan_pct_messages_name_the_field() {
        assert_eq!(
            make_fan_pct(0).unwrap_err().to_string(),
            "fan_pct must be in 1..100, got 0"
        );
        assert_eq!(
            make_fan_pct(101).unwrap_err().to_string(),
            "fan_pct must be in 1..100, got 101"
        );
    }

    #[test]
    fn positive_int_rejects_nonpositive_naming_field() {
        assert!(
            make_positive_int("poll_interval_s", 0)
                .unwrap_err()
                .to_string()
                .contains("poll_interval_s")
        );
        assert_eq!(
            make_positive_int("step_down_hysteresis_s", -5)
                .unwrap_err()
                .to_string(),
            "step_down_hysteresis_s must be > 0, got -5"
        );
        assert_eq!(make_positive_int("read_failure_limit", 3), Ok(3));
    }

    #[test]
    fn event_vocabulary_is_stable() {
        let names: Vec<&str> = EventType::ALL.iter().map(|e| e.as_str()).collect();
        assert_eq!(
            names,
            [
                "startup",
                "poll",
                "mode_change",
                "speed_change",
                "hysteresis_wait",
                "config_reload",
                "config_error",
                "ipmi_error",
                "shutdown"
            ]
        );
    }

    #[test]
    fn operating_modes_serialize_lowercase() {
        assert_eq!(OperatingMode::Unknown.to_string(), "unknown");
        assert_eq!(OperatingMode::Auto.to_string(), "auto");
        assert_eq!(OperatingMode::Manual.to_string(), "manual");
    }

    #[test]
    fn secret_never_reaches_diagnostics() {
        let secret = SecretString::new("hunter2");
        assert_eq!(secret.to_string(), "**********");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert_eq!(secret.expose_secret(), "hunter2");
    }
}
