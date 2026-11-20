# Python ↔ Rust parity contract

This file records what "parity" means for the rewrite and how it is verified.
The Rust implementation was built against the Python 0.3.1 tree at
`v0.3.1`; both were driven with the same fake `ipmitool` and their outputs
diffed.

## CLI surface

| Command | Rust | Notes |
| --- | --- | --- |
| `--version` | ✅ `breezed 0.3.1` | byte-identical |
| `run [-c PATH] [--metrics-port N] [-v]` | ✅ | same defaults, SIGINT/SIGTERM shutdown |
| `set PCT [-c PATH]` | ✅ | validates `1..=100` before loading config |
| `auto [-c PATH]` | ✅ | |
| `status [--config PATH] [--verbose]` | ✅ | same JSON keys/order; verbose text |
| `validate PATH [--probe]` | ✅ | same summary/`probe` shapes |
| `daemon install/status/uninstall` | ✅* | same shape/JSON/exit codes; install backend is Rust-native (see below) |
| `daemon apply/remove` | ✅* | hidden; `apply --owner STR`, `remove` |
| exit codes | ✅ | `0` ok, `1` runtime/IPMI, `2` usage/config |

`*` The deployment backend intentionally differs: the Rust installer copies the
running binary and does not use uv or a Python runtime. See
[Daemon deployment](#daemon-deployment).

Machine output uses Python `json.dumps` defaults: insertion-ordered keys,
`", "` / `": "` separators, `null`, and `ensure_ascii` escaping. This is
asserted byte-for-byte for command output and per-event log lines.

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

- `[settings]` holds host/user/password and the four positive-int fields;
  top-level `[[curve]]` holds points.
- `IDRAC_HOST`, `IDRAC_USER`, `IDRAC_PASSWORD` override the file when non-empty.
- Missing identity: one combined `host: missing (...); user: missing (...)`
  error, prefixed `Value error, ` exactly like the pydantic adapter.
- `bool` is rejected where an integer is required (`expected an integer, got
  bool`), with Python type names (`str`, `float`, `dict`, `list`, `NoneType`).
- Omitted/empty curve falls back to the shipped four-point default.
- `curve` placed inside `[settings]` is ignored (the original always reads the
  top-level table) — preserved bug-for-bug.
- Unknown keys are ignored.
- Hot reload triggers on mtime change; a failed reload keeps the last good
  settings and emits `config_error`.

## Logging / events

Nine closed event names: `startup`, `poll`, `mode_change`, `speed_change`,
`hysteresis_wait`, `config_reload`, `config_error`, `ipmi_error`, `shutdown`.

Field order is preserved per event, e.g.:

```json
{"event": "poll", "temp_c": 63, "fan_pct": 7, "mode": "manual", "target_pct": 10, "ts": "...", "level": "INFO", "logger": "breezed"}
{"event": "ipmi_error", "operation": "enable_auto", "reason": "read_failures", "failures": 1, "error": "...", "ts": "...", "level": "INFO", "logger": "breezed"}
```

Errors go to stderr as `breezed: …`; stdout stays machine-readable. `--verbose`
switches to human lines shaped like Rich's handler (local timestamp + level +
event name) on stdout.

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

Same fixed sequence, labels, file modes/owners, and retained state:

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

The unit is now stamped, so `daemon status` can report version drift instead of
`null`. The unit itself is hardened for a Rust daemon (`Type=exec`,
`MemoryHigh=16M`/`MemoryMax=32M`, `TasksMax=16`, `MemoryDenyWriteExecute`,
`SystemCallFilter=@system-service`, `ProtectProc=invisible`, W^X, etc.) and no
longer carries the Python-only `MALLOC_ARENA_MAX` knob.

## Verified by

- `src/**` unit tests mirroring the Python suite, including every controller
  case, runtime-loop orchestration with fake sleeper/device/sinks, installer
  idempotence/argv, and Rust-specific seams.
- `tests/cli.rs`: 20 end-to-end cases with a fake ipmitool, including a live
  metrics scrape and SIGTERM shutdown.
- `tests/daemon_cli.rs`: 11 end-to-end cases with fake sudo/systemctl.
- `tests/architecture.rs`: dependency-direction and `unsafe` guards.
- Manual oracle diff against the Python binary (command output, run events,
  verbose logs, daemon plans/status).
- Live iDRAC round on 2026-11-20: `validate --probe` (67 °C → 12 %), `status`
  (68 °C, six fans), `set 20` (RPMs dropped to ~3.8k), `auto`, a 1 s `run` with
  live `/metrics`, clean Ctrl-C AUTO restore, then a real `daemon install` and
  restart. Installed service: `unit_version: 0.3.1`, 2 tasks, 0 IPMI errors,
  ~8.7 MiB peak cgroup memory, hardened unit accepted by the real ipmitool.

Known non-behavioural differences are listed at the end of `README.md`.
