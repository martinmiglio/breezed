# T6 — Observability

## Goal

Give breezed its two machine-facing surfaces per SPEC's locked decisions:

1. **Structured JSON logs on stdout** — one JSON object per line (`ts`, `level`,
   `logger`, `event`, plus arbitrary extra fields) by default, switching to a plain
   human format under `--verbose`, exposed as a `setup_logging(verbose: bool)`
   factory that T7's CLI calls once at startup.
2. **The real `EventSink`** — a stdlib-logging adapter behind T5's `EventSink`
   Protocol so every controller/daemon action lands in the log stream under the
   exact SPEC event names (`startup`, `poll`, `mode_change`, `speed_change`,
   `hysteresis_wait`, `config_reload`, `config_error`, `ipmi_error`, `shutdown`),
   with a test enforcing that no non-SPEC event name ever escapes.
3. **An opt-in Prometheus metrics server** — `src/breezed/metrics.py`: stdlib
   `ThreadingHTTPServer` bound to `127.0.0.1` on `--metrics-port`, serving
   `GET /metrics` and `GET /` with the exact SPEC metric lines, fed by a small
   thread-safe `MetricsState` shared with the controller loop.

No third-party dependencies: `json`/`logging`/`http.server` only. The agent-facing
contract stays exactly as locked — logs are the machine output; there is no third
format.

## Depends on

(T1–T5)

## Files

- `src/breezed/logs.py` (new) — `SPEC_EVENT_NAMES`, `JsonLogFormatter`,
  `LoggingEventSink`, `setup_logging(verbose: bool) -> None`.
  Imports stdlib (`json`, `logging`, `datetime`) only — **no** breezed imports
  needed here except the type of T5's seam if desired for a structural assert;
  do **not** import from `breezed.controller` (keep the dependency arrow pointing
  the other way: controller defines the Protocol, this module implements it).
- `src/breezed/metrics.py` (new) — `MetricsState` (with a `.render()` method),
  `start_metrics_server(port: int, state: MetricsState) -> ThreadingHTTPServer | None`.
  Imports stdlib (`http.server`, `threading`, `socketserver` transitively) plus
  `breezed.types` (`OperatingMode`). No third-party imports; **never** import
  `prometheus_client`.
- `tests/test_logging.py` (new) — formatter/sink tests driven by a `StringIO`
  handler attached to a fresh logger; no caplog, no monkeypatching.
- `tests/test_metrics.py` (new) — pure-state render tests plus one HTTP round-trip
  against a server bound to port 0 (ephemeral).
- Do **not** touch `pyproject.toml` — no new dependencies. If ruff/ty flag
  anything, fix the code, not the config.

## Tasks

1. In `src/breezed/logs.py`, define the closed vocabulary up front:

   ```python
   SPEC_EVENT_NAMES: frozenset[str] = frozenset({
       "startup", "poll", "mode_change", "speed_change", "hysteresis_wait",
       "config_reload", "config_error", "ipmi_error", "shutdown",
   })
   ```

   This is the single source of truth both the sink guard and the tests check
   against. Do not derive it from docstrings or duplicate it in metrics.

2. Implement `JsonLogFormatter(logging.Formatter)`:

   - One JSON object per line (trailing `\n`, no pretty-printing).
   - Required keys on every record: `ts` (UTC ISO-8601, second precision, `Z`
     suffix — e.g. `"2026-08-25T04:24:30Z"`; build from
     `datetime.now(UTC)` via `.isoformat()` with `+00:00` replaced by `Z`; this is
     *wall clock*, deliberately — unlike the controller's monotonic hysteresis
     clock, humans correlate these timestamps with iDRAC/SEL entries),
     `level` (`record.levelname`), `logger` (`record.name`), `event`.
   - Extra fields: read them out of `record.__dict__` — take everything that is
     **not** a standard `LogRecord` attribute (compute the exclusion set from a
     bare `LogRecord("x", 0, "p", 1, "m", None, None).__dict__.keys()`) plus the
     well-known `message`/`asctime` keys. This is the collision-safe approach:
     `extra={"temp_c": ...}` never fights `LogRecord`'s own fields, and no
     `data_*` prefix convention needs policing. Serialize with
     `json.dumps(..., default=str)` so `StrEnum` members (`OperatingMode`,
     `ControlState`) pass through as their lowercase string values directly
     (T2's note: no `.value` mapping layer).
   - If `event` is missing from the record dict (someone logged through this
     formatter without the sink), fall back to using the message itself as
     `event` rather than crashing the handler.

3. Implement `setup_logging(verbose: bool) -> None`:

   - Configures the root `breezed` logger (via `logging.getLogger("breezed")`)
     idempotently: clear existing handlers, attach one `StreamHandler` to
     `sys.stdout` (stdout, not stderr — SPEC locks "JSON to stdout").
   - `verbose=False` → `level=INFO`, handler formatter = `JsonLogFormatter`.
   - `verbose=True` → `level=DEBUG`, handler formatter = plain
     `logging.Formatter("%(asctime)s %(levelname)s %(message)s")`.
   - Never call `logging.basicConfig()` — it would leak config onto third-party
     loggers and fight T7's CLI re-entry in tests.

4. Implement the sink adapter:

   ```python
   class LoggingEventSink:
       def __init__(self, logger: logging.Logger | None = None) -> None: ...
       def emit(self, event: str, /, **fields: object) -> None: ...
   ```

   - Structurally satisfies T5's `EventSink` Protocol (keyword-only positional
     `event`, arbitrary keyword fields).
   - Guard clause: if `event not in SPEC_EVENT_NAMES`, raise `ValueError` naming
     the offender. Fail loud in development rather than silently shipping a typo'd
     event name into production logs; the runtime cost is one set lookup per
     emission.
   - Emission is a single call:
     `self._log.info(event, extra={"event": event, **fields})` — the message *is*
     the event name (so verbose/human mode still shows something meaningful), and
     the same name rides `extra` for the JSON formatter.
   - Do **not** import `Controller`/`ControlState` here; the adapter knows nothing
     about what emits.

