# T2 — Domain types & curve engine

## Goal

Implement the pure domain layer per SPEC "Strong typing requirements": NewType wrappers
(`TempC`, `FanPercent`, `Celsius`), a validated constructor for `FanPercent` (1–100),
frozen/slotted dataclasses for curve points, a curve validator enforcing strictly
ascending `temp_c`, the `OperatingMode` StrEnum, and the `interpolate()` function —
all stdlib-only and fully unit-tested against SPEC curve test cases 1–8.

## Depends on

(T1)

## Files

- `src/breezed/types.py` (new) — `TempC`, `FanPercent`, `Celsius` NewTypes;
  `make_fan_pct(value: int) -> FanPercent` validated constructor (raises `ValueError`
  outside 1–100); `make_positive_int(name: str, value: int) -> int` validated
  constructor for interval-style settings (raises `ValueError` naming the field when
  `value <= 0`) so T3's loader holds zero business rules of its own;
  `OperatingMode(StrEnum)` with members `UNKNOWN`, `AUTO`, `MANUAL`; and
  `EventType(StrEnum)` — the closed log-event vocabulary (`STARTUP`, `POLL`,
  `MODE_CHANGE`, `SPEED_CHANGE`, `HYSTERESIS_WAIT`, `CONFIG_RELOAD`,
  `CONFIG_ERROR`, `IPMI_ERROR`, `SHUTDOWN`; lowercase values matching the SPEC
  event names) so ty enforces event names at every emit site.
  Also define a shared `DomainError(ValueError)` base here so later tickets
  (`ConfigError` in T3, `IpmiError` in T4) can subclass it.
- `src/breezed/curve.py` (new) — frozen/slotted `CurvePoint(temp_c: Celsius,
  fan_pct: FanPercent)`; `validate_curve(points: Sequence[CurvePoint]) -> tuple[CurvePoint, ...]`
  (empty → `ValueError`; non-strictly-ascending `temp_c` → `ValueError`; returns a
  normalized tuple copy); `interpolate(curve: Sequence[CurvePoint], temp_c: TempC) -> int | None`.
- `tests/test_curve.py` (new) — exactly SPEC cases 1–8, listed below.
- `tests/test_types.py` (new, implementation-time addition) — covers the AC tests that
  aren't curve cases: make_fan_pct boundaries, make_positive_int field naming,
  EventType vocabulary set, OperatingMode lowercase serialization.
- Do **not** touch `pyproject.toml` — no new dependencies; this layer is stdlib-only
  (`dataclasses`, `enum`, `typing`). If ruff/ty flag anything, fix the code, not the config.

## Tasks

1. Create `src/breezed/types.py`: the three NewTypes, `make_fan_pct()`, and
   `make_positive_int()` (message pattern: `{name} must be > 0, got {value}`), raising
   `ValueError` with messages naming the bounds/field. Add `OperatingMode` as a
   `StrEnum`, plus `EventType(StrEnum)` whose member values are the lowercase SPEC
   event names — T5 emits these members and T6 derives its test vocabulary from
   them. Keep `__all__` explicit.
1a. **Validation-split principle** (feedback-driven): every business rule lives in a
   domain constructor or validator in this module/curve.py — never inline in an
   adapter. If a rule can be phrased as "this value must satisfy X", it gets a
   constructor here. Adapters call constructors; they never re-implement checks.
2. **Decision — `OperatingMode` lives here, not in T5.** Rationale: SPEC's strong-typing
   section groups all domain wrappers/enums together, it has zero dependencies on
   controller state, and T3/T5/T7 will import it from one canonical place. T5 will add
   `ControlState` to its own module when its shape is known; do not preempt it here.
3. Create `src/breezed/curve.py`: `CurvePoint` as `@dataclass(frozen=True, slots=True)`
   with annotated fields only (no defaults). Curve is passed around as
   `tuple[CurvePoint, ...]`; accept `Sequence[CurvePoint]` at public boundaries and
   return tuples from validators.
