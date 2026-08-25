# T3 — Config loader

## Goal

Implement the TOML config layer per SPEC "Strong typing requirements" and the locked
decisions table: a `ConfigError` with field-level messages, `load_settings(path) -> Settings`
parsing `[settings]` + `[[curve]]` via stdlib `tomllib`, env-var overrides for secrets
(`IDRAC_HOST` / `IDRAC_USER` / `IDRAC_PASSWORD`, env wins over file), full validation
(positive intervals, `fan_pct` 1–100, strictly ascending curve via T2's validator,
omitted curve → built-in default), and an mtime-based hot-reload helper so T5 can
reload each poll and keep the last-good settings on failure. Fully unit-tested against
SPEC config cases 1–8.

## Depends on

(T1, T2)

## Files

- `src/breezed/config.py` (new) — everything in this ticket, stdlib-only
  (`tomllib`, `dataclasses`, `os`, `pathlib`, `typing`). Imports from `breezed.types`
  (`DomainError`, `FanPercent`, `make_fan_pct`, `TempC`, `Celsius`) and
  `breezed.curve` (`CurvePoint`, `validate_curve`). No `typer`, no third-party imports.
- `tests/test_config.py` (new) — exactly SPEC cases 1–8 below, plus hot-reload tests.
- `tests/fixtures/config_valid.toml` (new) — minimal fixture mirroring SPEC's config
  example (host + user in `[settings]`, four-point curve). The polished example lives
  in `deploy/` in T8; keep this one minimal.
- Do **not** touch `pyproject.toml` — no new dependencies. If ruff/ty flag anything,
  fix the code, not the config.

## Tasks

1. Define `ConfigError(DomainError)` in `config.py`. Every raise must name the offending
   field (and, where useful, the offending value): e.g.
   `host: missing (set [settings].host or IDRAC_HOST)`,
   `poll_interval_s: must be > 0, got {value}`,
   `curve[2].fan_pct: must be in 1..100, got {value}`.
   Wrap `tomllib.TOMLDecodeError` and `OSError` (missing/unreadable file) into
   `ConfigError` too — callers should only ever need to catch `ConfigError`.
2. Define `DEFAULT_CURVE: tuple[CurvePoint, ...]` here from the locked decision:
   `(45→6%, 60→8%, 68→12%, 74→18%)`, built via `make_fan_pct()` per T2's note — no bare
   primitives crossing boundaries. Export it via `__all__`; T5 will use it as fallback.
3. Define `Settings` as `@dataclass(frozen=True, slots=True)` with fields:
   `host: str`, `user: str`, `password: str`, `curve: tuple[CurvePoint, ...]`,
   `poll_interval_s: int = 10`, `read_failure_limit: int = 3`,
   `step_down_hysteresis_s: int = 30`, `metrics_port: int | None = None`,
   `ipmitool_path: str = "/usr/bin/ipmitool"`.
   No defaults for `host`/`user` — they are required (from file or env). `password`
   has no default either but may be empty string if genuinely unset; it is env-only
   per SPEC's locked secrets decision.
4. Implement `load_settings(path: str | Path) -> Settings`:
   - Open with a **binary** handle: `with open(path, "rb") as f: data = tomllib.load(f)`
     (see Notes). Parse `[settings]` table and `[[curve]]` array-of-tables; both are
     optional at the parse level — validation decides what's missing.
   - TOML ints arrive already as `int` — validate types defensively anyway (a string
     where an int belongs ⇒ `ConfigError` naming the field), but do not coerce.
   - Env resolution order per field: `IDRAC_*` env var → file value → `ConfigError`.
      Missing host/user with both unset ⇒ `ConfigError` naming the specific field(s)
      (SPEC case 2 expects the field named).
    - Curve handling: absent or empty `[[curve]]` ⇒ `Settings.curve = DEFAULT_CURVE`.
      Present ⇒ map rows through `make_fan_pct()` / construct `CurvePoint`s and run
      T2's `validate_curve()`; convert its `ValueError` into `ConfigError` preserving
      the message (do not duplicate the ascending-check logic).
    - **All business rules delegate to domain constructors** (feedback: adapters
      validate structure, domain validates rules): `poll_interval_s`,
      `read_failure_limit`, `step_down_hysteresis_s` go through
      `make_positive_int(field_name, value)`; `metrics_port` is either `None` or
      `make_positive_int("metrics_port", value)`. The loader's only job for these
      fields is catching the resulting `ValueError` and re-raising as `ConfigError`
      with the field name — zero arithmetic/comparison logic lives in this module.
      A non-int where an int belongs (TOML string `"10"`) is a *structural* error:
      reject here with `ConfigError`, do not pass to domain.
   - Unknown keys anywhere ([settings], top level, curve rows) are ignored silently —
     forward compatibility is a requirement (SPEC case 8), not an error.
   - Return a frozen `Settings`; never mutate anything after construction.