5. Create `src/breezed/metrics.py` with `MetricsState` — **decision: a mutable
   dataclass guarded by one `threading.Lock`**, not atomic-per-field types.
   Justification: the five gauges/counters are updated together inside one poll
   step, and a scrape must never observe torn state (e.g. new `fan_percent` with
   last poll's `temp_c`, which would look like a real regression to whoever reads
   the graphs). A single lock makes "update all" and "snapshot all" trivially
   atomic; contention is negligible at one write per 10 s and one scrape per few
   seconds. Atomic fields (or a lock-free seqlock) would buy throughput nobody
   needs while reintroducing torn-read reasoning. Shape:

   ```python
   @dataclass
   class MetricsState:
       _lock: threading.Lock = field(default_factory=threading.Lock, repr=False)
       temp_c: TempC | None = None
       fan_percent: FanPercent | None = None
       mode: OperatingMode = OperatingMode.UNKNOWN
       ipmi_errors_total: int = 0
       polls_total: int = 0

       def record_poll(self, temp_c: TempC, fan_percent: FanPercent,
                       mode: OperatingMode) -> None: ...
       def record_ipmi_error(self) -> None: ...
       def render(self) -> str: ...
   ```

   - `record_*` mutate under the lock; counters are plain ints incremented only
     upward (monotonicity invariant, asserted in tests) — never decremented or
     reset, including on hot-reload or failure recovery.
   - `render()` snapshots everything under one lock acquisition, then formats
     outside it, producing **exactly** these lines (Prometheus text format,
     trailing newline, no `# HELP`/`# TYPE` — netdata doesn't need them and SPEC
     documents the five-line block verbatim):

     ```
     breezed_temp_c{sensor="cpu_max"} 63
     breezed_fan_percent 12
     breezed_mode{mode="manual"} 1
     breezed_ipmi_errors_total 0
     breezed_polls_total 42
     ```

   - Before the first successful poll (`temp_c is None`) omit the
     `breezed_temp_c` and `breezed_fan_percent` lines but still render
     `breezed_mode` (with `mode="unknown"` until the first decision) and both
     counters — scrapers get a stable line-count after warm-up and honest
     counters from process start.
   - `breezed_temp_c` renders the raw int value of `TempC` (NewType is erased at
     runtime); `breezed_mode`'s label comes from `str(self.mode)` — `StrEnum`
     interpolates directly, again no `.value` ceremony.

6. Add the server plumbing in the same module:

   ```python
   def start_metrics_server(port: int, state: MetricsState) -> None | None: ...
   ```

   - Build an `http.server.BaseHTTPRequestHandler` subclass (closure over
     `state`, or a class attribute — your call, keep ty happy) answering
     `GET /metrics` **and** `GET /` with `200`, `Content-Type:
     text/plain; version=0.0.4; charset=utf-8`, body `state.render().encode()`.
     Anything else → `404`. Only `do_GET` implemented.
   - Wrap in `ThreadingHTTPServer(("127.0.0.1", port), Handler)` with
     `daemon_threads = True` (set it on the subclass explicitly — the class
     default is False and a stuck scraper connection would otherwise block
     interpreter exit).
   - Bind **and** `serve_forever` on a `daemon=True` thread; return the server
     object (typed `ThreadingHTTPServer | None`) so T7 keeps a handle for tests
     and clean shutdown. Return `None` instead of raising on `OSError` from the
     bind (port taken, permissions): log the failure through the `breezed`
     logger (plain message, not an `EventSink` event — it isn't in the SPEC
     vocabulary) and let the daemon run metrics-less. Metrics are opt-in sugar;
     losing them must never take down fan control.
   - Bind address is hard-coded `127.0.0.1` — never expose fan-control metrics
     off-host (locked decision; do not accept a bind-host parameter).