4. Implement `validate_curve()`: reject empty input; reject any pair where
   `points[i].temp_c >= points[i+1].temp_c` (strictly ascending; equal temps rejected).
   Duplicate points are duplicates of the same failure mode — one clear error message
   naming the offending indices/values is enough.
5. Implement `interpolate()`:
   - Empty curve → raise `ValueError` (defensive; validator normally runs first).
   - At-or-above top point's `temp_c` → return `None` (the AUTO signal per locked
     decision). This includes exact equality with the top temp.
   - Below first point → clamp to first point's `fan_pct`.
   - Otherwise linear interpolation between bracketing points, returned as `int`.
6. **Decide rounding once and document it in a docstring**: use built-in `round()`
   (banker's rounding) applied to the interpolated float. Justification: deterministic,
   stdlib, and SPEC's example works either way (52°C between 45→6% and 60→8% gives
   6.933… → 7 regardless of tie-breaking). Do NOT use `int()` truncation — it biases
   every fractional result downward and would make a true midpoint (e.g. 7.5) round to
   7 instead of the expected even value. State this choice in the `interpolate()`
   docstring so T5 inherits it without re-litigating.
7. Write `tests/test_curve.py` covering exactly the eight SPEC cases below as separate
   test functions (one per checkbox, named after the behavior, e.g.
   `test_below_first_point_clamps`). Use plain pytest asserts, parametrize where it keeps
   things tighter (e.g. case 4 quarter-position). No fixtures needed — pure functions.
8. Run the verification commands below; also run `uvx ruff format .` before checking.
9. Update README "Status" line noting T2 complete (one line change).

## Acceptance criteria

- [ ] `src/breezed/types.py` exports `TempC`, `FanPercent`, `Celsius`,
      `make_fan_pct`, `make_positive_int`, `DomainError`, `OperatingMode`, `EventType`
      via `__all__`; imports nothing beyond stdlib
- [ ] `make_fan_pct(0)` and `make_fan_pct(101)` raise `ValueError`;
      boundary values `make_fan_pct(1)` / `make_fan_pct(100)` succeed
- [ ] `make_positive_int("poll_interval_s", 0)` / negative raise `ValueError`
      naming the field; positive values pass through as `int`
- [ ] `EventType` members' `.value`s are exactly the nine SPEC event names;
      add a test asserting the set equals the documented vocabulary
- [ ] `src/breezed/curve.py` defines frozen+slotted `CurvePoint` and passes
      `validate_curve()` for strictly ascending input; empty curve raises `ValueError`
- [ ] `interpolate()` returns `int | None`: `None` at-or-above top of curve, clamped
      first-point pct below bottom, rounded linear value between points
- [ ] `tests/test_curve.py` implements SPEC curve cases, one test each:
  - [ ] 1. Below first point clamps to first `fan_pct`
  - [ ] 2. Exactly on a point returns that point's `fan_pct`
  - [ ] 3. Midpoint between two points returns rounded linear value
  - [ ] 4. Quarter-position interpolates proportionally (52°C between 45→6% and 60→8% ⇒ 7%)
  - [ ] 5. At/above top point returns `None` (AUTO signal)
  - [ ] 6. Empty curve raises `ValueError`
  - [ ] 7. Single-point curve: below → pct, at-or-above → `None`
  - [ ] 8. Non-monotonic curve rejected by validator (equal adjacent temps count as non-monotonic)
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest tests/test_curve.py -v`
  - [ ] `uv run pytest` (full suite still green alongside T1's `test_version.py`)
  - [ ] `uvx ty check` (zero errors, zero ignores)
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`

## Notes

