# breezed — SPEC

Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI.
Replacement for the C# JDMallen.IPMITempMonitor currently running on mmsrv.

## Goals

- Multi-point fan **curve** with linear interpolation between points
- Safe by default: iDRAC AUTO mode remains the fallback at all times
- Nice CLI + foreground daemon; **systemd is the only deployment path**, installed
  and upgraded idempotently via `breezed daemon install`
- Structured output for humans, machines, and agents
- Prometheus-format metrics socket for netdata scraping later
- Python 3.13, managed entirely by uv; Astral toolchain (ruff, ty, pre-commit)

## Non-goals (v1)

- Web UI / dashboard (netdata covers this)
- Multi-host support (one iDRAC per instance)
- PID control mode in v1 — curves only, but the `SpeedPolicy` seam reserves room
  for linear/two-step/PID strategies later
- OCI container packaging (systemd only; revisit if ever needed)

## Locked decisions

| Topic | Decision |
|---|---|
| Curve driver | Max CPU temp from `ipmitool sdr type temperature`, regex `(?<=0Eh\|0Fh).+(\d{2})` |
| Above curve top | Force iDRAC AUTOMATIC fan control |
| Below curve bottom | Clamp to lowest point's percentage |
| Startup behavior | First successful sensor read before touching fans; failures count toward limit |
| Sensor read failures | N consecutive (default 3) → force AUTOMATIC |
| Hysteresis | Downward speed changes require temp to hold below boundary for `step_down_hysteresis_s` (default 30s); upward changes immediate |
| Config | TOML file; hot-reload on mtime change each poll; invalid reload keeps last good config |
| Secrets | Env only: `IDRAC_HOST`, `IDRAC_USER`, `IDRAC_PASSWORD` (host/user may also live in config file; env wins) |
| Poll interval | 10s default |
| Logs | JSON to stdout by default; `--verbose` switches to human format |
| Agent/machine output | JSON (same as structured logs — no third format) |
| Metrics | Opt-in `--metrics-port` (e.g. 9762), binds `127.0.0.1`, Prometheus text format |
| Graceful shutdown | On SIGTERM/SIGINT restore iDRAC AUTOMATIC before exit (best effort) |
| Default curve | `(45°C→6%, 60°C→8%, 68°C→12%, 74°C→18%)`, ≥78 → AUTO |
| Ports & adapters | `ports.py` defines `TempReader`/`FanCommander` Protocols; `ipmi.py` is one swappable adapter; dependency arrows point at ports |
| Control strategy | Speed decision behind a `SpeedPolicy` Protocol (`target_pct(temp_c, settings)`); `CurvePolicy` ships in v1; hysteresis/safety stay central in the controller |
| Event vocabulary | `EventType(StrEnum)` in the domain layer — closed set enforced by ty at emit sites; no runtime string guard |
| Validation split | Adapters (config loader) validate structure only; all business rules live in domain constructors/validators (`make_fan_pct`, `make_positive_int`, `validate_curve`) |
| Deployment | systemd only: `breezed daemon install` runs unprivileged and renders a self-contained, idempotent POSIX-sh script that the user reviews and escalates themselves (`sudo sh <script>`); no Docker/Containerfile |

## CLI shape

```
breezed run [--config PATH] [--metrics-port INT] [-v]     # daemon (foreground)
breezed set <PCT>                                          # one-shot manual %
breezed auto                                               # one-shot back to iDRAC control
breezed status [--config PATH]                             # single snapshot: temps, RPMs, curve target
breezed validate <PATH> [--probe]                          # check config; --probe reads live temp + shows result
breezed daemon install [--start]                           # idempotent systemd unit install/upgrade (root)
breezed daemon status                                      # installed version + unit state
breezed daemon uninstall                                   # disable/remove unit, keep user/env/config
```

Exit codes: `0` ok, `1` runtime error, `2` usage/config error.

## Config example

```toml
[settings]
host = "169.254.0.1"
poll_interval_s = 10
read_failure_limit = 3
step_down_hysteresis_s = 30
# metrics_port = 9762

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
```

## Strong typing requirements

- All domain values use typed wrappers, never bare primitives:
  - `TempC = NewType("TempC", int)` — sensor readings
  - `FanPercent = NewType("FanPercent", int)` — duty cycle, validated 1–100 at construction
  - `Celsius = NewType(...)` used in curve points
- All dataclasses are `@dataclass(frozen=True, slots=True)`; no mutation of settings/points after load
- `OperatingMode` / `ControlState` are `StrEnum`s (`unknown`, `auto`, `manual`) — never string literals
- Config parsing returns `Settings` or raises `ConfigError` (with field-level messages); no silent defaults for missing required keys
- Public functions have full annotations; `ty check` passes clean with zero ignores
- `IpmiClient` is one adapter implementing the `TempReader` / `FanCommander`
  Protocols declared in `ports.py` (structural typing — adapters never inherit);
  tests inject fakes without monkey-patching. The speed decision sits behind a
  `SpeedPolicy` Protocol so alternative control loops can replace the curve
- No `Any` in src/ except at the two sanctioned process-boundary sites: the IPMI
  adapter (T4) and the daemon installer's systemd runner (T8)

## Metrics (Prometheus text format, port opt-in)

