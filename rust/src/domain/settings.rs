//! Runtime configuration schema (domain model).
//!
//! Holds the validated [`Settings`] aggregate. The TOML source lives in
//! `adapters::config`; this module never touches files or environment. The
//! adapter maps raw TOML values into [`SettingsInput`], and every business rule
//! (identity presence, ranges, port bounds, ascending curve) is enforced here.

use std::fmt;

use crate::domain::curve::{Curve, CurvePoint};
use crate::domain::types::{
    DomainError, FanPercent, SecretString, TempC, make_fan_pct, make_positive_int,
};

/// Default ipmitool path used when the config omits one.
pub const DEFAULT_TOOL_PATH: &str = "/usr/bin/ipmitool";

const DEFAULT_CURVE: [(i32, i32); 4] = [(45, 6), (60, 8), (68, 12), (74, 18)];

/// The shipped default curve.
pub fn default_curve() -> Curve {
    Curve::new(DEFAULT_CURVE.iter().map(|&(temp_c, fan_pct)| CurvePoint {
        temp_c: TempC(temp_c),
        fan_pct: FanPercent::from_validated(fan_pct),
    }))
    .expect("the shipped default curve is valid")
}

/// One raw `[[curve]]` row, before validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveRowInput {
    /// Raw `temp_c` cell.
    pub temp_c: i64,
    /// Raw `fan_pct` cell.
    pub fan_pct: i64,
}

/// Raw settings gathered by the config adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsInput {
    /// `host` / `IDRAC_HOST`.
    pub host: Option<String>,
    /// `user` / `IDRAC_USER`.
    pub user: Option<String>,
    /// `password` / `IDRAC_PASSWORD`.
    pub password: Option<String>,
    /// Top-level `[[curve]]`; `None` or an empty list selects the default.
    pub curve: Option<Vec<CurveRowInput>>,
    /// Poll cadence in seconds.
    pub poll_interval_s: Option<i64>,
    /// Consecutive read failures before forcing AUTO.
    pub read_failure_limit: Option<i64>,
    /// Downward-step hysteresis window in seconds.
    pub step_down_hysteresis_s: Option<i64>,
    /// Loopback metrics port.
    pub metrics_port: Option<i64>,
    /// ipmitool executable path; empty falls back to the default.
    pub ipmitool_path: Option<String>,
}

impl SettingsInput {
    /// Start an input with the required identity fields set.
    pub fn with_identity(host: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            host: Some(host.into()),
            user: Some(user.into()),
            ..Self::default()
        }
    }
}

/// Validated runtime configuration; immutable so consumers can share it.
#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    host: String,
    user: String,
    password: SecretString,
    curve: Curve,
    poll_interval_s: i64,
    read_failure_limit: i64,
    step_down_hysteresis_s: i64,
    metrics_port: Option<u16>,
    ipmitool_path: String,
}

