"""T7 CLI tests: CliRunner + fakes injected through the deps seam only.

No monkeypatching, no real subprocesses, no sleeps. SIGHUP-driven reload tests
send the real signal (run registers handlers in the main thread) and restore
the previous handlers afterwards.
"""

import json
import os
import re
import signal
import threading
from collections.abc import Callable
from pathlib import Path
from typing import cast

import pytest
from typer.testing import CliRunner

from breezed import cli
from breezed.cli import AppDeps, ClientFactory, app
from breezed.ipmi import IpmiError
from breezed.types import FanPercent, TempC

PASSWORD = "hunter2-supersecret"

VALID_TOML = f"""\
[settings]
host = "169.254.0.1"
user = "svc-breeze"
password = "{PASSWORD}"

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
"""

GOOD_RELOAD_TOML = """\
[settings]
host = "169.254.0.1"
user = "svc-breeze"

[[curve]]
temp_c = 45
fan_pct = 6

[[curve]]
temp_c = 49
fan_pct = 8

[[curve]]
temp_c = 60
fan_pct = 12

[[curve]]
temp_c = 74
fan_pct = 18
"""

BROKEN_TOML = "[[curve\ntemp_c = oops"


class FakeClient:
    def __init__(
        self,
        temps: list[TempC] | None = None,
        *,
        raise_on_read: IpmiError | None = None,
    ) -> None:
        self.temps = list(temps) if temps is not None else []
        self.commands: list[str] = []
        self.raise_on_read = raise_on_read

    def read_max_cpu_temp(self) -> TempC:
        if self.raise_on_read is not None:
            raise self.raise_on_read
        if not self.temps:
            return TempC(40)
        return self.temps.pop(0)

    def read_fan_rpms(self) -> list[tuple[str, int]]:
        return [("FAN_1", 4320)]

    def enable_auto(self) -> None:
        self.commands.append("auto")

    def disable_auto(self) -> None:
        self.commands.append("manual")

    def set_manual_pct(self, pct: FanPercent) -> None:
        self.commands.append(f"set:{pct}")


class FakeWait:
    def __init__(self, script: list[bool]) -> None:
        self.script = list(script)

    def __call__(self, _stop_event: threading.Event, _timeout: float) -> bool:
        if not self.script:
            return True
        return self.script.pop(0)


def scripted_wait(
    actions: dict[int, Callable[[], None]], script: list[bool]
) -> Callable[[threading.Event, float], bool]:
    state = {"call": 0}

    def wait(_stop_event: threading.Event, _timeout: float) -> bool:
        state["call"] += 1
        action = actions.get(state["call"])
        if action is not None:
            action()
        if not script:
            return True
        return script.pop(0)

    return wait


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def config_dir(tmp_path: Path):
    original = Path.cwd()
    os.chdir(tmp_path)
    yield tmp_path
    os.chdir(original)


@pytest.fixture
def install_deps():
    original = cli.deps

    def install(
        client: FakeClient,
        sleep_interruptible: Callable[[threading.Event, float], bool],
    ) -> FakeClient:
        build_client = cast(ClientFactory, lambda _settings: client)
        cli.deps = AppDeps(build_client=build_client, sleep_interruptible=sleep_interruptible)
        return client

    yield install
    cli.deps = original


@pytest.fixture
def install_client(install_deps):
    def install_client_fn(client: FakeClient) -> FakeClient:
        return install_deps(client, FakeWait([]))

    return install_client_fn


def write_config(config_dir: Path, name: str = "breezed.toml", body: str = VALID_TOML) -> Path:
    path = config_dir / name
    path.write_text(body)
    return path


def _bump(path: Path, offset_ns: int) -> None:
    stamp = os.stat(path).st_mtime_ns + 1_000_000 + offset_ns
    os.utime(path, ns=(stamp, stamp))


def _hup_after_write(path: Path, body: str, offset_ns: int) -> None:
    path.write_text(body)
    _bump(path, offset_ns)
    os.kill(os.getpid(), signal.SIGHUP)


