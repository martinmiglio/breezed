//! End-to-end CLI parity tests against a fake ipmitool.
#![allow(clippy::assert_is_empty)]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use common::{PASSWORD, World, event_names, json_lines};

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_matches_the_python_package() {
    let world = World::new(&["46"]);
    let output = world.run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "breezed 0.3.1\n");
}

#[test]
fn help_lists_the_documented_surface() {
    let world = World::new(&["46"]);
    let mut combined = String::new();
    for args in [
        vec!["--help"],
        vec!["run", "--help"],
        vec!["set", "--help"],
        vec!["auto", "--help"],
        vec!["status", "--help"],
        vec!["validate", "--help"],
    ] {
        let output = world.run(&args);
        assert!(output.status.success(), "{args:?}");
        combined.push_str(&stdout(&output));
    }
    for name in ["run", "set", "auto", "status", "validate"] {
        assert!(combined.contains(name), "missing {name}");
    }
    for flag in ["--config", "-c", "--metrics-port", "--verbose", "--probe"] {
        assert!(combined.contains(flag), "missing {flag}");
    }
}

#[test]
fn set_issues_manual_then_speed_commands_and_prints_json() {
    let world = World::new(&["46"]);
    let output = world
        .command()
        .env("IDRAC_PASSWORD", PASSWORD)
        .args(["set", "40"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({"event": "speed_change", "fan_pct": 40})
    );
    assert_eq!(
        world.calls(),
        [
            "-I lanplus -H 169.254.0.1 -U root -E raw 0x30 0x30 0x01 0x00",
            "-I lanplus -H 169.254.0.1 -U root -E raw 0x30 0x30 0x02 0xff 0x28",
        ]
    );
    let env = world.env_log();
    assert!(env.contains(&format!("IPMI_PASSWORD={PASSWORD}")));
    assert!(!env.contains("IDRAC_PASSWORD_LEAKED"));
    assert!(!world.calls().iter().any(|call| call.contains(PASSWORD)));
}

#[test]
fn auto_enables_auto_mode() {
    let world = World::new(&["46"]);
    let output = world.run(&["auto"]);
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({"event": "mode_change", "to": "auto"})
    );
    assert_eq!(
        world.calls(),
        ["-I lanplus -H 169.254.0.1 -U root -E raw 0x30 0x30 0x01 0x01"]
    );
}

#[test]
fn status_outputs_the_documented_json_schema() {
    let world = World::new(&["63"]);
    let output = world.run(&["status"]);
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({
            "temp_c": 63,
            "fan_rpms": [["FAN_1", 4320], ["FAN_2", 4320], ["FAN_3", 4080], ["FAN_4", 0]],
            "target_pct": 10,
        })
    );
}

#[test]
fn status_above_curve_reports_null_target() {
    let world = World::new(&["90"]);
    let output = world.run(&["status"]);
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["temp_c"], 90);
    assert!(payload["target_pct"].is_null());
}