impl Settings {
    /// Validate raw input into a frozen aggregate.
    ///
    /// Identity problems are reported together; other field errors are
    /// collected so one run names every invalid setting.
    ///
    /// # Errors
    ///
    /// Returns the collected validation messages.
    pub fn from_input(input: SettingsInput) -> Result<Self, Vec<DomainError>> {
        let mut errors = Vec::new();

        let host = input.host.filter(|value| !value.is_empty());
        let user = input.user.filter(|value| !value.is_empty());
        let mut missing = Vec::new();
        for (name, env_key, value) in [("host", "IDRAC_HOST", &host), ("user", "IDRAC_USER", &user)]
        {
            if value.is_none() {
                missing.push(format!(
                    "{name}: missing (set [settings].{name} or {env_key})"
                ));
            }
        }
        if !missing.is_empty() {
            errors.push(DomainError::new(missing.join("; ")));
        }

        let poll_interval_s = parse_positive("poll_interval_s", input.poll_interval_s, &mut errors);
        let read_failure_limit =
            parse_positive("read_failure_limit", input.read_failure_limit, &mut errors);
        let step_down_hysteresis_s = parse_positive(
            "step_down_hysteresis_s",
            input.step_down_hysteresis_s,
            &mut errors,
        );
        let metrics_port = parse_port(input.metrics_port, &mut errors);

        let curve = match input.curve {
            None => default_curve(),
            Some(rows) if rows.is_empty() => default_curve(),
            Some(rows) => match parse_curve_rows(&rows) {
                Ok(curve) => curve,
                Err(error) => {
                    errors.push(error);
                    default_curve()
                }
            },
        };

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(Self {
            host: host.unwrap_or_default(),
            user: user.unwrap_or_default(),
            password: SecretString::new(input.password.unwrap_or_default()),
            curve,
            poll_interval_s: poll_interval_s.unwrap_or(10),
            read_failure_limit: read_failure_limit.unwrap_or(3),
            step_down_hysteresis_s: step_down_hysteresis_s.unwrap_or(30),
            metrics_port,
            ipmitool_path: input
                .ipmitool_path
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| DEFAULT_TOOL_PATH.to_owned()),
        })
    }

    /// iDRAC hostname or address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// iDRAC username.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// iDRAC password (masked by `Debug`/`Display`).
    pub fn password(&self) -> &SecretString {
        &self.password
    }

    /// Validated fan curve, strictly ascending.
    pub fn curve(&self) -> &Curve {
        &self.curve
    }

    /// Poll cadence in seconds.
    pub fn poll_interval_s(&self) -> i64 {
        self.poll_interval_s
    }

    /// Consecutive read failures before forcing AUTO.
    pub fn read_failure_limit(&self) -> i64 {
        self.read_failure_limit
    }

    /// Downward-step hysteresis window in seconds.
    pub fn step_down_hysteresis_s(&self) -> i64 {
        self.step_down_hysteresis_s
    }

    /// Loopback metrics port, if configured.
    pub fn metrics_port(&self) -> Option<u16> {
        self.metrics_port
    }

    /// ipmitool executable path.
    pub fn ipmitool_path(&self) -> &str {
        &self.ipmitool_path
    }

    /// Trusted construction for unit tests (no validation, defaults elsewhere).
    #[cfg(test)]
    pub fn for_tests(curve: Curve) -> Self {
        Self {
            host: "169.254.0.1".to_owned(),
            user: "root".to_owned(),
            password: SecretString::new(""),
            curve,
            poll_interval_s: 10,
            read_failure_limit: 3,
            step_down_hysteresis_s: 30,
            metrics_port: None,
            ipmitool_path: DEFAULT_TOOL_PATH.to_owned(),
        }
    }
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("host", &self.host)
            .field("user", &self.user)
            .field("password", &self.password)
            .field("curve", &self.curve)
            .field("poll_interval_s", &self.poll_interval_s)
            .field("read_failure_limit", &self.read_failure_limit)
            .field("step_down_hysteresis_s", &self.step_down_hysteresis_s)
            .field("metrics_port", &self.metrics_port)
            .field("ipmitool_path", &self.ipmitool_path)
            .finish()
    }
}