def log_events(output: str) -> list[dict[str, object]]:
    events = []
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return events


ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def test_help_lists_all_five_commands(runner: CliRunner):
    combined = ""
    for argv in (
        ["--help"],
        ["run", "--help"],
        ["status", "--help"],
        ["validate", "--help"],
    ):
        result = runner.invoke(app, argv, catch_exceptions=False)
        assert result.exit_code == 0
        combined += ANSI_RE.sub("", result.output)
    for name in ("run", "set", "auto", "status", "validate"):
        assert name in combined
    for flag in ("--config", "-c", "--metrics-port", "--verbose", "--probe"):
        assert flag in combined


def test_set_valid_pct_issues_manual_then_speed_commands(
    runner: CliRunner, config_dir: Path, install_client
):
    write_config(config_dir)
    client = install_client(FakeClient())
    result = runner.invoke(app, ["set", "40"], catch_exceptions=False)
    assert result.exit_code == 0
    assert client.commands == ["manual", "set:40"]
    payload = json.loads(result.stdout)
    assert payload == {"event": "speed_change", "fan_pct": 40}


def test_set_out_of_range_pct_exits_2(runner: CliRunner, config_dir: Path, install_client):
    write_config(config_dir)
    client = install_client(FakeClient())
    for pct in ("0", "101"):
        result = runner.invoke(app, ["set", pct])
        assert result.exit_code == 2
        assert "1..100" in result.stderr
    assert client.commands == []
    result = runner.invoke(app, ["set", "-5"])
    assert result.exit_code == 2
    assert client.commands == []


def test_auto_enables_auto_mode(runner: CliRunner, config_dir: Path, install_client):
    write_config(config_dir)
    client = install_client(FakeClient())
    result = runner.invoke(app, ["auto"], catch_exceptions=False)
    assert result.exit_code == 0
    assert client.commands == ["auto"]
    assert json.loads(result.stdout) == {"event": "mode_change", "to": "auto"}


