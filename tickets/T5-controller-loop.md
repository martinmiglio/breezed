# T5 — Controller loop

## Goal

Implement the pure decision-making state machine per SPEC's locked decisions table:
a `Controller` class that each `tick()` reads max CPU temp through the T4
`TempReader` Protocol, counts consecutive failures toward the limit (forcing iDRAC
AUTO exactly once when reached), interpolates the curve target via T2's
`interpolate()`, switches to manual mode (disable-auto **then** set pct) whenever
the target is under curve, applies upward speed changes immediately, gates downward
changes behind `step_down_hysteresis_s` (cancellable mid-window), emits the SPEC
structured events through an injected sink, restores AUTO on `shutdown()` even from
manual, and accepts validated hot-reloaded `Settings` without mutating anything.
No I/O of its own beyond the protocol calls; fully unit-tested against SPEC
controller cases 1–12 with a fake client and fake clock.

## Depends on

(T1–T4)

## Files

- `src/breezed/policy.py` (new) — feedback-driven seam for future control loops
  (linear/two-step/PID): `CurvePolicy` implementing the `SpeedPolicy` Protocol by
  delegating to `interpolate()`. The `SpeedPolicy` Protocol itself is declared in
  `breezed.ports` (this ticket appends it there — ports.py owns all Protocols):

  ```python
  class SpeedPolicy(Protocol):
      def target_pct(self, temp_c: TempC, settings: Settings) -> int | None: ...
  ```

  Stateless w.r.t. config on purpose: the controller passes the *current* Settings
  each call, so hot-reload flows through without policies needing refresh hooks;
  stateful strategies (PID integrators) own their internal state on `self`.
  A `FakePolicy` returning fixed values drives the controller tests' policy-injection
  coverage.
- `src/breezed/controller.py` (new) — everything else in this ticket: `ControlState`
   StrEnum, `EventSink` Protocol, `Controller`. Imports from `breezed.types`
   (`TempC`, `FanPercent`, `make_fan_pct`, `OperatingMode`), `breezed.curve`
   (`CurvePoint`, `validate_curve`), `breezed.config` (`Settings`),
   `breezed.ports` (`TempReader`, `FanCommander`, `SpeedPolicy`), and
   `breezed.policy` (`CurvePolicy`). Stdlib-only beyond that
   (`time`, `typing`) — **no** `subprocess`, **no** third-party imports.
- `tests/test_controller.py` (new) — exactly SPEC cases 1–12 below plus the
  hot-reload contract tests, driven by `FakeIpmi` + `FakeClock` defined at the top
  of the file (no fixtures, no monkeypatching).
- Do **not** touch `pyproject.toml` — no new dependencies. If ruff/ty flag anything,
  fix the code, not the config.

## Tasks

