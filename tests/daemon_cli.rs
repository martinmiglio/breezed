//! End-to-end daemon deployment CLI tests with fake sudo/systemctl.

mod common;

use std::fs;

use common::{World, json_lines};

const FAKE_SUDO: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$SUDO_LOG"
"#;
const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
case "$1" in
  is-active) echo active ;;
  is-enabled) echo enabled ;;
  *) exit 1 ;;
esac
"#;

fn is_root() -> bool {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    status.lines().any(|line| {
        line.strip_prefix("Uid:").is_some_and(|rest| {
            rest.split_whitespace()
                .nth(2)
                .is_some_and(|value| value == "0")
        })
    })
}

struct FakeBin {
    dir: std::path::PathBuf,
}

impl FakeBin {
    fn new(world: &World) -> Self {
        let dir = world.path().join("bin");
        fs::create_dir_all(&dir).unwrap();
        let bin = Self { dir };
        bin.write("sudo", FAKE_SUDO);
        bin.write("systemctl", FAKE_SYSTEMCTL);
        bin
    }

    fn write(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join(name);
        fs::write(&path, body).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }

    fn path_value(&self) -> String {
        format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn install_dry_run_prints_the_binary_install_plan() {
    let world = World::new(&["46"]);
    let bin = FakeBin::new(&world);
    let output = world
        .command()
        .env("PATH", bin.path_value())
        .args(["daemon", "install", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(json_lines(&output.stdout).len(), 1);
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["event"], "install_planned");
    assert_eq!(
        payload["steps"],
        serde_json::json!([
            "ensure system group 'breezed'",
            "ensure system user 'breezed'",
            "install /usr/local/bin/breezed",
            "install /etc/systemd/system/breezed.service",
            "install /etc/breezed/breezed.toml (first run only)",
            "install /etc/breezed.env (first run only)",
            "systemctl daemon-reload",
            "systemctl enable breezed",
            "systemctl restart breezed",
        ])
    );
    assert!(stderr(&output).contains("The following privileged steps will run:"));
    assert!(stderr(&output).contains("install /usr/local/bin/breezed"));
}

#[test]
fn install_without_privileges_builds_the_fixed_sudo_argv() {
    if is_root() {
        return;
    }
    let world = World::new(&["46"]);
    let bin = FakeBin::new(&world);
    let sudo_log = world.path().join("sudo.log");
    let output = world
        .command()
        .env("PATH", bin.path_value())
        .env_remove("LOGNAME")
        .env("USER", "test-owner")
        .env("SUDO_LOG", &sudo_log)
        .args(["daemon", "install", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload, serde_json::json!({"event": "install_complete"}));
    let argv: Vec<String> = fs::read_to_string(&sudo_log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        argv,
        [common::BIN, "daemon", "apply", "--owner", "test-owner",]
    );
}

#[test]
fn uninstall_without_privileges_builds_the_fixed_sudo_argv() {
    if is_root() {
        return;
    }
    let world = World::new(&["46"]);
    let bin = FakeBin::new(&world);
    let sudo_log = world.path().join("sudo.log");
    let output = world
        .command()
        .env("PATH", bin.path_value())
        .env("SUDO_LOG", &sudo_log)
        .args(["daemon", "uninstall", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({
            "event": "uninstall_complete",
            "keeps": ["/etc/breezed.env", "/etc/breezed"],
        })
    );
    let argv: Vec<String> = fs::read_to_string(&sudo_log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(argv, [common::BIN, "daemon", "remove"]);
}

#[test]
fn uninstall_dry_run_reports_retained_state() {
    let world = World::new(&["46"]);
    let bin = FakeBin::new(&world);
    let output = world
        .command()
        .env("PATH", bin.path_value())
        .args(["daemon", "uninstall", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["event"], "uninstall_planned");
    assert_eq!(
        payload["keeps"],
        serde_json::json!(["/etc/breezed.env", "/etc/breezed"])
    );
    assert_eq!(
        payload["steps"],
        serde_json::json!([
            "systemctl disable --now breezed",
            "remove /etc/systemd/system/breezed.service",
            "remove /usr/local/bin/breezed",
            "systemctl daemon-reload",
        ])
    );
    assert!(stderr(&output).contains("systemctl disable --now breezed"));
}

#[test]
fn daemon_status_prints_one_json_object() {
    let world = World::new(&["46"]);
    let bin = FakeBin::new(&world);
    let output = world
        .command()
        .env("PATH", bin.path_value())
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(payload["unit_present"].is_boolean());
    assert_eq!(payload["active"], true);
    assert_eq!(payload["enabled"], true);
    assert!(payload["unit_version"].is_null() || payload["unit_version"].is_string());
    assert_eq!(payload["binary_version"], "0.3.1");
}

#[test]
fn daemon_help_hides_internal_apply_and_remove() {
    let world = World::new(&["46"]);
    let output = world.run(&["daemon", "--help"]);
    assert!(output.status.success());
    let text = stdout(&output);
    for name in ["install", "status", "uninstall"] {
        assert!(text.contains(name));
    }
    for line in text.lines() {
        let trimmed = line.trim_start();
        assert!(!trimmed.starts_with("apply"));
        assert!(!trimmed.starts_with("remove"));
    }
}

#[test]
fn apply_requires_root() {
    if is_root() {
        return;
    }
    let world = World::new(&["46"]);
    let output = world
        .command()
        .args(["daemon", "apply", "--owner", "martin"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("must run as root"));
}

#[test]
fn apply_exposes_owner() {
    let world = World::new(&["46"]);
    let output = world.run(&["daemon", "apply", "--help"]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("--owner"));
}
