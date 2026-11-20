# Python ↔ Rust compatibility contract

This file records what the Rust rewrite preserves from the Python 0.3.1
implementation and where the two intentionally differ. The port started as a
byte-for-byte exercise; it is now maintained for **contract compatibility**:
command names, JSON field names and shapes, event vocabulary, IPMI argv/env,
config keys, metric names, exit codes, paths, and systemd behaviour.

Whitespace, key order, and parser prose are no longer part of the contract.
Machine output is produced with `serde_json`; validation errors are native Rust
messages. Consumers should parse JSON, not compare bytes.

## CLI surface

| Command | Rust | Notes |
| --- | --- | --- |
| `--version` | ✅ `breezed 0.3.1` | same string |
| `run [-c PATH] [--metrics-port N] [-v]` | ✅ | same defaults, SIGINT/SIGTERM shutdown |
| `set PCT [-c PATH]` | ✅ | validates `1..=100` before loading config |
| `auto [-c PATH]` | ✅ | |
| `status [--config PATH] [--verbose]` | ✅ | same JSON fields; verbose text |
| `validate PATH [--probe]` | ✅ | same summary/`probe` shapes |
| `daemon install/status/uninstall` | ✅* | same JSON fields/exit codes; install backend is Rust-native (see below) |
| `daemon apply/remove` | ✅* | hidden; `apply --owner STR`, `remove` |
| exit codes | ✅ | `0` ok, `1` runtime/IPMI, `2` usage/config |