#[test]
fn status_verbose_is_not_json() {
    let world = World::new(&["63"]);
    let output = world.run(&["status", "--verbose"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(serde_json::from_str::<serde_json::Value>(&text).is_err());
    assert!(text.contains("CPU max: 63C"));
    assert!(text.contains("FAN_1: 4320 RPM"));
    assert!(text.contains("Curve target: 10% (manual)"));
}

#[test]
fn status_config_error_exits_2() {
    let world = World::new(&["46"]);
    let output = world.run(&["status", "--config", "missing.toml"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("missing.toml"));
    assert!(world.calls().is_empty());
}

#[test]
fn status_ipmi_error_exits_1_without_leaking_the_password() {
    let world = World::new(&["ERR"]);
    let output = world
        .command()
        .env("IDRAC_PASSWORD", PASSWORD)
        .args(["status"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(text.contains("rc=1"));
    assert!(text.contains("unable to establish IPMI v2"));
    assert!(!text.contains(PASSWORD));
    assert!(!stdout(&output).contains(PASSWORD));
}

#[test]
fn empty_sdr_output_is_a_runtime_error() {
    let world = World::new(&["EMPTY"]);
    let output = world.run(&["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("empty output"));
}

#[test]
fn set_out_of_range_exits_2_without_touching_fans() {
    let world = World::new(&["46"]);
    for pct in ["0", "101"] {
        let output = world.run(&["set", pct]);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("1..100"));
    }
    assert!(world.calls().is_empty());
}

#[test]
fn set_negative_is_a_usage_error() {
    let world = World::new(&["46"]);
    let output = world.run(&["set", "-5"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(world.calls().is_empty());
}

#[test]
fn validate_valid_config_prints_summary() {
    let world = World::new(&["46"]);
    let output = world.run(&["validate", "breezed.toml"]);
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({
            "valid": true,
            "path": "breezed.toml",
            "host": "169.254.0.1",
            "poll_interval_s": 1,
            "curve_points": 4,
            "metrics_port": null,
        })
    );
}

#[test]
fn validate_invalid_config_prints_valid_false_and_exits_2() {
    let world = World::new(&["46"]);
    world.write_file("broken.toml", "[settings]\nhost = ");
    let output = world.run(&["validate", "broken.toml"]);
    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["valid"], false);
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("invalid TOML in broken.toml")
    );
    assert!(stderr(&output).contains("invalid TOML"));
}

#[test]
fn validate_reports_missing_identity() {
    let world = World::new(&["46"]);
    world.write_file("noident.toml", "[settings]\npoll_interval_s = 5\n");
    let output = world.run(&["validate", "noident.toml"]);
    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        payload["error"],
        "host: missing (set [settings].host or IDRAC_HOST); \
         user: missing (set [settings].user or IDRAC_USER)"
    );
}

#[test]
fn validate_rejects_non_string_identity_fields() {
    for field in ["password", "ipmitool_path"] {
        let world = World::new(&["46"]);
        world.write_file(
            "wrong.toml",
            &format!("[settings]\nhost = \"h\"\nuser = \"u\"\n{field} = 42\n"),
        );
        let output = world.run(&["validate", "wrong.toml"]);
        assert_eq!(output.status.code(), Some(2), "{field}");
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(payload["valid"], false);
        assert!(
            payload["error"]
                .as_str()
                .unwrap()
                .contains(&format!("{field}: expected a string")),
            "{payload}"
        );
    }
}

#[test]
fn validate_rejects_an_out_of_range_metrics_port() {
    let world = World::new(&["46"]);
    world.write_file(
        "bigport.toml",
        "[settings]\nhost = \"h\"\nuser = \"u\"\nmetrics_port = 70000\n",
    );
    let output = world.run(&["validate", "bigport.toml"]);
    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("metrics_port must be in 1..=65535")
    );
}

#[test]
fn validate_rejects_a_descending_curve() {
    let world = World::new(&["46"]);
    world.write_config(
        r#"
[settings]
host = "h"
user = "u"
ipmitool_path = "__IPMITOOL__"

[[curve]]
temp_c = 60
fan_pct = 5

[[curve]]
temp_c = 50
fan_pct = 6
"#,
    );
    let output = world.run(&["validate", "breezed.toml"]);
    assert_eq!(output.status.code(), Some(2));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("strictly ascending")
    );
}

#[test]
fn validate_probe_reports_the_live_target() {
    let world = World::new(&["52"]);
    let output = world.run(&["validate", "breezed.toml", "--probe"]);
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        payload["probe"],
        serde_json::json!({"ok": true, "temp_c": 52, "target_pct": 7, "would_auto": false})
    );
    assert!(world.calls().iter().all(|call| !call.contains("raw")));
}

#[test]
fn validate_probe_above_curve_flags_auto() {
    let world = World::new(&["90"]);
    let output = world.run(&["validate", "breezed.toml", "--probe"]);
    assert!(output.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["probe"]["target_pct"], serde_json::Value::Null);
    assert_eq!(payload["probe"]["would_auto"], true);
}

#[test]
fn validate_probe_failure_prints_summary_then_exits_1() {
    let world = World::new(&["ERR"]);
    let output = world.run(&["validate", "breezed.toml", "--probe"]);
    assert_eq!(output.status.code(), Some(1));
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["probe"]["ok"], false);
    assert!(stderr(&output).contains("ipmitool sdr type temperature failed"));
}

#[test]
fn run_serves_metrics_and_shuts_down_gracefully() {
    let world = World::new(&["50", "50", "50", "50", "50"]);
    let port = free_port();
    let child = world
        .command()
        .args([
            "run",
            "--config",
            "breezed.toml",
            "--metrics-port",
            &port.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn run");

    sleep(Duration::from_millis(1300));
    let response = http_get(port, "/metrics");
    assert!(response.starts_with("HTTP/1.0 200 OK"), "{response}");
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
    assert!(
        body.contains("breezed_temp_c{sensor=\"cpu_max\"} 50"),
        "{body}"
    );
    assert!(body.contains("breezed_fan_percent 7"), "{body}");
    assert!(body.contains("breezed_mode{mode=\"manual\"} 1"), "{body}");
    assert!(body.contains("breezed_polls_total 1"), "{body}");

    let output = terminate(port, child);
    let records = json_lines(&output.stdout);
    let names = event_names(&records);
    assert_eq!(names.first().map(String::as_str), Some("startup"));
    assert!(names.iter().any(|name| name == "poll"));
    assert_eq!(names.last().map(String::as_str), Some("shutdown"));
    assert!(names.iter().any(|name| name == "mode_change"));
    let commands = world.calls();
    assert!(commands.iter().any(|call| call.ends_with("0x01 0x00")));
    assert!(commands.iter().any(|call| call.ends_with("0x02 0xff 0x07")));
    assert!(commands.iter().any(|call| call.ends_with("0x01 0x01")));
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind free port");
    listener.local_addr().unwrap().port()
}

fn http_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect metrics");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn terminate(port: u16, child: std::process::Child) -> std::process::Output {
    // Wake any pending accept loop before signalling, then send SIGTERM.
    let _ = TcpStream::connect(("127.0.0.1", port));
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success());
    child.wait_with_output().expect("wait for breezed")
}
