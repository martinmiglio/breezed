# Migrating from the Python package

`breezed` was previously shipped as a Python 3.13 package. This repository now
contains only the Rust implementation. This note covers what carried over for
operators upgrading from the Python 0.3.1 release.

## What stays the same

- Command names, flags, defaults, and exit codes (`0` ok, `1` runtime/IPMI,
  `2` usage/config).
- JSON field names and value shapes for `set`, `auto`, `status`, `validate`, and
  `daemon` commands.
- Log event names and field names (`startup`, `poll`, `mode_change`,
  `speed_change`, `hysteresis_wait`, `config_reload`, `config_error`,
  `ipmi_error`, `shutdown`).
- Config keys, defaults, and `IDRAC_HOST` / `IDRAC_USER` / `IDRAC_PASSWORD`
  environment precedence.
- IPMI argv shape and password handling (`-E` plus `IPMI_PASSWORD`; the password
  is never on argv or in logs).
- Prometheus metric names and body.
- Service name, unit path, config paths, and first-run-only config installation.

## Upgrade procedure

1. Download the release tarball and verify its checksum and attestation.
2. Run `sudo ./breezed daemon install`.
3. Confirm `sudo breezed daemon status` reports the new version and the service
   is active.

Existing `/etc/breezed/breezed.toml` and `/etc/breezed.env` are preserved. The
installer copies the running binary to `/usr/local/bin/breezed`; it no longer
uses `uv` or a Python runtime.

## Intentional differences

- JSON is produced by `serde_json`. Field names and shapes are stable; spacing,
  escaping, and key order are not byte-identical to Python's `json.dumps`.
- Validation reports native Rust field errors and may list several at once
  instead of one pydantic error.
- Config `metrics_port` must be in `1..=65535` and is rejected at load time.
  `--metrics-port` is parsed as a `u16`.
- Curve temperatures outside `i32` are rejected.
- A missing `ipmitool` binary is a retryable `IpmiError` that fails safe to AUTO
  rather than an uncaught exception. `validate --probe` therefore emits a JSON
  summary where the Python command emitted a traceback.
- TOML syntax errors keep the `config: invalid TOML in PATH:` prefix and exit
  code `2`, but the detail comes from the Rust parser. Source snippets that
  could echo a password are stripped.
- Help text comes from clap, and verbose log lines are unindented.
- Fatal `traceback` fields contain a Rust backtrace.
- The metrics server sends only `Content-Type` and `Content-Length`.
- `ipmitool` runs in its own process group, and `daemon install` prefers
  `SUDO_USER` when resolving the config owner.

## Verification

The Rust implementation was checked against the Python 0.3.1 binary with a fake
`ipmitool`: exit codes, JSON shapes/values, event field sets, config precedence,
and IPMI argv/env matched on every documented happy path. It was also validated
against a live iDRAC and installed as a systemd service on the reference host.
