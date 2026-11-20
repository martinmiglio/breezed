//! TOML config loader: structural adapter only.
//!
//! Reads the file, merges environment overrides, and hands plain values to the
//! domain [`Settings`] validator. Every business rule lives in `domain`; this
//! module only handles key presence, TOML type shape, env/file precedence, and
//! field-naming errors.

use std::fs;
use std::path::Path;

use thiserror::Error;
use toml::{Table, Value};

use crate::domain::settings::{CurveRowInput, Settings, SettingsInput};

/// Field-naming failure; also wraps TOML decode, OS, and validation errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    /// Build a config error from a field-naming message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Environment lookup seam, injectable so tests never mutate process globals.
pub trait Environment: Send + Sync {
    /// Read one variable; `None` when unset.
    fn var(&self, key: &str) -> Option<String>;
}

/// Production environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnvironment;

impl Environment for SystemEnvironment {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

const ENV_HOST: &str = "IDRAC_HOST";
const ENV_USER: &str = "IDRAC_USER";
const ENV_PASSWORD: &str = "IDRAC_PASSWORD";

/// Read one override; an empty env value counts as unset, same as absent.
fn env_override(env: &dyn Environment, key: &str) -> Option<String> {
    env.var(key).filter(|value| !value.is_empty())
}

/// Load and validate settings from `path`.
///
/// # Errors
///
/// Returns [`ConfigError`] for unreadable files, invalid TOML, or validation
/// failures.
pub fn load_settings(path: impl AsRef<Path>) -> Result<Settings, ConfigError> {
    load_settings_with(path, &SystemEnvironment)
}

/// Load settings with an injected environment (used by tests).
///
/// # Errors
///
/// Returns [`ConfigError`] for unreadable files, invalid TOML, or validation
/// failures.
pub fn load_settings_with(
    path: impl AsRef<Path>,
    env: &dyn Environment,
) -> Result<Settings, ConfigError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path).map_err(|error| {
        ConfigError::new(format!("config: cannot read {}: {error}", path.display()))
    })?;
    let data: Table = content.parse().map_err(|error: toml::de::Error| {
        ConfigError::new(format!(
            "config: invalid TOML in {}: {}",
            path.display(),
            toml_error_summary(&error)
        ))
    })?;

    let settings_table = match data.get("settings") {
        None => Table::new(),
        Some(Value::Table(table)) => table.clone(),
        Some(_) => return Err(ConfigError::new("settings: expected a table")),
    };

    let mut input = SettingsInput {
        host: string_field(&settings_table, "host")?,
        user: string_field(&settings_table, "user")?,
        password: string_field(&settings_table, "password")?,
        curve: curve_input(&data)?,
        poll_interval_s: int_field(&settings_table, "poll_interval_s")?,
        read_failure_limit: int_field(&settings_table, "read_failure_limit")?,
        step_down_hysteresis_s: int_field(&settings_table, "step_down_hysteresis_s")?,
        metrics_port: int_field(&settings_table, "metrics_port")?,
        ipmitool_path: string_field(&settings_table, "ipmitool_path")?,
    };

    input.host = env_override(env, ENV_HOST).or(input.host);
    input.user = env_override(env, ENV_USER).or(input.user);
    input.password = env_override(env, ENV_PASSWORD).or(input.password);

    Settings::from_input(input).map_err(|errors| {
        ConfigError::new(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })
}

/// Keep the location and reason from a TOML error, dropping the source snippet.
///
/// The parser's pretty-printed error includes the offending config line, which
/// could echo a password. Field-name-only errors are the project rule, so we
/// keep only the content-free header and reason lines.
fn toml_error_summary(error: &toml::de::Error) -> String {
    let text = error.to_string();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let Some(first) = lines.first().copied() else {
        return "invalid TOML".to_owned();
    };
    let last = lines
        .iter()
        .rev()
        .find(|line| !is_snippet_line(line))
        .copied()
        .unwrap_or(first);
    if last == first {
        return first.to_owned();
    }
    format!("{first}: {last}")
}

fn is_snippet_line(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with('|') || line.starts_with('^') {
        return true;
    }
    // `12 | password = "..."` — the parser's gutter plus source excerpt.
    line.split_once('|')
        .is_some_and(|(gutter, _)| !gutter.is_empty() && gutter.trim().parse::<usize>().is_ok())
}

fn string_field(table: &Table, key: &str) -> Result<Option<String>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ConfigError::new(format!("{key}: expected a string"))),
    }
}

fn int_field(table: &Table, key: &str) -> Result<Option<i64>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(ConfigError::new(format!("{key}: expected an integer"))),
    }
}

fn curve_input(data: &Table) -> Result<Option<Vec<CurveRowInput>>, ConfigError> {
    let Some(value) = data.get("curve") else {
        return Ok(None);
    };
    let Value::Array(rows) = value else {
        return Err(ConfigError::new("curve: expected an array of tables"));
    };
    let mut parsed = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let Value::Table(table) = row else {
            return Err(ConfigError::new(format!(
                "curve[{index}]: expected a table"
            )));
        };
        parsed.push(CurveRowInput {
            temp_c: row_int(table, index, "temp_c")?,
            fan_pct: row_int(table, index, "fan_pct")?,
        });
    }
    Ok(Some(parsed))
}