5. Implement the hot-reload helper `ConfigWatcher`:
   - `ConfigWatcher(path)` records the initial `stat().st_mtime_ns`.
   - `.changed() -> bool` re-stats and compares mtime (cheap enough to call every poll);
     treat a vanished file as changed so the reload path surfaces the error properly.
   - `.reload() -> Settings` calls `load_settings()` and refreshes the tracked mtime on
     success only — a failed load leaves the watcher state untouched so the caller's
     last-good `Settings` stays valid (SPEC: invalid reload keeps last good config;
     controller test 12 depends on this contract).
   - Keep it dumb: no caching of Settings inside the watcher beyond what's needed to
     track mtime; the caller owns the last-good object.
6. Write `tests/test_config.py` covering exactly the eight SPEC cases below as separate
   tests (one checkbox each, behavior-named like T2, e.g. `test_env_overrides_file`),
   using the fixture for case 1 and inline-written temp TOML files (via
   `tmp_path`) for the negative cases. For env cases use pytest `monkeypatch.setenv`/
   `delenv` — never mutate `os.environ` directly. Add two extra tests for the
   hot-reload contract: mtime change yields fresh `Settings`, and a failed reload
   raises while leaving the previously loaded `Settings` usable.
7. Run the verification commands below; also run `uvx ruff format .` before checking.
8. Update README "Status" line noting T3 complete (one line change).

## Acceptance criteria

- [ ] `src/breezed/config.py` exports `ConfigError`, `DEFAULT_CURVE`, `Settings`,
      `load_settings`, `ConfigWatcher` via `__all__`; imports nothing beyond stdlib +
      `breezed.types`/`breezed.curve`
- [ ] `Settings` is `@dataclass(frozen=True, slots=True)` with the exact field set and
      defaults from task 3; `host`/`user`/`password` have no silent defaults
- [ ] `ConfigError` subclasses `DomainError` from T2; every raise names the offending
      field; `tomllib` decode errors and file-not-found surface as `ConfigError`
- [ ] Env vars `IDRAC_HOST`/`IDRAC_USER`/`IDRAC_PASSWORD` override file values; missing
      required fields with env unset ⇒ `ConfigError` naming the field
- [ ] Omitted or empty `[[curve]]` ⇒ `DEFAULT_CURVE` `(45→6, 60→8, 68→12, 74→18)`
      built through T2 constructors; present-but-invalid curve delegates to
      `validate_curve()` (no duplicated ascending logic)
- [ ] Zero/negative intervals and out-of-range `fan_pct` ⇒ `ConfigError`; unknown keys
      ignored everywhere
- [ ] `tests/test_config.py` implements SPEC config cases, one test each:
  - [ ] 1. Full valid TOML loads into expected frozen `Settings`
  - [ ] 2. Missing `[settings]` host/user + env unset ⇒ `ConfigError` naming the field
  - [ ] 3. Env vars override file values for host/user/password
  - [ ] 4. Curve points not strictly ascending by `temp_c` ⇒ `ConfigError`
  - [ ] 5. `fan_pct` out of 1–100 ⇒ `ConfigError`
  - [ ] 6. Zero/negative intervals ⇒ `ConfigError`
  - [ ] 7. Omitted curve falls back to built-in default curve
  - [ ] 8. Unknown keys are ignored (forward compatibility)
- [ ] Hot-reload tests: mtime change detected and reloaded; failed reload raises
      without corrupting watcher state (caller keeps last good)
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest tests/test_config.py -v`
  - [ ] `uv run pytest` (full suite still green alongside T1/T2)
  - [ ] `uvx ty check` (zero errors, zero ignores)
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`

## Notes

- **tomllib requires binary mode**: `open(path, "rb")` + `tomllib.load(f)`, or
  `tomllib.loads(text)` for strings. Text-mode handles raise `UnicodeDecodeError`-flavored
  failures that mask real problems — always binary for files.