1. Define `ControlState(StrEnum)` here with members `UNKNOWN`, `AUTO`, `MANUAL`
   (lowercase values `unknown`/`auto`/`manual`, mirroring `OperatingMode` per T2
   task 2's note). Rationale for a second enum: `OperatingMode` is the *domain*
   vocabulary shared with logs/config; `ControlState` is *this machine's* tracked
   belief about what the iDRAC is currently doing (starts `UNKNOWN` because the
   daemon cannot know the pre-existing fan mode). Keep both so T6 log records can
   distinguish them if needed; do not collapse them into one import.

2. Define the logging seam up front so T6 can slot in the JSON formatter without
   touching this module:

   ```python
   class EventSink(Protocol):
       def emit(self, event: str, /, **fields: object) -> None: ...
   ```

   Every observable action goes through `sink.emit(...)` using exactly the SPEC
   event names this ticket produces: `poll`, `mode_change`, `speed_change`,
   `hysteresis_wait`, `ipmi_error`, `config_error`. Field names follow the SPEC
   examples (`temp_c`, `fan_pct`, `target_pct`, `from`, `to`, `reason`,
   `failures`, `error`). Tests inject a recording fake sink and assert on
   `(event, fields)` pairs — never on log strings. T6 will provide the real sink;
   T7 wires it. Do not import `logging` here.

3. Define `Controller` with this exact constructor shape (two params against T4's
   existing capability Protocols — **decision**: do not invent a combined
   `ControllerClient` Protocol; T4 deliberately split reader/commander so fakes
   stay composable, and `IpmiClient` structurally satisfies both):

   ```python
   def __init__(
       self,
       reader: TempReader,
       commander: FanCommander,
       settings: Settings,
       sink: EventSink,
       *,
       clock: Callable[[], float] = time.monotonic,
       policy: SpeedPolicy | None = None,
   ) -> None: ...
   ```

   `policy` defaults to `CurvePolicy()` — the curve stays the shipped strategy, but
   the controller never calls `interpolate()` directly (feedback: interchangeable
   control loops). **Hysteresis stays central in the controller** (agreed decision):
   the down-step delay is a safety property of the machine, not a property of the
   speed law — keeping it here means a future PID policy inherits it for free and
   the safety story cannot drift between strategies. Revisit only if/when a second
   policy actually lands with conflicting hysteresis needs.

   Internal state: `_state: ControlState = UNKNOWN`, `_last_pct: FanPercent | None`,
   `_failure_streak: int = 0`, `_pending_down: tuple[FanPercent, float] | None`
   (target pct + first-below timestamp), all private. No other public attributes.

4. Implement `tick() -> None` in this order (each numbered step is one code block,
   and the order is normative):
   1. **Read.** Call `reader.read_max_cpu_temp()` in a try/except `IpmiError`.
      On error: increment `_failure_streak`, emit `ipmi_error` (with `failures`
      count and redacted `error` message), then — only if
      `_failure_streak >= settings.read_failure_limit` **and**
      `_state is not AUTO` — call `commander.enable_auto()`, set `_state = AUTO`,
      emit `mode_change(to=auto, reason="read_failures", failures=N)`, and return.
      If the limit was already reached while AUTO, do nothing further (idempotence
      guard — see Notes). Return after any failure tick: never touch fans on data
      you don't have.
   2. **Good read.** Reset `_failure_streak = 0`.
   3. **Interpolate.** `target = self._policy.target_pct(temp, settings)` where
       target is `int | None`; convert through `make_fan_pct(target)` only when it
       is not `None`. (Never import/call `interpolate` here — that would bypass
       the strategy seam.)
   4. **Target `None` (at/above curve top).** Clear any `_pending_down`. If
      `_state is not AUTO`: `commander.enable_auto()`, `_state = AUTO`, emit
      `mode_change(from=old, to=auto, reason="temp_above_curve", temp_c=T)`. If
      already AUTO, emit nothing extra (case 3's "exactly once").
   5. **Target under curve.** Emit the per-tick `poll` event
      (`temp_c`, `fan_pct=_last_pct`, `mode=_state.value`, `target_pct=target`)
      — see Notes for placement — then branch on state:
      - `_state is UNKNOWN or AUTO` (cold/manual-resume path): call
        `commander.disable_auto()` **then** `commander.set_manual_pct(pct)` —
        order locked (setting a pct while auto is still enabled is a no-op on
        iDRAC). Set `_state = MANUAL`, `_last_pct = pct`, clear `_pending_down`,
        emit `mode_change` (`reason="temp_under_curve"`) followed by
        `speed_change(fan_pct=pct, target_pct=target, reason="mode_enter")`.
      - `_state is MANUAL` and `target > _last_pct` (upward): apply immediately —
        `set_manual_pct(make_fan_pct(target))`, update `_last_pct`, clear
        `_pending_down`, emit `speed_change(reason="temp_rise")`.
      - `_state is MANUAL` and `target < _last_pct` (downward):
        - no pending step yet ⇒ record `_pending_down = (make_fan_pct(target),
          clock())`, emit `hysteresis_wait(target_pct=target,
          hysteresis_s=settings.step_down_hysteresis_s)`;
        - pending and `clock() - since >= settings.step_down_hysteresis_s` ⇒ apply
          the stored pct (re-read from the tuple, not the fresh target — see
          Notes), update `_last_pct`, clear pending, emit
          `speed_change(reason="hysteresis_elapsed")`.
      - `target == _last_pct` (equal): issue **no** command and emit no
        `speed_change`; leave any pending step alive only while the fresh target
        is still strictly below `_last_pct` — an equal-or-higher reading cancels
        it (drop pending silently or fold into the upward branch above).
   Only steps that actually change hardware emit `mode_change`/`speed_change`;
   steady-state ticks emit `poll` alone.

5. Implement `shutdown() -> None`: best-effort AUTO restore — call
   `commander.enable_auto()`, set `_state = AUTO`, emit `mode_change(from=…,
   to=auto, reason="shutdown")`, catching and emitting-as-`ipmi_error` any
   `IpmiError` (graceful shutdown must never crash the exit path; SPEC locked
   decision). Idempotent: safe if already AUTO (skip the command, still emit
   nothing extra unless the state actually changed).

6. Implement hot-reload intake — **decision**: the caller (T7 run loop) owns the
   T3 `ConfigWatcher`; the controller never stats files or imports
   `config.ConfigWatcher`. It exposes:

   ```python
   def replace_settings(self, new_settings: Settings) -> bool: ...
   ```

   Validate before swapping: run `validate_curve(new_settings.curve)` (and rely on
   T3 having already range-checked the rest); on `ValueError` emit
   `config_error(error=msg)` and return `False` keeping the current settings
   untouched (SPEC: invalid reload keeps last good config — controller case 12).
   On success swap the frozen reference atomically (`self._settings =
   new_settings` — frozen dataclass means readers can never observe a half-updated
   config), reset `_pending_down` to `None` (a new curve invalidates any in-flight
   hysteresis window; keep `_last_pct` so the next tick compares against reality),
   and return `True`. Never mutate the old `Settings`.

7. Write `tests/test_controller.py` with, at top of file:
   - `FakeClock` — starts at a fixed float; `.advance(seconds)` moves it; callable.
     Used exclusively for hysteresis timing (never sleep).
   - `FakeIpmi` — implements both `TempReader` and `FanCommander` structurally:
     scriptable `temps: list[TempC]` popped per `read_max_cpu_temp()` call (or an
     `errors: list[IpmiError]` interleaved), records every command in
     `commands: list[str]` (`"auto"`, `"manual"`, `"set:12"` style tuples) so tests
     assert exact call sequences including the disable-before-set ordering.
   - `RecordingSink` — appends `(event, fields-dict)` tuples; helper
     `.events(name)` filters.
   Then one test per SPEC case, behavior-named:
   `test_cold_start_no_fan_commands_before_first_read`,
   `test_under_curve_from_unknown_switches_to_manual_with_pct`,
   `test_above_curve_forces_auto_exactly_once`,
   `test_returns_under_curve_after_hysteresis_resumes_manual`,
   `test_third_consecutive_failure_forces_auto_once`,
   `test_good_read_resets_failure_streak`,
   `test_upward_speed_change_applies_immediately`,
   `test_downward_change_applies_only_after_full_hysteresis`,
   `test_midwindow_rise_cancels_pending_down_step`,
   `test_equal_target_issues_no_command`,
   `test_shutdown_restores_auto_even_from_manual`,
   `test_invalid_hot_reload_keeps_last_good_config_and_logs_config_error`.
   Add companion tests for: failure-limit idempotence (limit reached while already
   AUTO ⇒ no repeated `enable_auto`), disable-auto-before-set-pct ordering,
   `replace_settings` success path (next tick uses the new curve), and shutdown
   swallowing an `IpmiError` from `enable_auto`.

8. Run the verification commands below; also run `uvx ruff format .` before checking.
9. Update README "Status" line noting T5 complete (one line change).

## Acceptance criteria

- [ ] `src/breezed/ports.py` gains `SpeedPolicy`; `src/breezed/policy.py` exports
      `CurvePolicy` via `__all__`; controller imports stdlib (`time`, `typing`) +
      `breezed.types` / `breezed.curve` / `breezed.config` / `breezed.ports` /
      `breezed.policy` only; never imports `subprocess`, `logging`, or `tomllib`,
      and never calls `interpolate()` directly
- [ ] Constructor takes `reader: TempReader`, `commander: FanCommander` (T4 Protocols),
      `settings: Settings`, `sink: EventSink`, keyword-only
      `clock: Callable[[], float] = time.monotonic` and
      `policy: SpeedPolicy | None = None` (default `CurvePolicy()`)
- [ ] Cold start: zero fan commands until the first successful read (startup
      behavior locked decision)
- [ ] Failure handling: consecutive `IpmiError`s count toward
      `read_failure_limit`; reaching it forces AUTO exactly once (idempotent while
      already AUTO); any good read resets the streak
- [ ] Under-curve entry into manual always issues `disable_auto()` **before**
      `set_manual_pct()`; AUTO-on-top-target is issued exactly once per transition
- [ ] Hysteresis: downward changes wait the full `step_down_hysteresis_s` measured
      by the injected clock before applying; upward changes immediate; a rise
      mid-window cancels the pending step; equal targets issue nothing
- [ ] All output flows through `sink.emit()` with SPEC event names (`poll`,
      `mode_change`, `speed_change`, `hysteresis_wait`, `ipmi_error`,
      `config_error`) and SPEC field names; no direct prints/logging in the module
- [ ] `replace_settings()` validates the incoming curve first, returns `False` +
      emits `config_error` + keeps last-good on failure, swaps the frozen reference
      and clears pending hysteresis state on success; no `Settings` is ever mutated
- [ ] `shutdown()` best-effort restores AUTO even from manual, swallows/emits
      `IpmiError` instead of raising
- [ ] Tests inject fakes exclusively via constructor parameters (`reader`,
      `commander`, `sink`, `clock`); no sleeps, no monkeypatching anywhere
- [ ] `tests/test_controller.py` implements SPEC controller cases, one test each:
  - [ ] 1. Cold start: no fan commands until first successful read —
        `test_cold_start_no_fan_commands_before_first_read`
  - [ ] 2. Temp under curve while in unknown/auto mode ⇒ manual switch + correct
        pct applied — `test_under_curve_from_unknown_switches_to_manual_with_pct`
  - [ ] 3. Temp rises across top of curve ⇒ AUTO enabled exactly once —
        `test_above_curve_forces_auto_exactly_once`
  - [ ] 4. Temp falls back under curve after hysteresis ⇒ manual resumed at
        interpolated pct — `test_returns_under_curve_after_hysteresis_resumes_manual`
  - [ ] 5. 2 consecutive failures (< limit) ⇒ no action; 3rd ⇒ AUTO forced once —
        `test_third_consecutive_failure_forces_auto_once`
  - [ ] 6. Recovery: failure streak resets after a good read —
        `test_good_read_resets_failure_streak`
  - [ ] 7. Upward speed change applies immediately —
        `test_upward_speed_change_applies_immediately`
  - [ ] 8. Downward change waits full hysteresis window, then applies —
        `test_downward_change_applies_only_after_full_hysteresis`
  - [ ] 9. Downward change cancelled when temp rises again mid-window —
        `test_midwindow_rise_cancels_pending_down_step`
  - [ ] 10. Interpolated target equal to current pct ⇒ no command issued —
        `test_equal_target_issues_no_command`
  - [ ] 11. Shutdown hook restores AUTO even if last state was manual —
        `test_shutdown_restores_auto_even_from_manual`
  - [ ] 12. Invalid curve during hot-reload keeps last good config and logs
        `config_error` —
        `test_invalid_hot_reload_keeps_last_good_config_and_logs_config_error`
- [ ] Companion coverage present: limit-reached-while-AUTO idempotence,
      disable-before-set ordering, successful `replace_settings` picked up next
      tick, shutdown swallowing `IpmiError`, and a `FakePolicy` injection test
      proving the controller drives any strategy (fixed-target fake ⇒ expected
      commands without touching `interpolate`)
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest tests/test_controller.py -v`
  - [ ] `uv run pytest` (full suite still green alongside T1–T4)
  - [ ] `uvx ty check` (zero errors, zero ignores)
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`

## Notes

- **Monotonic clock injection**: the clock is keyword-only and defaults to
  `time.monotonic` (not `time.time` — wall-clock jumps would corrupt hysteresis
  windows). Tests drive `FakeClock.advance()`; there must be no `time.sleep` in
  src or tests, and no `datetime` anywhere in this module.
- **Force AUTO exactly once needs an idempotence guard**: the failure-limit branch
  fires only when `_state is not AUTO`. Without the guard, every failing poll past
  the limit re-sends `raw 0x30 0x30 0x01 0x01` forever — harmless-looking, but it
  breaks case 3's contract and spams `mode_change` records that T6's consumers
  would read as real transitions.
- **Avoid float drift in hysteresis comparisons**: compute
  `clock() - pending_since >= settings.step_down_hysteresis_s` fresh each tick;
  never accumulate elapsed time by adding `poll_interval_s` to a counter (drifts
  under jitter and double-counts on hot-reload ticks). Compare with `>=`, not `>`
  — a window that expires exactly between polls must apply, not wait another full
  interval.
- **Apply the pending pct, not the fresh one**: on hysteresis expiry use the pct
  captured when the window opened. The fresh target may have drifted lower during
  the window; re-using the stored value means one clean `speed_change` now and a
  normal gated step next tick, instead of silently skipping a hysteresis stage.
- **`poll` event placement**: emit it once per successful-read tick with the
  *current* mode/pct and the fresh target, after the mode/speed branches resolve
  their commands — so the JSON line reflects the post-decision state (SPEC example
  shows `mode` and `target_pct` together). Failed-read ticks emit `ipmi_error`
  instead of `poll`.
- **Two enums, one vocabulary**: `OperatingMode` (T2) vs `ControlState` (here)
  look redundant; they are not — same three values, different owners (domain vs
  machine belief). If a reviewer pushes to merge them, merge *here* and update
  case 2/3 assertions together; do not repurpose the domain enum as mutable state.
- **Ordering is the safety story**: `disable_auto()` before `set_manual_pct()`
  because iDRAC ignores duty-cycle raws while automatic control is enabled; the
  fake's recorded command list makes that ordering directly assertable — write the
  assertion as one sequence check, not two membership checks.
- **Frozen-settings discipline carries over from T3**: `replace_settings()` swaps
  a reference; nothing anywhere assigns into a `Settings` field. This is what lets
  the T7 run loop call `watcher.changed()` → `watcher.reload()` →
  `controller.replace_settings(...)` between ticks without locking — the worst a
  concurrent read can see is one tick of the previous config.
- **The controller stays file-blind**: if a reviewer asks why `config_error`
  appears in a controller-level event list when the watcher lives in T7 — the
  controller emits `config_error` only for curves rejected by *its own*
  `replace_settings()` validation; TOML/mtime failures surface in T7's loop around
  `watcher.reload()` and are logged there. Case 12 exercises the former.
- Use `uvx` for ruff/ty per T1's note; the system-wide tools are stale.