fn row_int(table: &Table, index: usize, key: &str) -> Result<i64, ConfigError> {
    match table.get(key) {
        None => Err(ConfigError::new(format!("curve[{index}].{key}: missing"))),
        Some(Value::Integer(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!(
            "curve[{index}].{key}: expected an integer"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::domain::curve::CurvePoint;
    use crate::domain::settings::default_curve;
    use crate::domain::types::{FanPercent, TempC};

    #[derive(Default)]
    struct FakeEnv(HashMap<String, String>);

    impl FakeEnv {
        fn with(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            )
        }
    }

    impl Environment for FakeEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    const IDENTITY: &str = "[settings]\nhost = \"h\"\nuser = \"u\"\n";

    #[test]
    fn full_valid_toml_loads_frozen_settings() {
        let settings = load_settings(fixture("config_valid.toml")).unwrap();
        let expected = vec![
            CurvePoint {
                temp_c: TempC(45),
                fan_pct: FanPercent::from_validated(6),
            },
            CurvePoint {
                temp_c: TempC(60),
                fan_pct: FanPercent::from_validated(8),
            },
            CurvePoint {
                temp_c: TempC(68),
                fan_pct: FanPercent::from_validated(12),
            },
            CurvePoint {
                temp_c: TempC(74),
                fan_pct: FanPercent::from_validated(18),
            },
        ];
        assert_eq!(settings.host(), "169.254.0.1");
        assert_eq!(settings.user(), "root");
        assert_eq!(settings.password().expose_secret(), "");
        assert_eq!(settings.curve().points(), expected);
        assert_eq!(settings.poll_interval_s(), 10);
        assert_eq!(settings.read_failure_limit(), 3);
        assert_eq!(settings.step_down_hysteresis_s(), 30);
        assert_eq!(settings.metrics_port(), None);
        assert_eq!(settings.ipmitool_path(), "/usr/bin/ipmitool");
    }

    #[test]
    fn missing_host_and_user_raises_naming_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "no_identity.toml",
            "[settings]\npoll_interval_s = 5\n",
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("host: missing"));
        assert!(error.contains("user: missing"));
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "with_identity.toml",
            "[settings]\nhost = \"file-host\"\nuser = \"file-user\"\npassword = \"file-pass\"\n",
        );
        let env = FakeEnv::with(&[
            ("IDRAC_HOST", "10.9.8.7"),
            ("IDRAC_USER", "envuser"),
            ("IDRAC_PASSWORD", "envpass"),
        ]);
        let settings = load_settings_with(&path, &env).unwrap();
        assert_eq!(settings.host(), "10.9.8.7");
        assert_eq!(settings.user(), "envuser");
        assert_eq!(settings.password().expose_secret(), "envpass");
    }

    #[test]
    fn empty_env_value_falls_back_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "with_identity.toml",
            "[settings]\nhost = \"file-host\"\nuser = \"file-user\"\n",
        );
        let env = FakeEnv::with(&[("IDRAC_HOST", ""), ("IDRAC_PASSWORD", "")]);
        let settings = load_settings_with(&path, &env).unwrap();
        assert_eq!(settings.host(), "file-host");
        assert_eq!(settings.password().expose_secret(), "");
    }

    #[test]
    fn empty_env_value_with_missing_file_key_raises() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "no_user.toml", "[settings]\nhost = \"h\"\n");
        let env = FakeEnv::with(&[("IDRAC_USER", "")]);
        let error = load_settings_with(&path, &env).unwrap_err().to_string();
        assert!(error.contains("IDRAC_USER"));
    }

    #[test]
    fn bool_rejected_for_positive_int_fields() {
        for field in [
            "poll_interval_s",
            "read_failure_limit",
            "step_down_hysteresis_s",
            "metrics_port",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let body = format!("{IDENTITY}{field} = true\n");
            let path = write(dir.path(), "bool_trap.toml", &body);
            let error = load_settings(&path).unwrap_err().to_string();
            assert!(
                error.contains(&format!("{field}: expected an integer")),
                "{error}"
            );
        }
    }

    #[test]
    fn non_string_identity_fields_are_rejected() {
        for field in ["password", "ipmitool_path"] {
            let dir = tempfile::tempdir().unwrap();
            let body = format!("{IDENTITY}{field} = 42\n");
            let path = write(dir.path(), "wrong_type.toml", &body);
            let error = load_settings(&path).unwrap_err().to_string();
            assert!(
                error.contains(&format!("{field}: expected a string")),
                "{error}"
            );
        }
    }

    #[test]
    fn non_string_password_does_not_echo_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "numeric_password.toml",
            &format!("{IDENTITY}password = 123456789\n"),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(!error.contains("123456789"), "{error}");
    }

    #[test]
    fn metrics_port_above_u16_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "big_port.toml",
            &format!("{IDENTITY}metrics_port = 70000\n"),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(
            error.contains("metrics_port must be in 1..=65535"),
            "{error}"
        );
    }

    #[test]
    fn non_ascending_curve_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "flat_curve.toml",
            &format!(
                "{IDENTITY}\n[[curve]]\ntemp_c = 40\nfan_pct = 5\n\n\
                 [[curve]]\ntemp_c = 70\nfan_pct = 50\n\n\
                 [[curve]]\ntemp_c = 70\nfan_pct = 55\n"
            ),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("strictly ascending"));
    }

    #[test]
    fn fan_pct_out_of_range_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "bad_pct.toml",
            &format!("{IDENTITY}\n[[curve]]\ntemp_c = 40\nfan_pct = 101\n"),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("curve[0].fan_pct"));
    }

    #[test]
    fn zero_or_negative_interval_rejected() {
        for field in [
            "poll_interval_s",
            "read_failure_limit",
            "step_down_hysteresis_s",
        ] {
            for value in [0, -3] {
                let dir = tempfile::tempdir().unwrap();
                let body = format!("{IDENTITY}{field} = {value}\n");
                let path = write(dir.path(), "bad_interval.toml", &body);
                let error = load_settings(&path).unwrap_err().to_string();
                assert!(error.contains(field));
            }
        }
    }

    #[test]
    fn omitted_curve_and_empty_curve_fall_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "no_curve.toml", IDENTITY);
        let empty = write(
            dir.path(),
            "empty_curve.toml",
            &format!("{IDENTITY}curve = []\n"),
        );
        assert_eq!(
            load_settings(&path).unwrap().curve().points(),
            default_curve().points()
        );
        assert_eq!(
            load_settings(&empty).unwrap().curve().points(),
            default_curve().points()
        );
    }

    #[test]
    fn settings_scoped_curve_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "nested_curve.toml",
            "[settings]\nhost = \"h\"\nuser = \"u\"\ncurve = []\n",
        );
        assert_eq!(
            load_settings(&path).unwrap().curve().points(),
            default_curve().points()
        );
    }

    #[test]
    fn unknown_keys_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "extra_keys.toml",
            "top_level_stray = \"ignored\"\n\
             [settings]\nhost = \"h\"\nuser = \"u\"\nsomeday_field = true\n\n\
             [[curve]]\ntemp_c = 40\nfan_pct = 5\nfuture_col = \"x\"\n",
        );
        let settings = load_settings(&path).unwrap();
        assert_eq!(settings.host(), "h");
        assert_eq!(
            settings.curve().points(),
            [CurvePoint {
                temp_c: TempC(40),
                fan_pct: FanPercent::from_validated(5),
            }]
        );
    }

    #[test]
    fn invalid_toml_and_missing_file_surface_as_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "broken.toml", "[settings]\nhost = ");
        let missing = dir.path().join("absent.toml");
        assert!(load_settings(&bad).is_err());
        let error = load_settings(&missing).unwrap_err().to_string();
        assert!(error.contains("cannot read"));
        assert!(error.contains("absent.toml"));
    }

    #[test]
    fn toml_syntax_errors_never_echo_the_offending_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "leaky.toml",
            "[settings]\nhost = \"h\"\nuser = \"u\"\npassword = \"hunter2-supersecret\n",
        );
        let message = load_settings(&path).unwrap_err().to_string();
        assert!(!message.contains("hunter2-supersecret"), "{message}");
        assert!(message.contains("invalid TOML"));
        assert!(message.contains("line 4"));
    }

    #[test]
    fn snippet_detection_rejects_parser_gutters() {
        assert!(is_snippet_line("4 | password = \"secret\""));
        assert!(is_snippet_line("  |            ^"));
        assert!(is_snippet_line("|"));
        assert!(!is_snippet_line("invalid basic string, expected `\"`"));
        assert!(!is_snippet_line("TOML parse error at line 4, column 32"));
    }

    #[test]
    fn empty_ipmitool_path_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "empty_tool_path.toml",
            &format!("{IDENTITY}ipmitool_path = \"\"\n"),
        );
        assert_eq!(
            load_settings(&path).unwrap().ipmitool_path(),
            "/usr/bin/ipmitool"
        );
    }

    #[test]
    fn curve_type_shape_errors_are_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "shape.toml",
            &format!("curve = \"nope\"\n{IDENTITY}"),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("expected an array of tables"), "{error}");

        let path = write(
            dir.path(),
            "shape.toml",
            &format!("curve = [1]\n{IDENTITY}"),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("curve[0]: expected a table"), "{error}");
    }

    #[test]
    fn row_type_errors_name_the_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "bad_row.toml",
            &format!("{IDENTITY}\n[[curve]]\ntemp_c = \"hot\"\nfan_pct = 5\n"),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("curve[0].temp_c: expected an integer"));
    }

    #[test]
    fn missing_row_cell_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "missing_cell.toml",
            &format!("{IDENTITY}\n[[curve]]\ntemp_c = 40\n"),
        );
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("curve[0].fan_pct: missing"));
    }

    #[test]
    fn non_table_settings_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "bad_settings.toml", "settings = 5\n");
        let error = load_settings(&path).unwrap_err().to_string();
        assert!(error.contains("settings: expected a table"));
    }
}