fn parse_positive(name: &str, raw: Option<i64>, errors: &mut Vec<DomainError>) -> Option<i64> {
    let value = raw?;
    match make_positive_int(name, value) {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

fn parse_port(raw: Option<i64>, errors: &mut Vec<DomainError>) -> Option<u16> {
    let value = raw?;
    if let Ok(port) = u16::try_from(value) {
        if port > 0 {
            return Some(port);
        }
    }
    errors.push(DomainError::new(format!(
        "metrics_port must be in 1..=65535, got {value}"
    )));
    None
}

fn parse_curve_rows(rows: &[CurveRowInput]) -> Result<Curve, DomainError> {
    let mut points = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let fan_pct = make_fan_pct(row.fan_pct)
            .map_err(|error| DomainError::new(format!("curve[{index}].{error}")))?;
        let temp_c = i32::try_from(row.temp_c).map_err(|_| {
            DomainError::new(format!(
                "curve[{index}].temp_c: expected an i32, got {}",
                row.temp_c
            ))
        })?;
        points.push(CurvePoint {
            temp_c: TempC(temp_c),
            fan_pct,
        });
    }
    Curve::new(points).map_err(|error| DomainError::new(format!("curve: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> SettingsInput {
        SettingsInput::with_identity("h", "u")
    }

    #[test]
    fn defaults_match_the_shipped_curve() {
        let settings = Settings::from_input(input()).unwrap();
        assert_eq!(settings.curve().points(), default_curve().points());
        assert_eq!(settings.poll_interval_s(), 10);
        assert_eq!(settings.read_failure_limit(), 3);
        assert_eq!(settings.step_down_hysteresis_s(), 30);
        assert_eq!(settings.metrics_port(), None);
        assert_eq!(settings.ipmitool_path(), DEFAULT_TOOL_PATH);
        assert_eq!(settings.password().expose_secret(), "");
    }

    #[test]
    fn missing_identity_is_one_combined_message() {
        let errors = Settings::from_input(SettingsInput::default()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].to_string(),
            "host: missing (set [settings].host or IDRAC_HOST); \
             user: missing (set [settings].user or IDRAC_USER)"
        );
    }

    #[test]
    fn empty_identity_values_count_as_missing() {
        let errors = Settings::from_input(SettingsInput::with_identity("", "u")).unwrap_err();
        assert_eq!(
            errors[0].to_string(),
            "host: missing (set [settings].host or IDRAC_HOST)"
        );
    }

    #[test]
    fn nonpositive_int_fields_are_rejected() {
        let mut raw = input();
        raw.metrics_port = Some(0);
        let errors = Settings::from_input(raw).unwrap_err();
        assert_eq!(
            errors[0].to_string(),
            "metrics_port must be in 1..=65535, got 0"
        );
    }

    #[test]
    fn metrics_port_above_u16_is_rejected() {
        let mut raw = input();
        raw.metrics_port = Some(70_000);
        let errors = Settings::from_input(raw).unwrap_err();
        assert_eq!(
            errors[0].to_string(),
            "metrics_port must be in 1..=65535, got 70000"
        );
    }

    #[test]
    fn metrics_port_is_narrowed() {
        let mut raw = input();
        raw.metrics_port = Some(9762);
        assert_eq!(
            Settings::from_input(raw).unwrap().metrics_port(),
            Some(9762)
        );
    }

    #[test]
    fn curve_errors_are_prefixed_with_curve() {
        let mut raw = input();
        raw.curve = Some(vec![
            CurveRowInput {
                temp_c: 60,
                fan_pct: 5,
            },
            CurveRowInput {
                temp_c: 50,
                fan_pct: 6,
            },
        ]);
        let errors = Settings::from_input(raw).unwrap_err();
        assert_eq!(
            errors[0].to_string(),
            "curve: curve temp_c must be strictly ascending: \
             points[0].temp_c=60 >= points[1].temp_c=50"
        );
    }

    #[test]
    fn fan_pct_row_error_names_the_row() {
        let mut raw = input();
        raw.curve = Some(vec![CurveRowInput {
            temp_c: 40,
            fan_pct: 101,
        }]);
        let errors = Settings::from_input(raw).unwrap_err();
        assert_eq!(
            errors[0].to_string(),
            "curve[0].fan_pct must be in 1..100, got 101"
        );
    }

    #[test]
    fn temperature_beyond_i32_is_rejected() {
        let mut raw = input();
        raw.curve = Some(vec![CurveRowInput {
            temp_c: i64::from(i32::MAX) + 1,
            fan_pct: 50,
        }]);
        let errors = Settings::from_input(raw).unwrap_err();
        assert!(errors[0].to_string().contains("curve[0].temp_c"));
    }

    #[test]
    fn descending_curve_is_rejected() {
        let mut raw = input();
        raw.curve = Some(vec![
            CurveRowInput {
                temp_c: 60,
                fan_pct: 5,
            },
            CurveRowInput {
                temp_c: 50,
                fan_pct: 6,
            },
        ]);
        let errors = Settings::from_input(raw).unwrap_err();
        assert_eq!(
            errors[0].to_string(),
            "curve: curve temp_c must be strictly ascending: \
             points[0].temp_c=60 >= points[1].temp_c=50"
        );
    }

    #[test]
    fn empty_curve_falls_back_to_default() {
        let mut raw = input();
        raw.curve = Some(Vec::new());
        assert_eq!(
            Settings::from_input(raw).unwrap().curve().points(),
            default_curve().points()
        );
    }

    #[test]
    fn settings_repr_masks_the_secret() {
        let mut raw = input();
        raw.password = Some("hunter2".to_owned());
        let settings = Settings::from_input(raw).unwrap();
        assert!(!format!("{settings:?}").contains("hunter2"));
        assert!(!settings.password().to_string().contains("hunter2"));
        assert_eq!(settings.password().expose_secret(), "hunter2");
    }

    #[test]
    fn empty_tool_path_falls_back() {
        let mut raw = input();
        raw.ipmitool_path = Some(String::new());
        assert_eq!(
            Settings::from_input(raw).unwrap().ipmitool_path(),
            DEFAULT_TOOL_PATH
        );
    }
}