`*` The deployment backend intentionally differs: the Rust installer copies the
running binary and does not use uv or a Python runtime. See
[Daemon deployment](#daemon-deployment).

JSON objects carry the same field names and value shapes as the Python CLI.
Formatting (spacing, escaping, key order) is `serde_json`'s and is not asserted
byte-for-byte.

## IPMI contract

Every invocation is `ipmitool -I lanplus -H HOST -U USER -E <subcommand>`, with:

| Operation | Subcommand |
| --- | --- |
| read temperature | `sdr type temperature` |
| read fan RPMs | `sdr type fan` |
| automatic control | `raw 0x30 0x30 0x01 0x01` |
| manual control | `raw 0x30 0x30 0x01 0x00` |
| duty cycle | `raw 0x30 0x30 0x02 0xff 0xNN` (lowercase, `#04x`) |

- Password travels only in `IPMI_PASSWORD`; `IDRAC_PASSWORD` is stripped from
  the child environment and the password is never on argv.
- `ipmitool` runs in its own process group, so an interactive Ctrl-C stops the
  controller without aborting an in-flight read; the unit uses `KillMode=mixed`
  for the same reason under systemd.
- 15-second timeout; timeout messages name the subcommand args, not the full
  argv, and carry no cause chain.
- stderr is redacted before the first line (max 200 characters) is included in
  errors.
- Read commands treat empty stdout as failure; raw writes treat empty stdout as
  success.
- SDR temperature rows match `0Eh`/`0Fh`; fan rows require `<digits> RPM`.
- `read_fan_rpms` drops non-RPM rows (`No Reading`, `Fully Redundant`) but keeps
  `0 RPM`.

## Config contract

- `[settings]` holds host/user/password, the four integer settings, and
  `ipmitool_path`; top-level `[[curve]]` holds points.
- `IDRAC_HOST`, `IDRAC_USER`, `IDRAC_PASSWORD` override the file when non-empty.
- Missing identity is reported as one combined
  `host: missing (...); user: missing (...)` message.
- Every string field must be a TOML string. A non-string `password` or
  `ipmitool_path` is rejected with a field-only message, never echoing the
  value.
- Every integer field must be a TOML integer; booleans and other types are
  rejected by field name.
- `poll_interval_s`, `read_failure_limit`, and `step_down_hysteresis_s` must be
  `> 0`. `metrics_port` must be in `1..=65535` and is validated at config load,
  so an out-of-range value fails before startup.
- Omitted/empty curve falls back to the shipped four-point default.
- `curve` placed inside `[settings]` is ignored (the original always reads the
  top-level table) — preserved bug-for-bug.
- Unknown keys are ignored.
- Hot reload triggers on mtime change; a failed reload keeps the last good
  settings and emits `config_error`.
- Validation may report several field errors in one message; Python's pydantic
  validators stopped at the first.

## Logging / events

Nine closed event names: `startup`, `poll`, `mode_change`, `speed_change`,
`hysteresis_wait`, `config_reload`, `config_error`, `ipmi_error`, `shutdown`.

Field names are stable per event, e.g.:

```json
{"event":"poll","temp_c":63,"fan_pct":7,"mode":"manual","target_pct":10,"ts":"...","level":"INFO","logger":"breezed"}
{"event":"ipmi_error","operation":"enable_auto","reason":"read_failures","failures":1,"error":"...","ts":"...","level":"INFO","logger":"breezed"}
```

Optional fields are omitted rather than emitted as `null`. Errors go to stderr
as `breezed: …`; stdout stays machine-readable. `--verbose` switches to human
lines (local timestamp + level + event name) on stdout.

## Metrics

Body is byte-identical:

```
# HELP breezed_temp_c Current maximum CPU temperature in degrees Celsius.
...
breezed_temp_c{sensor="cpu_max"} 63
breezed_fan_percent 12
breezed_mode{mode="manual"} 1
breezed_ipmi_errors_total 0
breezed_polls_total 42
```

Served on `127.0.0.1` only, `/` and `/metrics` return 200, anything else 404,
bind failure logs a warning and degrades without stopping fan control.

## Daemon deployment

The Rust installer intentionally replaces the Python `uv` backend:

- ensures the `breezed` system group, then creates the user with
  `useradd --system --gid breezed …`;
- installs the running binary as `root:root 0755` at `/usr/local/bin/breezed`;
- refuses a config owner that does not exist, has no resolvable primary group,
  or does not match `SUDO_USER`;
- stages the unit/env/config in a root-private temp dir (no user-controlled
  staging path is accepted by the privileged command);
- unit `root:root 0644` with an `# Installed by breezed VERSION on …` stamp,
  config `OWNER:PRIMARY_GROUP 0664`, env `root:breezed 0640`;
- config/env installed only when absent;
- unconditional `systemctl restart breezed` on install;
- uninstall disables/removes the unit and binary, keeps `/etc/breezed.env`,
  `/etc/breezed/`, and the service user/group;
- one sudo re-exec with the absolute running binary and argv only (no shell).

The unit is stamped, so `daemon status` can report version drift instead of
`null`. The unit is hardened for a Rust daemon (`Type=exec`,
`MemoryHigh=16M`/`MemoryMax=32M`, `TasksMax=16`, `MemoryDenyWriteExecute`,
`SystemCallFilter=@system-service`, `ProtectProc=invisible`, W^X, etc.) and no
longer carries the Python-only `MALLOC_ARENA_MAX` knob.

## Intentional differences

- **Validation prose and error count.** Rust reports field-only messages and may
  collect several at once; Python reported one pydantic `Value error, …` at a
  time.
- **`temp_c` range.** Rust stores temperatures as `i32` and rejects curve points
  outside that range. Python accepted any integer.
- **Missing `ipmitool` binary.** Rust wraps the spawn failure in `IpmiError`,
  retries, and falls back to AUTO; Python raised an uncaught
  `FileNotFoundError`. `validate --probe` therefore emits a JSON summary in
  Rust where Python emitted a traceback.
- **TOML syntax errors.** The `config: invalid TOML in PATH:` prefix and exit
  code `2` match; parser detail comes from the Rust `toml` crate and source
  snippets that could echo a password are stripped.
- **Help and verbose formatting.** Clap/Typer prose differs; verbose lines are
  unindented.
- **Metrics headers.** Rust sends `Content-Type`/`Content-Length` only; Python's
  `BaseHTTPRequestHandler` also sent `Server`/`Date`. The body is identical.
- **`--metrics-port` type.** Parsed as `u16`; a value above 65535 is a usage
  error. Config-file values are range-checked with the same bound.
- **Fatal `traceback` field.** Contains a Rust backtrace instead of a Python
  traceback.
- **Process groups and daemon ownership.** Rust starts `ipmitool` in its own
  process group and prefers `SUDO_USER` over `getpass.getuser()`.

## Verified by

- `src/**` unit tests mirroring the Python suite, including every controller
  case, runtime-loop orchestration with fake sleeper/device/sinks, installer
  idempotence/argv, and Rust-specific seams.
- `tests/cli.rs`: 22 end-to-end cases with a fake ipmitool, including a live
  metrics scrape, SIGTERM shutdown, non-string identity fields, and an
  out-of-range metrics port.
- `tests/daemon_cli.rs`: 11 end-to-end cases with fake sudo/systemctl.
- `tests/architecture.rs`: dependency-direction and `unsafe` guards.
- Manual oracle diff against the Python binary (command output, run events,
  verbose logs, daemon plans/status) before the compatibility layer was
  simplified.
- Live iDRAC round on 2026-11-20: `validate --probe` (67 °C → 12 %), `status`
  (68 °C, six fans), `set 20` (RPMs dropped to ~3.8k), `auto`, a 1 s `run` with
  live `/metrics`, clean Ctrl-C AUTO restore, then a real `daemon install` and
  restart. Installed service: `unit_version: 0.3.1`, 2 tasks, 0 IPMI errors,
  ~8.7 MiB peak cgroup memory, hardened unit accepted by the real ipmitool.
