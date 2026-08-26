# T4 — IPMI client

## Goal

Implement the only module allowed to touch `subprocess`: an `IpmiClient` **adapter**
per SPEC's ports-and-adapters decision. The interfaces it satisfies live in a new
`src/breezed/ports.py` (`TempReader`, `FanCommander` Protocols) so the dependency
arrow points at the port — a future Redfish/SSH/BMC client implements the same
shapes and drops in via constructor injection with zero changes elsewhere.
It wraps `ipmitool -I lanplus -H {host} -U {user} -P {pass} <args>` with a timeout,
parses `sdr type temperature` output (locked regex `(?<=0Eh\|0Fh).+(\d{2})`, max CPU
temp → `TempC`) and `sdr type fan` output (name/rpm pairs for the T7 status command),
issues raw fan commands (auto enable/disable, hex-encoded manual percentage), and
raises a single `IpmiError(DomainError)` whose message is **always redacted** of the
iDRAC password (SPEC ipmi test case 5). Fully tested against canned SDR fixtures
covering SPEC ipmi cases 1–5.

## Depends on

(T1–T3)

## Files

- `src/breezed/ports.py` (new) — the app's interface vocabulary: `TempReader`
  and `FanCommander` runtime-checkable `Protocol`s, declared here unconditionally
  (adapters depend on ports, never the reverse). **T5 will append `SpeedPolicy`
  to this module in its own ticket** — that is the plan of record, not a
  coordination hedge; do not create a module-local duplicate. Stdlib-only.
- `src/breezed/ipmi.py` (new) — the adapter: `IpmiError`,
  `Runner` type alias + default subprocess
  runner, `IpmiClient`, SDR parsers. Imports from `breezed.types`
  (`DomainError`, `TempC`, `FanPercent`), `breezed.config`
  (`Settings` — read-only, for host/user/password/ipmitool_path), and
  `breezed.ports` (`TempReader`, `FanCommander`). Stdlib-only
  beyond that (`subprocess`, `re`, `typing`); no third-party imports.
- `tests/test_ipmi.py` (new) — exactly SPEC ipmi cases 1–5 below, behavior-named like
  T2/T3 tests.
- `tests/fixtures/sdr_temperature.txt` (new) — captured-style R620/R720
  `ipmitool sdr type temperature` output: several sensor rows (ambient `0Eh`, CPU
  temp rows at `0Fh`, exhaust/inlet), multi-digit temps, so MAX-picking is actually
  exercised (the max must come from a `0Fh` row, not ambient).
- `tests/fixtures/sdr_fan.txt` (new) — captured-style `ipmitool sdr type fan` output:
  `FAN_1 | 30h | ok | 7.20 | 4320 RPM` style rows across both fan redundancy groups.
- Do **not** touch `pyproject.toml` — no new dependencies. If ruff/ty flag anything,
  fix the code, not the config.

## Tasks

1. Define `IpmiError(DomainError)` in `ipmi.py`. Every raise goes through it; messages
   carry a short human context plus an optional stderr **snippet**, never full command
   lines or env (see task 6 for the redaction contract).
2. Define the capability Protocols **in `ports.py`** (not ipmi.py — this is the
   feedback-driven swap point for future BMC clients):

   ```python
   class TempReader(Protocol):
       def read_max_cpu_temp(self) -> TempC: ...


   class FanCommander(Protocol):
       def enable_auto(self) -> None: ...
       def disable_auto(self) -> None: ...
       def set_manual_pct(self, pct: FanPercent) -> None: ...
   ```

   `IpmiClient` implements both (structural typing — do not inherit). T5's fake
   clients and T7's CLI wiring depend on these interfaces, not on `IpmiClient`.
   Add `read_fan_rpms() -> list[tuple[str, int]]` on `IpmiClient` itself (status-only;
   keep it off `FanCommander` so controller fakes stay minimal).
3. Define the single injection seam up front — a `Runner` callable:

   ```python
   Runner = Callable[..., subprocess.CompletedProcess[str]]
   ```

   `IpmiClient.__init__(self, settings: Settings, *, runner: Runner | None = None)`
   stores `runner or _default_runner`, where `_default_runner` calls
   `subprocess.run(..., capture_output=True, text=True, encoding="utf-8",
   errors="replace", timeout=15)` with `check=False` (returncode handled explicitly).
   **Tests inject a stub runner via this keyword — they never `monkeypatch
   subprocess`.** This keeps the seam explicit, typed under `ty`, and lets one fake
   script multiple command sequences (see Notes).
4. Implement `_run(args: Sequence[str]) -> str`: build argv
   `[ipmitool_path, "-I", "lanplus", "-H", host, "-U", user, "-P", password, *args]`,
   call the stored runner, then check results in order: non-zero returncode ⇒
   `IpmiError(f"ipmitool {' '.join(args)} failed (rc={rc}): {stderr snippet}")`;
   returncode 0 but empty/whitespace stdout ⇒ `IpmiError("ipmitool {args}: empty
   output")` — empty-but-successful output must still raise (SPEC case 2 path).
   Timeouts surface as `subprocess.TimeoutExpired`; catch and re-raise as `IpmiError`.