7. Write `tests/test_logging.py`:

   - Build the harness at top of file: `make_logger()` attaching a
     `StreamHandler(StringIO())` with `JsonLogFormatter` to a fresh
     `logging.getLogger("breezed.test")`; helper `emit(sink_or_logger, ...)`.
   - Named tests:
     - `test_json_output_parses_per_line_with_required_keys` — emit several
       records (including a multi-field `mode_change` mirroring the SPEC example),
       split stdout capture on newlines, `json.loads` each line, assert `ts`,
       `level`, `logger`, `event` present on all and `ts` matches the
       `YYYY-MM-DDTHH:MM:SSZ` shape.
     - `test_extra_fields_surface_as_top_level_keys` —
       `emit("poll", temp_c=65, fan_pct=8, target_pct=8)` parses back with those
       keys/values at the top level (not nested, not prefixed).
     - `test_verbose_mode_is_not_json` — same emissions through the human
       formatter; assert lines match the `%(asctime)s %(levelname)s %(message)s`
       shape (regex) and that `json.loads` raises on the first line.
     - `test_sink_rejects_non_spec_event_names` — `pytest.raises(ValueError)` on
       `emit("polled")` (typo) and on a plausible-sounding invention like
       `metrics_scrape`.
     - `test_sink_emits_only_spec_event_names` — drive `LoggingEventSink` across
       every name in `SPEC_EVENT_NAMES`, collect the resulting records, assert
       `record.event ∈ SPEC_EVENT_NAMES` for each (the closed-vocabulary
       guarantee T5's consumers rely on).

8. Write `tests/test_metrics.py`:

   - Named tests:
     - `test_render_exact_documented_lines_from_populated_state` — populate
       `MetricsState` with the SPEC example values (63 °C, 12 %, manual,
       errors 0, polls 42), assert `render()` equals the exact five-line block
       above (string equality, including order and trailing newline).
     - `test_mode_label_reflects_state` — flip `mode` through
       `OperatingMode.AUTO` / `MANUAL` / `UNKNOWN` via `record_poll` /
       construction and assert the label changes accordingly
       (`breezed_mode{mode="auto"} 1` etc., always value `1`).
     - `test_render_omits_gauge_lines_before_first_poll` — fresh state renders
       three lines (mode unknown + both counters), zero for gauges.
     - `test_counters_are_monotonic_ints` — interleave successes/errors, assert
       counters only increase and remain `int` (no floats sneaking in).
     - `test_http_endpoints_serve_rendered_body` — bind on port 0
       (`ThreadingHTTPServer` directly, no need for the launcher's retry logic),
       request `/metrics` and `/` via `urllib.request`, assert status 200, the
       content-type header, and body == `state.render()`; assert `/nope` is 404.
   - No sleeps: start the server thread, poll-with-timeout (e.g. try connecting
     up to ~2 s in small increments) or simply rely on the OS accepting binds
     synchronously before `serve_forever` matters — prefer the bounded-retry
     helper over any `time.sleep` loop longer than milliseconds.

9. Run the verification commands below; also run `uvx ruff format .` before
   checking.
10. Update README "Status" line noting T6 complete (one line change).

## Acceptance criteria

- [ ] `src/breezed/logs.py` exports `SPEC_EVENT_NAMES`, `JsonLogFormatter`,
      `LoggingEventSink`, `setup_logging` via `__all__`; imports stdlib only
      (`json`, `logging`, `datetime`); no import from `breezed.controller`
- [ ] `src/breezed/metrics.py` exports `MetricsState` (including `.render()`) and
      `start_metrics_server` via `__all__`; imports stdlib + `breezed.types`
      only; no third-party dependencies added anywhere in this ticket
- [ ] Default logging emits one JSON object per line with required keys `ts`
      (UTC ISO-8601 `Z` form), `level`, `logger`, `event`; `extra` fields appear
      as top-level JSON keys with no collisions against `LogRecord` attributes
- [ ] `setup_logging(False)` → JSON/INFO on stdout; `setup_logging(True)` →
      `%(asctime)s %(levelname)s %(message)s`/DEBUG on stdout; repeated calls are
      idempotent (no duplicated handlers)
- [ ] `LoggingEventSink` satisfies T5's `EventSink` Protocol structurally, maps
      `emit(event, **fields)` onto the `breezed` logger, and raises `ValueError`
      on any event name outside `SPEC_EVENT_NAMES`
- [ ] Metrics server: `ThreadingHTTPServer` with `daemon_threads = True`, bound
      hard-coded to `127.0.0.1` on the opt-in port; `GET /metrics` and `GET /`
      return the Prometheus text block; other paths 404; bind `OSError` logs a
      warning and returns `None` instead of crashing the daemon
- [ ] `MetricsState` is one `Lock`-guarded dataclass (decision recorded in task 5);
      updates and render snapshots are each atomic; counters are monotonically
      increasing ints never reset
- [ ] `render()` reproduces the SPEC block byte-for-byte from the example state;
      mode label tracks `OperatingMode`; gauge lines omitted before first poll
- [ ] `tests/test_logging.py` includes, at minimum:
  - [ ] `test_json_output_parses_per_line_with_required_keys`
  - [ ] `test_extra_fields_surface_as_top_level_keys`
  - [ ] `test_verbose_mode_is_not_json`
  - [ ] `test_sink_rejects_non_spec_event_names`
  - [ ] `test_sink_emits_only_spec_event_names`
- [ ] `tests/test_metrics.py` includes, at minimum:
  - [ ] `test_render_exact_documented_lines_from_populated_state`
  - [ ] `test_mode_label_reflects_state`
  - [ ] `test_render_omits_gauge_lines_before_first_poll`
  - [ ] `test_counters_are_monotonic_ints`
  - [ ] `test_http_endpoints_serve_rendered_body`
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest tests/test_logging.py tests/test_metrics.py -v`
  - [ ] `uv run pytest` (full suite still green alongside T1–T5)
  - [ ] `uvx ty check` (zero errors, zero ignores)
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`

## Notes

- **`extra=` vs `LogRecord` attribute collisions**: `logging` merges `extra`
  into the record's `__dict__`, so `extra={"name": ...}` or `extra={"msg": ...}`
  silently clobbers record internals. The formatter therefore treats
  `record.__dict__` as the payload source and excludes a computed set of standard
  `LogRecord` keys rather than trusting callers to prefix fields with `data_`.
  Consequence: SPEC field names like `from`, `to`, `reason`, `error` are safe
  verbatim (they aren't record attributes); if someone ever adds a colliding key,
  the exclusion list wins and the JSON output drops it — acceptable, since the
  SPEC field vocabulary is fixed and enforced upstream by the sink's closed event
  set.
- **`ThreadingHTTPServer` threads must be daemonic**: the class attribute
  defaults to `False`; without `daemon_threads = True` a hung scraper socket
  keeps a worker alive and `SIGTERM` handling (T7/T8 systemd) stalls interpreter
  exit past the AUTO-restore deadline. Set it explicitly on the subclass even
  though the serving thread itself is also `daemon=True` — they guard different
  lifetimes.
- **Bind failure degrades, never crashes**: `start_metrics_server` catches
  `OSError` around socket creation/bind, logs a plain warning through the
  `breezed` logger, and returns `None`. Rationale: metrics are diagnostic sugar;
  a second breezed instance or an unrelated service squatting the port must not
  stop the fan-control loop. Do not route this warning through `EventSink` — it
  is not in the SPEC event vocabulary and the sink guard would (correctly)
  reject it.
- **Wall-clock `ts` vs the controller's monotonic clock**: deliberate split.
  Hysteresis math must be immune to NTP steps (T5's injected
  `time.monotonic`); log timestamps must be comparable to iDRAC SEL entries and
  journalctl, so they come from `datetime.now(UTC)`. Don't "fix" either side to
  match the other.
- **StrEnum flows straight into JSON**: T2 guaranteed `OperatingMode` (and T5's
  `ControlState`) serialize via `str()`; `json.dumps(..., default=str)` covers
  any other non-primitive field defensively, but don't lean on `default=str` for
  domain types — annotate fields properly so `ty` catches mistakes at the emit
  site, not in the formatter.
- **Metrics are written by T7's loop, not the controller**: keep
  `controller.py` untouched — it already emits everything observability-wise via
  its sink. T7 will call `state.record_poll(...)` / `record_ipmi_error()` next to
  `controller.tick()`. This ticket ships only the shared state, renderer, and
  server; wiring is out of scope.
- **No `# HELP`/`# TYPE` lines**: SPEC documents the exact five-line block, and
  the render test asserts string equality — adding Prometheus metadata would fail
  our own acceptance criteria and buy netdata nothing. If a future consumer needs
  metadata, change SPEC first, then the test, then the code — in that order.
- **Test hygiene carries over**: no sleeps, no monkeypatching, fakes built
  in-file at top of the test modules (T5 conventions). The HTTP test uses port 0
  (ephemeral) so parallel pytest runs never race for a fixed port.
- Use `uvx` for ruff/ty per T1's note; the system-wide tools are stale.