- **Rounding semantics**: settled in task 6 — `round()` (banker's), never `int()` cut.
  Documented in the `interpolate()` docstring; if a reviewer disagrees, change it *here*
  and update case 3/4 expectations together, nowhere else.
- **Empty curve raises `ValueError`** in both `validate_curve()` (config path) and
  `interpolate()` (defensive). SPEC case 8 notes overlap with T3 config tests — fine;
  T2 tests the validator directly, T3 re-tests through TOML parsing.
- **Stdlib only** in these modules. `typer` must not appear in an import graph reachable
  from `types.py`/`curve.py`; this keeps the engine trivially testable and reusable.
- **No bare primitives cross module boundaries going forward**: T3/T5 should call
  `make_fan_pct()` at construction sites rather than casting raw ints. Don't add a
  validating `__post_init__` to `CurvePoint` — validation happens at the constructor
  function level, keeping dataclasses dumb and frozen.
- `TempC` vs `Celsius` look redundant; they are not: `TempC` marks *sensor readings*,
  `Celsius` marks *curve point abscissas*. Keep them distinct so `interpolate(curve,
  temp_c: TempC)` signatures read correctly and mixing them up shows up under `ty`.
- `OperatingMode` member values are lowercase strings matching log events/SPEC examples:
  `StrEnum` members serialize via `str()` directly into JSON logs in T6 — do not add a
  `.value` mapping layer later.
- Use `uvx` for ruff/ty per T1's note; the system-wide tools are stale.

## Draft interfaces (for review)

> DRAFT for human review — sketches, not implementations. Names/signatures below are
> the proposal; they must stay identical wherever other tickets reference them
> (`EventType` → consumed by T5 emits and T6 derivation; `DomainError` → subclassed
> by T3/T4; `make_fan_pct`/`make_positive_int` → called by T3/T5/T7).

```python
# src/breezed/types.py
from dataclasses import dataclass
from enum import StrEnum
from typing import NewType

TempC = NewType("TempC", int)  # sensor readings (max CPU temp from SDR)
FanPercent = NewType("FanPercent", int)  # duty cycle, validated 1..100 at construction
Celsius = NewType("Celsius", int)  # curve point abscissas — distinct from TempC


def make_fan_pct(value: int) -> FanPercent:
    """Validated constructor; raises ValueError outside 1..100."""
    ...


def make_positive_int(name: str, value: int) -> int:
    """Interval-style setting; raises ValueError('{name} must be > 0, got {value}')."""
    ...


class DomainError(ValueError):
    """Shared base; T3's ConfigError and T4's IpmiError subclass this."""


class OperatingMode(StrEnum):
    """Domain vocabulary; members serialize via str() straight into JSON logs (T6)."""

    UNKNOWN = "unknown"
    AUTO = "auto"
    MANUAL = "manual"


# contract owner: T2 types.py — T5 emits these members; T6 derives SPEC_EVENT_NAMES
class EventType(StrEnum):
    STARTUP = "startup"
    POLL = "poll"
    MODE_CHANGE = "mode_change"
    SPEED_CHANGE = "speed_change"
    HYSTERESIS_WAIT = "hysteresis_wait"
    CONFIG_RELOAD = "config_reload"
    CONFIG_ERROR = "config_error"
    IPMI_ERROR = "ipmi_error"
    SHUTDOWN = "shutdown"


__all__ = [
    "TempC",
    "FanPercent",
    "Celsius",
    "make_fan_pct",
    "make_positive_int",
    "DomainError",
    "OperatingMode",
    "EventType",
]

# src/breezed/curve.py
from collections.abc import Sequence
from dataclasses import dataclass

from breezed.types import Celsius, FanPercent, TempC  # noqa: compile-check strips


@dataclass(frozen=True, slots=True)
class CurvePoint:
    temp_c: Celsius
    fan_pct: FanPercent


def validate_curve(points: Sequence[CurvePoint]) -> tuple[CurvePoint, ...]:
    """Empty → ValueError; any adjacent pair with temp_c[i] >= temp_c[i+1] → ValueError.

    Returns a normalized tuple copy.
    """
    ...


def interpolate(curve: Sequence[CurvePoint], temp_c: TempC) -> int | None:
    """At-or-above top point → None (AUTO signal); below first → first fan_pct;

    otherwise linear interpolation between bracketing points, rounded with built-in
    round() (banker's rounding — never int() truncation).
    """
    ...


__all__ = ["CurvePoint", "validate_curve", "interpolate"]
```