def test_status_outputs_documented_json_schema(runner: CliRunner, config_dir: Path, install_client):
    write_config(config_dir)
    install_client(FakeClient(temps=[TempC(63)]))
    result = runner.invoke(app, ["status"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert set(payload) == {"temp_c", "fan_rpms", "target_pct"}
    assert isinstance(payload["temp_c"], int)
    assert payload["temp_c"] == 63
    assert payload["fan_rpms"] == [["FAN_1", 4320]]
    assert isinstance(payload["target_pct"], int)
    assert payload["target_pct"] == 10


def test_status_above_curve_reports_null_target(
    runner: CliRunner, config_dir: Path, install_client
):
    write_config(config_dir)
    install_client(FakeClient(temps=[TempC(90)]))
    result = runner.invoke(app, ["status"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert set(payload) == {"temp_c", "fan_rpms", "target_pct"}
    assert payload["target_pct"] is None


def test_status_verbose_is_not_json(runner: CliRunner, config_dir: Path, install_client):
    write_config(config_dir)
    install_client(FakeClient(temps=[TempC(63)]))
    result = runner.invoke(app, ["status", "--verbose"], catch_exceptions=False)
    assert result.exit_code == 0
    with pytest.raises(json.JSONDecodeError):
        json.loads(result.stdout)
    assert "CPU max" in result.stdout
    assert "Curve target" in result.stdout


def test_status_config_error_exits_2(runner: CliRunner, config_dir: Path):
    result = runner.invoke(app, ["status", "--config", "missing.toml"])
    assert result.exit_code == 2
    assert "missing.toml" in result.stderr


def test_status_ipmi_error_exits_1(runner: CliRunner, config_dir: Path, install_client):
    write_config(config_dir)
    install_client(FakeClient(raise_on_read=IpmiError("sdr failed")))
    result = runner.invoke(app, ["status"])
    assert result.exit_code == 1
    assert PASSWORD not in result.output
    assert PASSWORD not in result.stdout
    assert PASSWORD not in result.stderr


def test_validate_valid_config_prints_summary_exit_0(
    runner: CliRunner, config_dir: Path, install_client
):
    path = write_config(config_dir)
    install_client(FakeClient())
    result = runner.invoke(app, ["validate", str(path)], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["valid"] is True
    assert payload["path"] == str(path)
    assert payload["host"] == "169.254.0.1"
    assert payload["poll_interval_s"] == 10
    assert payload["curve_points"] == 4
    assert payload["metrics_port"] is None


def test_validate_invalid_config_prints_valid_false_exits_2(
    runner: CliRunner, config_dir: Path, install_client
):
    path = write_config(config_dir, body=BROKEN_TOML)
    install_client(FakeClient())
    result = runner.invoke(app, ["validate", str(path)])
    assert result.exit_code == 2
    payload = json.loads(result.stdout)
    assert payload["valid"] is False
    assert "error" in payload


def test_validate_probe_reads_live_temp_and_reports_target(
    runner: CliRunner, config_dir: Path, install_client
):
    path = write_config(config_dir)
    client = install_client(FakeClient(temps=[TempC(52)]))
    result = runner.invoke(app, ["validate", str(path), "--probe"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["valid"] is True
    probe = payload["probe"]
    assert probe == {"ok": True, "temp_c": 52, "target_pct": 7, "would_auto": False}
    assert client.commands == []


def test_validate_probe_above_curve_flags_auto(runner: CliRunner, config_dir: Path, install_client):
    path = write_config(config_dir)
    client = install_client(FakeClient(temps=[TempC(90)]))
    result = runner.invoke(app, ["validate", str(path), "--probe"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["probe"]["ok"] is True
    assert payload["probe"]["target_pct"] is None
    assert payload["probe"]["would_auto"] is True
    assert client.commands == []


def test_run_config_error_exits_2_without_touching_fans(
    runner: CliRunner, config_dir: Path, install_client
):
    client = install_client(FakeClient())
    result = runner.invoke(app, ["run", "--config", "nope.toml"])
    assert result.exit_code == 2
    assert client.commands == []


def test_run_ticks_controller_and_stops_via_stop_event(
    runner: CliRunner, config_dir: Path, install_deps
):
    write_config(config_dir)
    client = install_deps(FakeClient(temps=[TempC(50), TempC(55)]), FakeWait([False, False]))
    result = runner.invoke(app, ["run"], catch_exceptions=False)
    assert result.exit_code == 0
    assert client.commands == ["manual", "set:7", "auto"]
    events = log_events(result.stdout)
    shutdowns = [e for e in events if e.get("event") == "shutdown"]
    assert len(shutdowns) == 1
    startups = [e for e in events if e.get("event") == "startup"]
    assert len(startups) == 1


def test_run_sighup_reload_picks_up_new_curve(runner: CliRunner, config_dir: Path, install_deps):
    cfg = write_config(config_dir)

    def apply_good_reload() -> None:
        _hup_after_write(cfg, GOOD_RELOAD_TOML, 1)

    def apply_broken_reload() -> None:
        _hup_after_write(cfg, BROKEN_TOML, 2)

    client = install_deps(
        FakeClient(temps=[TempC(50), TempC(52), TempC(52), TempC(52)]),
        scripted_wait(
            {2: apply_good_reload, 4: apply_broken_reload},
            [False, False, False, False],
        ),
    )
    saved = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        result = runner.invoke(app, ["run"], catch_exceptions=False)
    finally:
        for sig, handler in saved.items():
            signal.signal(sig, handler)
    assert result.exit_code == 0
    assert client.commands == ["manual", "set:7", "set:9", "auto"]
    events = log_events(result.stdout)
    names = [e.get("event") for e in events]
    assert "config_reload" in names
    assert "config_error" in names


def test_run_ipmi_failures_force_auto_then_shutdown_restores_auto(
    runner: CliRunner, config_dir: Path, install_deps
):
    write_config(config_dir)
    client = install_deps(
        FakeClient(raise_on_read=IpmiError("sdr failed")),
        FakeWait([False, False, False]),
    )
    result = runner.invoke(app, ["run"], catch_exceptions=False)
    assert result.exit_code == 0
    forced = [
        e
        for e in log_events(result.stdout)
        if e.get("event") == "mode_change"
        and e.get("to") == "auto"
        and e.get("reason") == "read_failures"
    ]
    assert len(forced) == 1
    assert client.commands.count("auto") == 1