```
breezed_temp_c{sensor="cpu_max"} 63
breezed_fan_percent 12
breezed_mode{mode="manual"} 1
breezed_ipmi_errors_total 0
breezed_polls_total 42
```

## Structured log events (JSON lines on stdout)

```json
{"ts": "2026-08-25T04:24:30Z", "level": "INFO", "event": "mode_change", "from": "manual", "to": "auto", "reason": "temp_above_curve", "temp_c": 80}
{"ts": "...", "level": "INFO", "event": "poll", "temp_c": 65, "fan_pct": 8, "mode": "manual", "target_pct": 8}
```

Events: `startup`, `poll`, `mode_change`, `speed_change`, `hysteresis_wait`, `config_reload`, `config_error`, `ipmi_error`, `shutdown`.

## Test strategy

Pure logic (curve, config, controller) is fully unit-tested against fakes;
only `IpmiClient` touches subprocesses and gets one integration-style test
with canned SDR output fixtures captured from mmsrv.

### Curve engine (`test_curve.py`)
1. Below first point clamps to first `fan_pct`
2. Exactly on a point returns that point's `fan_pct`
3. Midpoint between two points returns rounded linear value
4. Quarter-position interpolates proportionally (e.g. 52°C between 45→6% and 60→8% ⇒ 7%)
5. At/above top point returns `None` (AUTO signal)
6. Empty curve raises `ValueError`
7. Single-point curve: below → pct, at-or-above → `None`
8. Non-monotonic curve rejected by validator (config test overlap ok)

### Config (`test_config.py`)
1. Full valid TOML loads into expected frozen `Settings`
2. Missing `[settings]` host/user + env unset ⇒ `ConfigError` naming the field
3. Env vars override file values for host/user/password
4. Curve points not strictly ascending by `temp_c` ⇒ `ConfigError`
5. `fan_pct` out of 1–100 ⇒ `ConfigError`
6. Zero/negative intervals ⇒ `ConfigError`
7. Omitted curve falls back to built-in default curve
8. Unknown keys are ignored (forward compatibility)

### Controller state machine (`test_controller.py`, fake IPMI)
1. Cold start: no fan commands until first successful read
2. Temp under curve while in unknown/auto mode ⇒ manual switch + correct pct applied
3. Temp rises across top of curve ⇒ AUTO enabled exactly once (no repeated calls while already auto)
4. Temp falls back under curve after hysteresis ⇒ manual resumed at interpolated pct
5. 2 consecutive failures (< limit) ⇒ no action; 3rd ⇒ AUTO forced once
6. Recovery: failure streak resets after a good read
7. Upward speed change applies immediately
8. Downward change waits full hysteresis window, then applies
9. Downward change cancelled when temp rises again mid-window
10. Interpolated target equal to current pct ⇒ no command issued
11. Shutdown hook restores AUTO even if last state was manual
12. Invalid curve during hot-reload keeps last good config and logs `config_error`

### IPMI client (`test_ipmi.py`)
1. Parses canned R620/R720-style SDR output fixture → max CPU temp
2. Empty SDR output raises `IpmiError`
3. Non-zero exit code raises `IpmiError` including stderr snippet
4. Speed command formats percentage as hex byte (10 ⇒ `0x0a`)
5. Password never appears in any raised exception message/log record

## Tickets

1. **T1 — Scaffold & toolchain**: uv project layout, pyproject (ruff/ty config), `.pre-commit-config.yaml` (ruff-check --fix, ruff-format, ty), pytest wired, MIT LICENSE, README skeleton, GitHub Actions CI running pre-commit + pytest gates on PRs, private `martinmiglio/breezed` repo via gh. AC: `uv sync && uv run pytest && uvx ruff check . && uvx ty check && uvx pre-commit run --all-files` all green; CI green on first PR.
2. **T2 — Domain types & curve engine**: NewTypes, frozen dataclasses, StrEnums (`OperatingMode`, `EventType`), domain constructors (`make_fan_pct`, `make_positive_int`), `DomainError` base, `interpolate()`, curve validator. All business-rule validation lives here. AC: curve + type tests pass.
3. **T3 — Config loader**: structural TOML → Settings adapter only; delegates every business rule to T2's domain constructors/validators and wraps failures as field-named `ConfigError`; env override; hot-reload helper. AC: config tests pass.
4. **T4 — IPMI adapter**: `ports.py` Protocols; `ipmi.py` implements them over subprocess ipmitool; SDR parsing, hex speed command, error type, fixtures from mmsrv. Swappable for other BMC clients. AC: ipmi tests pass against fixtures.
5. **T5 — Controller loop**: state machine with injected `SpeedPolicy` (CurvePolicy default), hysteresis, failure fallback, hot-reload integration, graceful-shutdown AUTO restore. AC: controller tests pass.
6. **T6 — Observability**: JSON logging + `EventType`-typed events, verbose formatter, metrics server. AC: log event assertions in controller tests; metrics endpoint renders documented fields.
7. **T7 — CLI**: Typer app with run/set/auto/status/validate (+`--probe`), exit-code contract. AC: `--help` snapshot test; status/validate work against fake client in tests.
8. **T8 — Daemon deploy & docs**: `daemon install/status/uninstall` subcommands (idempotent systemd unit management), unit template packaged, README final. No container artifacts. AC: `systemd-analyze verify` passes; daemon install is idempotent end-to-end.