- **TOML ints are already `int`** — no coercion needed, but a value like
  `poll_interval_s = "10"` arrives as `str` and must be rejected with `ConfigError`,
  not trusted.
- **Password redaction**: the password value must never appear in any exception
  message, log line, or test assertion output. `ConfigError` messages reference the
  *field name*, never secret values. This mirrors IPMI test case 5 in T4 — keep the
  discipline consistent from day one.
- **Env wins over file** for host/user/password per SPEC's locked secrets decision,
  even when the file has values for all three. Only these three keys read env; intervals
  and paths are file-only (CLI flags will layer on top in T7).
- **Validation split discipline**: this module contains no `> 0`/range comparisons —
  grep for `0` comparisons in review; every rule routes through `breezed.types`
  constructors or `validate_curve`. The only structural checks permitted are:
  key presence, TOML type shape (int-vs-str), and env/file precedence.
- **Example config location**: the polished user-facing example lands in `deploy/` with
  T8; this ticket only adds the minimal `tests/fixtures/config_valid.toml`. Don't create
  `deploy/` early.
- **No mutation anywhere**: `Settings` and `CurvePoint` are frozen/slotted; hot-reload
  means constructing a *new* `Settings`, never editing the old one — T5's loop swaps the
  reference atomically between polls.
- **mtime granularity**: same-second writes can share an `st_mtime_ns` on some
  filesystems; `st_mtime_ns` (not `st_mtime`) keeps precision. Tests writing then
  reloading should bump content/mtime explicitly (`os.utime` with a later timestamp) if
  flaky on the dev machine's fs.
- Use `uvx` for ruff/ty per T1's note; the system-wide tools are stale.

## Draft interfaces (for review)

> DRAFT for human review — sketches, not implementations. Consumes T2's domain
> constructors/validators exclusively (validation-split discipline); `Settings` and
> `DEFAULT_CURVE` shapes are referenced by T4 (constructor arg) and T5 (fallback +
> hot-reload swap).

```python
# src/breezed/config.py
from dataclasses import dataclass
from pathlib import Path

from breezed.curve import CurvePoint, validate_curve  # noqa: compile-check strips
from breezed.types import (  # noqa: compile-check strips
    Celsius,
    DomainError,
    FanPercent,
    TempC,
    make_fan_pct,
    make_positive_int,
)


class ConfigError(DomainError):
    """Field-naming failure; also wraps tomllib.TOMLDecodeError and OSError.

    Messages reference field names, never secret values (password discipline).
    """


# Built through T2 constructors — no bare primitives crossing boundaries.
DEFAULT_CURVE: tuple[CurvePoint, ...] = (
    CurvePoint(temp_c=Celsius(45), fan_pct=make_fan_pct(6)),
    CurvePoint(temp_c=Celsius(60), fan_pct=make_fan_pct(8)),
    CurvePoint(temp_c=Celsius(68), fan_pct=make_fan_pct(12)),
    CurvePoint(temp_c=Celsius(74), fan_pct=make_fan_pct(18)),
)


@dataclass(frozen=True, slots=True)
class Settings:
    host: str                        # required: [settings].host or IDRAC_HOST
    user: str                        # required: [settings].user or IDRAC_USER
    password: str                    # env-only (IDRAC_PASSWORD); may be ""
    curve: tuple[CurvePoint, ...]
    poll_interval_s: int = 10
    read_failure_limit: int = 3
    step_down_hysteresis_s: int = 30
    metrics_port: int | None = None
    ipmitool_path: str = "/usr/bin/ipmitool"


def load_settings(path: str | Path) -> Settings:
    """Binary-mode tomllib load; env wins over file for host/user/password;

    omitted/empty [[curve]] → DEFAULT_CURVE; unknown keys ignored everywhere.
    """
    ...


class ConfigWatcher:
    """mtime_ns-tracked hot-reload helper; caller owns the last-good Settings."""

    def __init__(self, path: str | Path) -> None: ...
    def changed(self) -> bool: ...          # vanished file counts as changed
    def reload(self) -> Settings: ...       # mtime refreshed on success ONLY


__all__ = ["ConfigError", "DEFAULT_CURVE", "Settings", "load_settings", "ConfigWatcher"]
```

