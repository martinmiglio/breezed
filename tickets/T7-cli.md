# T7 — CLI

## Goal

Implement the user-facing Typer application in `src/breezed/cli.py` exactly per SPEC
"CLI shape": five commands (`run`, `set`, `auto`, `status`, `validate`), the locked
exit-code contract (`0` ok, `1` runtime error such as `IpmiError`, `2` usage/config
error such as `ConfigError` or out-of-range PCT), the foreground daemon loop with
signal handling (SIGINT/SIGTERM stop, SIGHUP hot-reload via T3's `ConfigWatcher`
feeding T5's `replace_settings()`), graceful-shutdown AUTO restore, opt-in metrics
server wiring, and machine output as JSON everywhere (SPEC locked decision: no third
format). Fully tested via `tests/test_cli.py` driving `typer.testing.CliRunner`
against injected fake clients — no monkeypatching, no real subprocesses, no sleeps.

## Depends on

(T1–T6)

## Files

- `src/breezed/cli.py` (new) — the whole ticket: the `app = typer.Typer(...)` entry
  point already declared in `pyproject.toml` (`breezed = "breezed.cli:app"`), the
  five commands, the dependency seam (task 2), the run loop (task 4), and the
  exit-code mapping helper (task 3). Imports from `breezed.config`
  (`ConfigError`, `Settings`, `load_settings`, `ConfigWatcher`), `breezed.ipmi`
  (`IpmiClient`, `IpmiError`, `TempReader`, `FanCommander`),
  `breezed.controller` (`Controller`), `breezed.logs`
  (`LoggingEventSink`, `setup_logging`), `breezed.metrics`
  (`MetricsState`, `start_metrics_server`), `breezed.curve` (`interpolate`),
  `breezed.types` (`FanPercent`, `make_fan_pct`). Third-party: `typer` only
  (already in `pyproject.toml` — **do not touch `pyproject.toml`**).
- `tests/test_cli.py` (new) — all CLI tests below, built on `CliRunner` plus the
  fakes defined in-file at top of module (T5 conventions: no monkeypatching, no
  sleeps).
- Do **not** modify any other file. If ruff/ty flag anything, fix the code, not
  the config.

## Tasks

1. Define the command surface exactly per SPEC (names, flags, defaults are
   normative):

   ```
   breezed run    [--config/-c PATH]  [--metrics-port INT]  [-v/--verbose]
   breezed set    <PCT>
   breezed auto
   breezed status [--config PATH]
   breezed validate <PATH> [--probe]
   ```

   - `run`: `--config`/`-c` defaults to `"breezed.toml"`; `--metrics-port INT`
     overrides `settings.metrics_port` when given (absent flag ⇒ fall back to the
     config value, which may be `None` ⇒ no server); `-v/--verbose` flips logging
     to human format.
   - `set`: `PCT` is a required `int` argument; outside 1–100 ⇒ usage error,
     exit 2 (do this explicitly — see Notes; do not rely on Typer range handling).
   - `auto`: no options.
   - `status`: `--config PATH` defaults to `"breezed.toml"`.
   - `validate`: `PATH` required positional; `--probe` optional flag.
   - Register a no-op `@app.callback()` only if needed for global options — none
     exist, so prefer no callback (see Notes on callback ordering gotchas).
   - Set `app = typer.Typer(add_completion=False, pretty_exceptions_enable=False)`
     so tracebacks never leak the iDRAC password into terminal output (extends
     T4's redaction contract to the CLI layer).

2. Define the single injection seam — **decision: a module-level `AppDeps`
   dataclass instance**, not monkeypatching, not Typer callbacks-with-hidden-state:

   ```python
   ClientFactory = Callable[[Settings], TempReader & FanCommander]  # structural;
   # spell the type as Callable[[Settings], IpmiClient] — IpmiClient satisfies
   # both Protocols structurally, and callers depend on the Protocol shapes.

    @dataclass(frozen=True)
    class AppDeps:
        build_client: Callable[[Settings], IpmiClient]
        sleep_interruptible: Callable[[threading.Event, float], bool]

    def _default_deps() -> AppDeps:
        return AppDeps(
            build_client=lambda settings: IpmiClient(settings),
            sleep_interruptible=lambda stop, timeout: stop.wait(timeout),
        )

    deps = _default_deps()
    ```

    (Module-level functions, not lambdas in the class body — dataclass defaults
    of callable type must come from a factory or constructor call anyway, and
    inline lambdas assigned as class attributes trip ruff E731. Constructing
    via `_default_deps()` keeps the seam swappable.)

   Commands always go through `deps.build_client(settings)` /
   `deps.sleep_interruptible(...)`. Tests override by constructing a fresh
   `AppDeps` with fake callables and assigning it (`cli_module_under_test`'s
   `deps` attribute replaced by plain assignment inside a fixture that restores
   the original afterwards — assignment-to-explicit-seam, never
   `monkeypatch.setattr` on foreign modules, mirroring T4/T5's
   constructor-injection philosophy: the seam is a *value the caller owns*, here
   the test owns the module's `deps` binding). Rationale: one seam covers both
   "no subprocess in tests" (fake client factory) and "no wall-clock waiting in
   the run-loop test" (fake wait callable), keeping `run` fully exercisable.

3. Implement the exit-code contract centrally — one helper every command uses:

   ```python
   def _fail(err: Exception, *, code: int) -> NoReturn: ...
   ```

   Prints the message to **stderr** (plain text, prefixed `breezed:` — errors are
   for humans; stdout stays reserved for JSON/machine output) and raises
   `typer.Exit(code=code)`. Mapping rules, enforced by dedicated tests:

   | Failure | Exit |
   |---|---|
   | `ConfigError` anywhere (load, validate, probe prep) | 2 |
   | PCT argument outside 1–100 | 2 |
   | `IpmiError` anywhere | 1 |
   | success | 0 |

   Catch narrowly per site (`except ConfigError`, `except IpmiError`) — never a
   bare `except Exception` that would swallow bugs as exit 1.

4. Implement `run` as the daemon loop, in this exact order:

   1. `setup_logging(config.verbose)` first (so even pre-config failures land in
      the chosen format), then `load_settings(path)` — `ConfigError` ⇒ emit
      nothing further, `_fail(..., code=2)`.
   2. Resolve metrics port (`--metrics-port` wins over `settings.metrics_port`);
      if non-`None`, build the shared `MetricsState` and call
      `start_metrics_server(port, state)` keeping the returned handle for
      `.shutdown()` in step 6 (a `None` return means degraded-but-running, per
      T6's contract — continue normally).
   3. Build client via `deps.build_client(settings)`; sink =
      `LoggingEventSink()`; `controller = Controller(client, client, settings,
      sink)` (one object satisfies both Protocol params — T5's shape).
   4. Register signal handlers **only when running in the main thread** (guard
      with `threading.current_thread() is threading.main_thread()`, wrapped so
      `ValueError` from non-main-thread registration degrades to "signals
      disabled" — this keeps the run loop testable under `CliRunner`): SIGINT +
      SIGTERM ⇒ `stop_event.set()`; SIGHUP ⇒ `reload_requested.set()`.
   5. Emit `startup` via the sink, then loop:

      ```python
      while not deps.sleep_interruptible(stop_event, settings.poll_interval_s):
          controller.tick()
          if state is not None:
              state.record_poll(...)  # from controller-visible values, or
              # record_ipmi_error() when the tick failed — track via the sink
              # wrapper (task 5 note) or a small adapter counting ipmi_error events
          if reload_requested.is_set():
              reload_requested.clear()
              if watcher.changed():
                  try:
                      new_settings = watcher.reload()
                  except ConfigError as err:
                      emit config_error(error=str(err))  # last good kept
                   else:
                       if controller.replace_settings(new_settings):
                           emit config_reload(...)  # only on accepted reload;
                           # a False return means the controller already emitted
                           # config_error and kept last-good (T5 contract)
      ```

      `watcher = ConfigWatcher(path)` is constructed right after the initial
      successful load. The interruptible sleep is the loop heartbeat — Ctrl-C
      must cut a 10 s wait short immediately (that is why the sleep goes through
      `stop_event.wait`, never `time.sleep`).
   6. In `finally:` call `controller.shutdown()` (best-effort AUTO restore, T5
      guarantees it swallows `IpmiError`), then shut down the metrics server if
      one was started, then emit `shutdown` via the sink. Re-raise nothing —
      after `finally`, fall off the end ⇒ exit 0.

5. Implement the one-shot commands — **they use the `FanCommander` capability
   directly and must NOT construct a `Controller`** (no state machine exists for
   a single shot):

   - `set(pct)`: validate `1 <= pct <= 100` else `_fail(..., code=2)`;
     `client = deps.build_client(load_settings(config_path))`;
     `client.disable_auto()` then `client.set_manual_pct(make_fan_pct(pct))`
     (same ordering rule as T5 task 4: iDRAC ignores duty-cycle raws while auto
     is enabled). Print a one-line JSON confirmation to stdout:
     `{"event": "speed_change", "fan_pct": pct}`. `IpmiError` ⇒ exit 1.
   - `auto()`: same construction; `client.enable_auto()`; print
     `{"event": "mode_change", "to": "auto"}`. `IpmiError` ⇒ exit 1.
   - Both accept no `--verbose`/logging setup beyond silencing: they should not
     call `setup_logging` (keep one-shot output purely the result JSON on
     stdout).

6. Implement `status(config_path)`:

   - Load settings (`ConfigError` ⇒ 2), build client, perform exactly one
     `read_max_cpu_temp()` and one `read_fan_rpms()`, compute
     `interpolate(settings.curve, temp)`.
   - Default (non-verbose): print **one JSON object** to stdout:

     ```json
     {"temp_c": 63, "fan_rpms": [["FAN_1", 4320]], "target_pct": null}
     ```

     keys exactly `temp_c`, `fan_rpms`, `target_pct` (`null` when at/above
     curve top ⇒ AUTO would trigger). This is the machine/agent snapshot per
     SPEC's locked agent-output decision. `IpmiError` ⇒ exit 1.
   - Under `--verbose`: pretty human text instead (e.g. aligned table lines
     `CPU max: 63C`, one line per fan, `Curve target: 12% (manual)` /
     `(auto — above curve)`). Verbose status still exits 0/1 identically.

7. Implement `validate(path, probe)`:

   - Always: `load_settings(path)`; on success print summary JSON to stdout:

     ```json
     {"valid": true, "path": "...", "host": "...", "poll_interval_s": 10,
      "curve_points": 4, "metrics_port": null}
     ```

     `ConfigError` ⇒ print `{"valid": false, "error": "<message>"}` and exit 2
     (the JSON goes to stdout, the exit code carries the verdict — scripts can
     consume either).
   - With `--probe`: additionally `deps.build_client(settings)`, one live
     `read_max_cpu_temp()` (`IpmiError` ⇒ exit 1, after the valid:true summary
     has been adjusted — probe failure makes the overall result invalid-for-use:
     emit `{"valid": true, "probe": {"ok": false, "error": ...}}` and exit 1),
     then extend the JSON with
     `"probe": {"ok": true, "temp_c": 63, "target_pct": 8, "would_auto": false}`
     where `would_auto` is `target_pct is None`.
   - Probe reads sensors only — it must never issue fan commands.

8. Write `tests/test_cli.py`. Harness at top of file (T5 conventions):

   - `FakeClient` — implements `TempReader` + `FanCommander` structurally:
     scriptable `temps: list[TempC]`, records `commands: list[str]`
     (`"auto"`, `"manual"`, `"set:12"`), optionally `raise_on_read: IpmiError |
     None`.
   - `deps` override fixture: saves `cli.deps`, installs
     `AppDeps(build_client=lambda s: fake_client, sleep_interruptible=fake_wait)`
     where `fake_wait` pops from a scripted list of booleans (first call returns
     `True` ⇒ loop body skipped or executed N times, then stops) and restores
     afterwards. No `pytest.monkeypatch`, no `setattr` on foreign modules — the
     assignment targets the documented seam only.
   - Fixture TOML written via `tmp_path` (reuse the shape of
     `tests/fixtures/config_valid.toml` inline; do not depend on T3's fixture
     file staying stable).
   - Runner: `runner = CliRunner()`; invoke via
     `runner.invoke(app, [...], catch_exceptions=False)` for exit-code tests so
     assertion failures surface directly (see Notes on `CliRunner` exception
     semantics); leave default behavior where a test asserts on
     `result.exception`.

   Named tests:

   - `test_help_lists_all_five_commands` — `--help` output contains `run`,
     `set`, `auto`, `status`, `validate` (this doubles as the SPEC's help
     snapshot; assert command names + the key flags `--config`, `-c`,
     `--metrics-port`, `--verbose`, `--probe` appear).
   - `test_set_valid_pct_issues_manual_then_speed_commands` — recorded order is
     exactly `["manual", "set:40"]`; stdout JSON confirms `fan_pct`.
   - `test_set_out_of_range_pct_exits_2` — `0`, `101`, negative; stderr mentions
     range; exit code 2.
   - `test_auto_enables_auto_mode` — commands == `["auto"]`, exit 0.
   - `test_status_outputs_documented_json_schema` — parse stdout JSON; assert
     key set is exactly `{temp_c, fan_rpms, target_pct}`, types correct
     (`temp_c: int`, `fan_rpms: list[[str, int]]`, `target_pct: int | None`).
   - `test_status_above_curve_reports_null_target` — temp above top point ⇒
     `target_pct is None`.
   - `test_status_verbose_is_not_json` — human text, `json.loads` fails.
   - `test_status_config_error_exits_2` — missing/broken TOML path.
   - `test_status_ipmi_error_exits_1` — `raise_on_read` set; password string
     absent from combined output (redaction discipline extends to CLI).
   - `test_validate_valid_config_prints_summary_exit_0`.
   - `test_validate_invalid_config_prints_valid_false_exits_2`.
   - `test_validate_probe_reads_live_temp_and_reports_target` — fake temp 52 on
     the default-style curve ⇒ `target_pct == 8`-style interpolation, `ok: true`,
     `would_auto: false`; assert zero fan commands were issued.
   - `test_validate_probe_above_curve_flags_auto` — `would_auto: true`,
     `target_pct: null`.
   - `test_run_config_error_exits_2_without_touching_fans` — bad config path;
     fake client records zero commands (startup decision: no read ⇒ no touch).
   - `test_run_ticks_controller_and_stops_via_stop_event` — scripted
     `sleep_interruptible` yields two ticks then stops; fake temps drive manual
     entry; assert `commands` sequence and process exit 0; assert
     `enable_auto` appears last (graceful restore) when final state was manual.
   - `test_run_sighup_reload_picks_up_new_curve` — second scripted iteration
     flips the reload flag with a rewritten TOML (later mtime via `os.utime`);
     next tick's interpolated pct reflects the new curve; a broken rewrite keeps
     last-good (commands continue on old curve, `config_error` surfaced).
   - `test_run_ipmi_failures_force_auto_then_shutdown_restores_auto` — limit
     reached during run; verify exactly one forced-`enable_auto` mid-run and the
     `finally` path emits shutdown restore without crashing exit 0.

9. Run the verification commands below; also run `uvx ruff format .` before
   checking.
10. Update README "Status" line noting T7 complete (one line change).

## Acceptance criteria

- [ ] `src/breezed/cli.py` exposes `app` (Typer instance wired as the
      `breezed` console script) and the `deps: AppDeps` seam via `__all__`;
      imports limited to stdlib, `typer`, and `breezed.*` modules listed above
- [ ] All five commands match SPEC names/options/defaults exactly: `run
      [--config/-c breezed.toml] [--metrics-port INT] [-v]`, `set <PCT>`,
      `auto`, `status [--config PATH]`, `validate <PATH> [--probe]`
- [ ] Exit-code contract holds for every failure class: `ConfigError` and
      out-of-range PCT ⇒ 2; `IpmiError` ⇒ 1; success ⇒ 0; error text goes to
      stderr, JSON results to stdout
- [ ] `run` loop: `setup_logging` → config load (exit 2 on `ConfigError`) →
      optional metrics server (`--metrics-port` overrides config; bind failure
      degrades per T6) → `Controller` over the built client → SIGINT/SIGTERM set
      the stop event, SIGHUP drives `ConfigWatcher.changed()/reload()` →
      `replace_settings()` with last-good preserved on failure → poll-interval
      sleep is interruptible (`Event.wait`) → `finally` performs best-effort
      `controller.shutdown()` and metrics-server teardown
- [ ] `set`/`auto` issue `FanCommander` calls directly (disable-auto-before-set
      ordering in `set`) and never construct a `Controller`
- [ ] `status` prints exactly `{temp_c, fan_rpms, target_pct}` JSON by default
      (`null` target above curve top); `--verbose` renders human text instead;
      identical exit codes either way
- [ ] `validate` prints the documented summary JSON; `--probe` additionally does
      one sensor read via the built client, reports `temp_c`, interpolated
      `target_pct`, and `would_auto`, and issues zero fan commands
- [ ] Tests inject fakes exclusively through the `deps` seam (assignment of a
      fresh `AppDeps`); no monkeypatching, no real subprocesses, no sleeps
      anywhere in `tests/test_cli.py`
- [ ] `tests/test_cli.py` includes, at minimum:
  - [ ] `test_help_lists_all_five_commands`
  - [ ] `test_set_valid_pct_issues_manual_then_speed_commands`
  - [ ] `test_set_out_of_range_pct_exits_2`
  - [ ] `test_auto_enables_auto_mode`
  - [ ] `test_status_outputs_documented_json_schema`
  - [ ] `test_status_above_curve_reports_null_target`
  - [ ] `test_status_verbose_is_not_json`
  - [ ] `test_status_config_error_exits_2`
  - [ ] `test_status_ipmi_error_exits_1` (asserts password absent from output)
  - [ ] `test_validate_valid_config_prints_summary_exit_0`
  - [ ] `test_validate_invalid_config_prints_valid_false_exits_2`
  - [ ] `test_validate_probe_reads_live_temp_and_reports_target`
  - [ ] `test_validate_probe_above_curve_flags_auto`
  - [ ] `test_run_config_error_exits_2_without_touching_fans`
  - [ ] `test_run_ticks_controller_and_stops_via_stop_event`
  - [ ] `test_run_sighup_reload_picks_up_new_curve` (incl. broken-reload
        keeps-last-good case)
  - [ ] `test_run_ipmi_failures_force_auto_then_shutdown_restores_auto`
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest tests/test_cli.py -v`
  - [ ] `uv run pytest` (full suite still green alongside T1–T6)
  - [ ] `uvx ty check` (zero errors, zero ignores)
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`

## Notes

- **No callback unless needed**: SPEC defines zero global options, so skip
  `@app.callback()`. Typer evaluates callbacks before command options and a
  stray eager callback complicates `CliRunner` invocation order and help output;
  add one only if a future ticket introduces shared flags — and then keep them
  *global* (on the callback), because Typer rejects mixing an option name across
  callback and command.
- **`CliRunner` vs `SystemExit`**: `typer.Exit(2)` becomes `result.exit_code ==
  2` cleanly, but an *uncaught* exception inside a command makes Click mark exit
  1 and stash the traceback in `result.exception` — silently converting your
  carefully-mapped exit 2 into a 1. Hence: (a) all expected failures flow
  through `_fail()` raising `typer.Exit`, never bare `raise`; (b) exit-code
  tests invoke with `catch_exceptions=False` so unexpected exceptions fail the
  test loudly instead of masquerading as a passing "exit 1".
- **Explicit PCT validation**: don't lean on Typer/Click range annotations for
  `set <PCT>` — Click's own usage errors exit 2, which happens to match, but the
  message wouldn't name the allowed range consistently and `int` coercion of
  e.g. `--` edge inputs gets murky. One `if not 1 <= pct <= 100: _fail(...)`
  keeps the contract in our hands and directly testable.
- **Signal handlers only in the main thread**: `signal.signal()` raises
  `ValueError` otherwise — under `CliRunner` (or any future thread embedding)
  registration must degrade gracefully, not crash `run`. The guard also
  documents that systemd (T8) runs breezed in the main thread where signals
  work. SIGHUP is registered best-effort too (absent on some platforms — wrap in
  the same guard).
- **Reload flag vs doing work inside the handler**: handlers only set
  `threading.Event`s. All reload logic lives in the loop between ticks — a
  handler that called `watcher.reload()` would run arbitrary code (including
  `IpmiError`s and logging lock acquisition) in signal-context, risking
  re-entrant deadlock on the logging lock.
- **Metrics updates ride the loop, not the controller** (T6's contract):
  `record_poll`/`record_ipmi_error` happen next to `tick()`. If getting
  tick-outcome info into the loop feels contorted, the sanctioned shape is a
  thin `EventSink` wrapper that forwards to `LoggingEventSink` and flips
  `MetricsState` counters on `poll`/`ipmi_error` events — do not add public
  accessors to `Controller` for this.
- **One-shot commands bypass logging entirely**: `set`/`auto` print their result
  JSON and nothing else; calling `setup_logging` there would interleave JSON log
  lines with the command's own JSON output and break consumers parsing stdout.
  Errors still go to stderr via `_fail`.
- **Password hygiene ends at the CLI**: `pretty_exceptions_enable=False` keeps
  rich tracebacks (which could embed local variables, including the password
  inside `IpmiClient`) out of the terminal; `_fail` prints only
  `str(exception)`, which T4 guarantees is redacted. Never log or echo argv.
- **`status`/`validate --probe` read-only guarantee**: neither may issue fan
  commands even accidentally — the fake-client `commands` assertions in their
  tests pin this (a regression that builds a `Controller` in these paths fails
  those tests immediately).
- Use `uvx` for ruff/ty per T1's note; the system-wide tools are stale.