5. Implement parsing:
   - `read_max_cpu_temp() -> TempC`: run `["sdr", "type", "temperature"]`, apply
     compiled `re.compile(r"(?<=0Eh|0Fh).+(\d{2})", re.MULTILINE)` per line, collect
     all matches' 2-digit captures, take the **max** as `int` → `TempC`. Zero matches
     on non-empty output ⇒ `IpmiError` (unparseable SDR, don't guess).
   - `read_fan_rpms() -> list[tuple[str, int]]`: run `["sdr", "type", "fan"]`, parse
     rows like `<name> | <addr>h | <state> | ... | <rpm> RPM` (split on `|`, last
     numeric field before/at `RPM`); skip unparseable rows rather than failing — fan
     status is display data, not control input.
6. Implement commands via `_run(["raw", ...])`:
   - auto: `raw 0x30 0x30 0x01 0x01`
   - manual: `raw 0x30 0x30 0x01 0x00`
   - speed: `raw 0x30 0x30 0x02 0xff 0x{pct:02x}` — **lowercase, zero-padded two
     digits** via `format(pct, "02x")` (`set_manual_pct(make_fan_pct(10))` ⇒
     `... 0x02 0xff 0x0a`) so every percentage renders as a stable two-hex-digit
     byte token per SPEC case 4. Accept `FanPercent` only; construct callers'
     values through `make_fan_pct()`.
   - **Redaction contract (SPEC case 5)**: the password appears *only* inside
     `_run`'s local argv list. No exception message, log line, repr, or debug string
     may contain it — error messages use the ipmitool subcommand args only (which are
   password-free), never the full argv. Write one dedicated test asserting the
   password string appears nowhere in any raised message even when stderr echoes it.
7. Write `tests/test_ipmi.py` covering exactly the five SPEC cases below as separate
   tests, driving `IpmiClient` with an injected fake `Runner` returning canned
   `CompletedProcess[str]` objects built from the fixtures (read fixture files with
   `(Path(__file__).parent / "fixtures" / ...).read_text()`):
   - case 1 uses `sdr_temperature.txt`, expects the known max `0Fh` temp;
   - add a companion test using `sdr_fan.txt` for `read_fan_rpms()` (expected name/rpm
     tuples);
   - add a test pinning the exact argv for each command (auto/manual/speed), including
     `-I lanplus -H/-U/-P` placement and hex formatting.
8. Run the verification commands below; also run `uvx ruff format .` before checking.
9. Update README "Status" line noting T4 complete (one line change).

## Acceptance criteria

- [ ] `src/breezed/ports.py` exports `TempReader`, `FanCommander` via `__all__`;
      `src/breezed/ipmi.py` exports `IpmiError`, `Runner`, `IpmiClient` via
      `__all__`; ipmi imports stdlib + `breezed.types` + `breezed.config.Settings`
      + `breezed.ports` only
- [ ] Dependency direction holds: `ports.py` imports nothing from `breezed.ipmi`
      (grep-checkable); adapters depend on ports, never the reverse
- [ ] `TempReader`/`FanCommander` are runtime-checkable-style `Protocol`s as specced;
      `IpmiClient` structurally satisfies both without inheriting them
- [ ] Subprocess boundary confined to `_default_runner` + `_run`: fixed argv shape
      `[path, -I lanplus -H host -U user -P pass, args...]`, 15s timeout,
      `capture_output=True`, decoded text with `errors="replace"`
- [ ] Non-zero rc ⇒ `IpmiError` including a stderr snippet; rc 0 with empty stdout ⇒
      `IpmiError` too; `TimeoutExpired` converted to `IpmiError`
- [ ] SDR parsing uses the locked regex with `re.MULTILINE`, takes the **max** matched
      two-digit value across all `0Eh`/`0Fh` rows, returns `TempC`; no matches ⇒
      `IpmiError`
- [ ] Commands emit exact raw byte sequences: auto `0x30 0x30 0x01 0x01`, manual
      `0x30 0x30 0x01 0x00`, speed `0x30 0x30 0x02 0xff 0x{pct:02x}` (10 ⇒ `0x0a`,
      lowercase zero-padded)
- [ ] Password never appears in any exception message, argv echo, or log record —
      enforced by a dedicated test (SPEC case 5)
- [ ] Tests inject fakes exclusively via the `runner=` constructor keyword; no
      `monkeypatch.setattr(subprocess, ...)` anywhere
- [ ] `tests/test_ipmi.py` implements SPEC ipmi cases, one test each:
  - [ ] 1. Parses canned R620/R720-style SDR output fixture → max CPU temp
  - [ ] 2. Empty SDR output raises `IpmiError` (even though rc == 0)
  - [ ] 3. Non-zero exit code raises `IpmiError` including stderr snippet
  - [ ] 4. Speed command formats percentage as hex byte (10 ⇒ `0x0a`)
  - [ ] 5. Password never appears in any raised exception message/log record
- [ ] Bonus coverage present: `read_fan_rpms()` against `sdr_fan.txt`; exact-argv
      assertions for all three commands
- [ ] All verification commands pass clean:
  - [ ] `uv run pytest tests/test_ipmi.py -v`
  - [ ] `uv run pytest` (full suite still green alongside T1–T3)
  - [ ] `uvx ty check` (zero errors, zero ignores)
  - [ ] `uvx ruff check .`
  - [ ] `uvx ruff format --check .`

## Notes

- **Injection seam decision**: constructor-injected `runner` callable (task 3), not
  monkeypatching. Monkeypatching `subprocess.run` is global, untyped under `ty`, and
  hides the seam; a `Runner` alias documents the exact call signature, allows
  sequencing (a fake can pop canned responses per call to simulate read→command
  flows), and matches how T5 injects whole fake clients anyway. Keep `subprocess.run`
  referenced only inside `_default_runner`.
- **bytes vs str**: always decode in the runner layer (`text=True` +
  `encoding="utf-8"` + `errors="replace"`). iDRACs occasionally emit stray bytes in
  SDR tables; a hard decode failure would masquerade as an IPMI outage. Never parse
  bytes downstream — parsers see clean `str`.
- **Regex gotchas**: `(?<=0Eh|0Fh)` needs the alternation inside the lookbehind
  parentheses (fixed-width alternatives are fine); without `re.MULTILINE`, `.+` will
  not anchor per-row behavior the way tests expect when combined with line-oriented
  iteration. The trailing `(\d{2})` grabs the last two digits before end-of-line-ish
  context — verify against the real fixture that it captures the *temp*, not an RPM
  column or sensor ID; adjust anchoring (e.g. `\s*$`) if the fixture proves ambiguous.
  Compile once at module level.
- **Empty output ≠ success**: `ipmitool` can exit 0 with empty stdout on some network
  failures — treat empty stdout as `IpmiError` regardless of rc, or T5's failure
  counter never triggers.
- **This stays the only module importing `subprocess` within T1–T7** — SPEC's
  "no `Any` except at the subprocess boundary" clause lives here. T8's
  `DaemonInstaller` is the one sanctioned second site (systemd control is
  inherently process supervision); no other module may spawn processes.
- **Max, not first**: the locked curve-driver decision is max CPU temp across all
  matching rows; R720s report two CPU packages. The fixture must contain at least two
  distinct `0Fh` temps so picking the wrong row fails the test.
- **Password hygiene mirrors T3**: redact by construction (never build strings from
  full argv), not by scrubbing after the fact. A `repr()` of the client or an
  accidental f-string of argv would leak it — the case 5 test asserts against a
  deliberately hostile stderr containing the password.
- Use `uvx` for ruff/ty per T1's note; the system-wide tools are stale.

## Draft interfaces (for review)

> DRAFT for human review — sketches, not implementations. `TempReader`/`FanCommander`
> spellings here are canonical: T5's constructor params, T5/T7 fakes, and T7's
> one-shot commands all reference these exact shapes.

```python
# src/breezed/ports.py
from typing import Protocol, runtime_checkable

from breezed.types import FanPercent, TempC  # noqa: compile-check strips


@runtime_checkable
class TempReader(Protocol):
    def read_max_cpu_temp(self) -> TempC: ...


@runtime_checkable
class FanCommander(Protocol):
    def enable_auto(self) -> None: ...
    def disable_auto(self) -> None: ...
    def set_manual_pct(self, pct: FanPercent) -> None: ...


# NOTE: this ticket ships only the two Protocols above; T5 appends SpeedPolicy to
# this same module (plan of record — ports.py owns all Protocols).
__all__ = ["TempReader", "FanCommander"]

# src/breezed/ipmi.py
import subprocess
from collections.abc import Callable, Sequence

from breezed.config import Settings  # read-only: host/user/password/ipmitool_path
from breezed.types import DomainError, FanPercent, TempC  # noqa: compile-check strips


class IpmiError(DomainError):
    """Messages carry short context + optional stderr snippet; never full argv

    (password redaction is by construction — see task 6 / SPEC case 5).
    """


# The single injection seam; tests pass stub runners via the `runner=` keyword.
Runner = Callable[..., subprocess.CompletedProcess[str]]


def _default_runner(args: Sequence[str]) -> str:
    """subprocess.run(..., capture_output=True, text=True, encoding="utf-8",

    errors="replace", timeout=15, check=False); returns stdout.
    """
    ...


class IpmiClient:
    """Structurally satisfies TempReader + FanCommander — never inherits them."""

    def __init__(self, settings: Settings, *, runner: Runner | None = None) -> None: ...

    # TempReader port
    def read_max_cpu_temp(self) -> TempC: ...
    # FanCommander port
    def enable_auto(self) -> None: ...
    def disable_auto(self) -> None: ...
    def set_manual_pct(self, pct: FanPercent) -> None: ...
    # status-only display data; deliberately OFF FanCommander so controller fakes
    # stay minimal (task 2)
    def read_fan_rpms(self) -> list[tuple[str, int]]: ...


__all__ = ["IpmiError", "Runner", "IpmiClient"]
```
