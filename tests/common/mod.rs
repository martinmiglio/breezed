//! Shared integration-test harness: a fake ipmitool and a temp config.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Secret used to prove the password never reaches argv or logs.
pub const PASSWORD: &str = "hunter2-supersecret";

/// Binary under test.
pub const BIN: &str = env!("CARGO_BIN_EXE_breezed");

const FAKE_IPMITOOL: &str = r#"#!/bin/sh
dir="$FAKE_IPMI_DIR"
mkdir -p "$dir"
printf '%s\n' "$*" >> "$dir/calls.log"
printf 'IPMI_PASSWORD=%s\n' "${IPMI_PASSWORD-unset}" >> "$dir/env.log"
if [ -n "${IDRAC_PASSWORD-}" ]; then
  printf 'IDRAC_PASSWORD_LEAKED\n' >> "$dir/env.log"
fi
# skip: -I lanplus -H host -U user -E
shift 7
case "$*" in
  "sdr type temperature")
    if [ -f "$dir/temp_error" ]; then
      cat "$dir/temp_error" >&2
      exit 1
    fi
    index=$(cat "$dir/temp_index" 2>/dev/null || echo 1)
    printf '%s' "$((index + 1))" > "$dir/temp_index"
    temp=$(sed -n "${index}p" "$dir/temps")
    if [ -z "$temp" ]; then temp=46; fi
    if [ "$temp" = "ERR" ]; then
      echo "unable to establish IPMI v2 / RMCP+ session" >&2
      exit 1
    fi
    if [ "$temp" = "EMPTY" ]; then exit 0; fi
    printf 'Inlet Temp       | 08h | ok  |  7.1 | 18 degrees C\n'
    printf 'CPU1 Temp        | 0Fh | ok  |  3.1 | %s degrees C\n' "$temp"
    ;;
  "sdr type fan")
    printf 'Fan Redundancy   | 39h | ok  |  7.1 | Fully Redundant\n'
    printf 'FAN_1            | 30h | ok  |  7.2 | 4320 RPM\n'
    printf 'FAN_2            | 31h | ok  |  7.2 | 4320 RPM\n'
    printf 'FAN_3            | 32h | ok  |  7.2 | 4080 RPM\n'
    printf 'FAN_4            | 33h | cr  |  7.2 | 0 RPM\n'
    printf 'FAN_6            | 35h | ns  |  7.2 | No Reading\n'
    ;;
  raw*)
    if [ -f "$dir/raw_rc" ]; then
      rc=$(cat "$dir/raw_rc")
      if [ "$rc" != "0" ]; then
        echo "raw command failed: $*" >&2
        exit "$rc"
      fi
    fi
    ;;
  *)
    echo "unexpected args: $*" >&2
    exit 99
    ;;
esac
"#;

/// One isolated test world.
pub struct World {
    dir: TempDir,
}

impl World {
    /// Create the fake ipmitool and a valid config.
    pub fn new(temps: &[&str]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let world = Self { dir };
        world.write_executable("ipmitool", FAKE_IPMITOOL);
        world.set_temps(temps);
        world.write_config(CONFIG);
        world
    }

    /// The world's directory.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Path of a file inside the world.
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Write a UTF-8 file, returning its path.
    pub fn write_file(&self, name: &str, body: &str) -> PathBuf {
        let path = self.file(name);
        fs::write(&path, body).expect("write file");
        path
    }

    /// Write an executable text file.
    pub fn write_executable(&self, name: &str, body: &str) -> PathBuf {
        let path = self.write_file(name, body);
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod");
        path
    }

    /// Replace the scripted temperature lines.
    pub fn set_temps(&self, temps: &[&str]) {
        self.write_file("temps", &format!("{}\n", temps.join("\n")));
        let _ = fs::remove_file(self.file("temp_index"));
    }

    /// Write `breezed.toml` with the fake tool path substituted.
    pub fn write_config(&self, body: &str) {
        let body = body.replace("__IPMITOOL__", &self.file("ipmitool").display().to_string());
        self.write_file("breezed.toml", &body);
    }

    /// Command for the binary under test, rooted in the world directory.
    pub fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .current_dir(self.dir.path())
            .env("FAKE_IPMI_DIR", self.dir.path())
            .env_remove("IDRAC_HOST")
            .env_remove("IDRAC_USER")
            .env_remove("IDRAC_PASSWORD")
            .env_remove("BREEZED_NATIVE_IPMI");
        command
    }

    /// Run the binary and capture output.
    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run breezed")
    }

    /// Recorded ipmitool argv lines.
    pub fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.file("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Recorded child environment lines.
    pub fn env_log(&self) -> String {
        fs::read_to_string(self.file("env.log")).unwrap_or_default()
    }
}

/// A valid config with a 1-second poll interval.
pub const CONFIG: &str = r#"
[settings]
host = "169.254.0.1"
user = "root"
ipmitool_path = "__IPMITOOL__"
poll_interval_s = 1

[[curve]]
temp_c = 45
fan_pct = 6

[[curve]]
temp_c = 60
fan_pct = 8

[[curve]]
temp_c = 68
fan_pct = 12

[[curve]]
temp_c = 74
fan_pct = 18
"#;

/// Parse stdout lines as JSON objects, ignoring non-JSON noise.
pub fn json_lines(output: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Captured stdout as UTF-8 (lossy).
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Captured stderr as UTF-8 (lossy).
pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Event names from parsed JSON lines.
pub fn event_names(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| record["event"].as_str().map(str::to_owned))
        .collect()
}
