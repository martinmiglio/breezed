"""Typer CLI: run/set/auto/status/validate plus the exit-code contract.

0 ok, 1 runtime error (IpmiError), 2 usage/config error (ConfigError,
out-of-range PCT). Errors go to stderr prefixed "breezed:"; stdout stays
reserved for JSON/machine output. Kept free of module-level side effects beyond
``app`` and ``deps`` so daemon_app can mount via ``app.add_typer(...)``.
"""

import getpass
import json
import logging
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
from collections.abc import Callable
from dataclasses import asdict, dataclass
from pathlib import Path
from traceback import format_exc
from typing import Annotated, NoReturn

import rich.console
import typer

from breezed import __version__
from breezed.config import ConfigError, Settings, load_settings
from breezed.controller import Controller, EventSink
from breezed.curve import interpolate
from breezed.daemon import (
    DaemonError,
    Step,
    StepOutcome,
    apply,
    build_remove_steps,
    build_steps,
    daemon_status,
    remove,
    stage_files,
)
from breezed.ipmi import IpmiClient, IpmiError
from breezed.logs import LoggingEventSink, setup_logging
from breezed.metrics import MetricsState, start_metrics_server
from breezed.types import EventType, FanPercent, OperatingMode, TempC, make_fan_pct
from breezed.watcher import ConfigWatcher

app = typer.Typer(add_completion=False, pretty_exceptions_enable=False)

ClientFactory = Callable[[Settings], IpmiClient]


@dataclass(frozen=True)
class AppDeps:
    build_client: ClientFactory
    sleep_interruptible: Callable[[threading.Event, float], bool]


deps = AppDeps(build_client=IpmiClient, sleep_interruptible=threading.Event.wait)

__all__ = ["app", "deps"]


def _print_version(value: bool) -> None:
    if value:
        print(f"breezed {__version__}")
        raise typer.Exit()


@app.callback()
def _root_callback(
    version: Annotated[
        bool,
        typer.Option("--version", callback=_print_version, is_eager=True, help="Show version."),
    ] = False,
) -> None:
    """Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI."""


def _fail(err: Exception, *, code: int) -> NoReturn:
    typer.secho(f"breezed: {err}", fg=typer.colors.RED, err=True)
    raise typer.Exit(code=code) from err


def _load_settings_or_fail(config: Path) -> Settings:
    try:
        return load_settings(config)
    except ConfigError as err:
        _fail(err, code=2)


def _connect(config: Path) -> tuple[Settings, IpmiClient]:
    try:
        settings = _load_settings_or_fail(config)
        return settings, deps.build_client(settings)
    except IpmiError as err:
        _fail(err, code=1)


class _MetricsSink:
    """Thin EventSink wrapper: forwards to LoggingEventSink, mirrors counters."""

    def __init__(self, base: EventSink, state: MetricsState | None) -> None:
        self._base = base
        self._state = state

    def emit(self, event: EventType, /, **fields: object) -> None:
        self._base.emit(event, **fields)
        if self._state is None:
            return
        if event is EventType.IPMI_ERROR:
            self._state.record_ipmi_error()
        elif event is EventType.POLL:
            temp_c = fields.get("temp_c")
            fan_pct = fields.get("fan_pct")
            mode = fields.get("mode")
            if isinstance(temp_c, int) and isinstance(mode, str):
                pct: FanPercent | None = FanPercent(fan_pct) if isinstance(fan_pct, int) else None
                self._state.record_poll(TempC(temp_c), pct, OperatingMode(mode))


def _install_signal_handlers(stop_event: threading.Event) -> None:
    def stop(_signum: int, _frame: object) -> None:
        stop_event.set()

    try:
        if threading.current_thread() is not threading.main_thread():
            return
        signal.signal(signal.SIGINT, stop)
        signal.signal(signal.SIGTERM, stop)
    except ValueError:
        pass


@app.command()
def run(
    config: Annotated[Path, typer.Option("--config", "-c")] = Path("breezed.toml"),
    metrics_port: Annotated[
        int | None, typer.Option("--metrics-port", help="Loopback port for the metrics endpoint")
    ] = None,
    verbose: Annotated[
        bool, typer.Option("-v", "--verbose", help="Plain-text debug logging instead of JSON")
    ] = False,
) -> None:
    """Run the control loop until SIGINT/SIGTERM."""
    setup_logging(verbose)
    try:
        _run_body(config, metrics_port)
    except typer.Exit:
        raise
    except Exception as err:
        logging.getLogger("breezed").error(
            "fatal",
            extra={
                "event": "fatal",
                "error": type(err).__name__,
                "detail": str(err),
                "traceback": format_exc(),
            },
        )
        raise


def _run_body(config: Path, metrics_port: int | None) -> None:
    settings = _load_settings_or_fail(config)
    watcher = ConfigWatcher(config)

    port = metrics_port if metrics_port is not None else settings.metrics_port
    state: MetricsState | None = None
    server = None
    if port is not None:
        state = MetricsState()
        server = start_metrics_server(port, state)

    sink = _MetricsSink(LoggingEventSink(), state)
    current_settings = settings
    controller = None

    stop_event = threading.Event()
    _install_signal_handlers(stop_event)

    sink.emit(EventType.STARTUP)
    try:
        client = deps.build_client(current_settings)
        controller = Controller(client, client, current_settings, sink)
        while not deps.sleep_interruptible(stop_event, current_settings.poll_interval_s):
            controller.tick()
            if watcher.changed():
                try:
                    new_settings = watcher.reload()
                except ConfigError as err:
                    sink.emit(EventType.CONFIG_ERROR, error=str(err))
                else:
                    if controller.replace_settings(new_settings):
                        current_settings = new_settings
                        sink.emit(EventType.CONFIG_RELOAD)
    finally:
        if controller is not None:
            controller.shutdown()
        if server is not None:
            server.shutdown()
        sink.emit(EventType.SHUTDOWN)


@app.command("set")
def set_speed(
    pct: Annotated[int, typer.Argument()],
    config: Annotated[Path, typer.Option("--config", "-c")] = Path("breezed.toml"),
) -> None:
    """Set a fixed manual fan percentage (1-100) until the next mode change."""
    if not 1 <= pct <= 100:
        err = ValueError(f"PCT must be in 1..100, got {pct}")
        _fail(err, code=2)
    _, client = _connect(config)
    try:
        client.disable_auto()
        client.set_manual_pct(make_fan_pct(pct))
    except IpmiError as err:
        _fail(err, code=1)
    print(json.dumps({"event": "speed_change", "fan_pct": pct}))


@app.command()
def auto(
    config: Annotated[Path, typer.Option("--config", "-c")] = Path("breezed.toml"),
) -> None:
    """Hand fan control back to the iDRAC's automatic policy."""
    _, client = _connect(config)
    try:
        client.enable_auto()
    except IpmiError as err:
        _fail(err, code=1)
    print(json.dumps({"event": "mode_change", "to": "auto"}))


@app.command()
def status(
    config: Annotated[Path, typer.Option("--config")] = Path("breezed.toml"),
    verbose: Annotated[bool, typer.Option("--verbose")] = False,
) -> None:
    """Print one-shot sensor status as JSON (human-readable with --verbose)."""
    settings, client = _connect(config)
    try:
        temp = client.read_max_cpu_temp()
        rpms = client.read_fan_rpms()
    except IpmiError as err:
        _fail(err, code=1)
    target = interpolate(settings.curve, temp)
    if verbose:
        console = rich.console.Console()
        console.print(f"CPU max: {temp}C", style="bold")
        for name, rpm in rpms:
            console.print(f"{name}: {rpm} RPM")
        if target is None:
            console.print("Curve target: auto (above curve)", style="cyan")
        else:
            console.print(f"Curve target: {target}% (manual)", style="cyan")
    else:
        print(json.dumps({"temp_c": temp, "fan_rpms": rpms, "target_pct": target}))


@app.command()
def validate(
    path: Annotated[Path, typer.Argument()],
    probe: Annotated[
        bool, typer.Option("--probe", help="Also read a live temperature from the iDRAC")
    ] = False,
) -> None:
    """Validate a config file and print a JSON summary; --probe adds a live read."""
    try:
        settings = load_settings(path)
    except ConfigError as err:
        print(json.dumps({"valid": False, "error": str(err)}))
        _fail(err, code=2)
    summary: dict[str, object] = {
        "valid": True,
        "path": str(path),
        "host": settings.host,
        "poll_interval_s": settings.poll_interval_s,
        "curve_points": len(settings.curve),
        "metrics_port": settings.metrics_port,
    }
    if probe:
        try:
            client = deps.build_client(settings)
            temp = client.read_max_cpu_temp()
        except IpmiError as err:
            summary["probe"] = {"ok": False, "error": str(err)}
            print(json.dumps(summary))
            _fail(err, code=1)
        target = interpolate(settings.curve, temp)
        summary["probe"] = {
            "ok": True,
            "temp_c": temp,
            "target_pct": target,
            "would_auto": target is None,
        }
    print(json.dumps(summary))


daemon_app = typer.Typer(add_completion=False, pretty_exceptions_enable=False)

_KEEP = ["/etc/breezed.env", "/etc/breezed"]


def _resolve_self() -> Path:
    return Path(os.path.realpath(sys.argv[0]))


def _resolve_sudo() -> str:
    sudo = shutil.which("sudo")
    if sudo is None:
        _fail(DaemonError("sudo not found; `daemon install` needs a single sudo prompt"), code=1)
    return sudo


def _resolve_uv() -> str:
    uv = shutil.which("uv")
    if uv is None:
        _fail(DaemonError("uv not found on PATH; install uv first"), code=1)
    return uv


def _print_step_plan(steps: list[Step], note: str) -> None:
    typer.secho(note, fg=typer.colors.YELLOW, err=True)
    for step in steps:
        typer.secho(f"  • {step.label}", fg=typer.colors.CYAN, bold=True, err=True)


def _print_results(results: list[tuple[str, StepOutcome]]) -> None:
    for label, outcome in results:
        if outcome is StepOutcome.SKIPPED:
            typer.secho(f"  · {label}", dim=True, err=True)
        else:
            typer.secho(f"  ✔ {label}", fg=typer.colors.GREEN, err=True)


def _run_sudo(argv: list[str]) -> None:
    completed = subprocess.run(argv, check=False)
    if completed.returncode != 0:
        raise typer.Exit(code=completed.returncode or 1)


@daemon_app.command("install")
def daemon_install(
    staging_dir: Annotated[
        Path | None, typer.Option("--staging-dir", help="Override the private staging directory")
    ] = None,
    source: Annotated[
        Path | None, typer.Option("--source", help="uv source spec (defaults to current checkout)")
    ] = None,
    dry_run: Annotated[
        bool, typer.Option("--dry-run", help="Print the privileged steps without escalating")
    ] = False,
) -> None:
    """Stage the unit/env/config, then install the system service via one sudo prompt."""
    resolved_source = str((source or Path.cwd()).resolve())
    resolved_uv = _resolve_uv()
    owner = getpass.getuser()
    stage_dir = staging_dir or Path(tempfile.mkdtemp(prefix="breezed-install-"))
    try:
        stage_files(stage_dir)
    except DaemonError as err:
        _fail(err, code=1)

    steps = build_steps(stage_dir, owner, resolved_uv, resolved_source)

    if os.geteuid() == 0:
        try:
            _print_results(apply(stage_dir, owner, resolved_uv, resolved_source))
        except DaemonError as err:
            _fail(err, code=1)
        print(json.dumps({"event": "install_complete"}))
        return

    _print_step_plan(steps, "The following privileged steps will run:")
    if dry_run:
        print(json.dumps({"event": "install_planned", "steps": [step.label for step in steps]}))
        return
    argv = [
        _resolve_sudo(),
        str(_resolve_self()),
        "daemon",
        "apply",
        "--staging-dir",
        str(stage_dir),
        "--owner",
        owner,
        "--uv",
        resolved_uv,
        "--source",
        resolved_source,
    ]
    _run_sudo(argv)
    print(json.dumps({"event": "install_complete"}))


@daemon_app.command("apply", hidden=True)
def daemon_apply(
    staging_dir: Annotated[Path, typer.Option()],
    owner: Annotated[str, typer.Option()],
    uv: Annotated[str, typer.Option()],
    source: Annotated[str, typer.Option()],
) -> None:
    """Internal privileged install step, invoked via sudo by `daemon install`."""
    try:
        results = apply(staging_dir, owner, uv, source)
    except DaemonError as err:
        _fail(err, code=1)
    _print_results(results)


@daemon_app.command("status")
def daemon_status_command() -> None:
    try:
        report = daemon_status()
    except DaemonError as err:
        _fail(err, code=1)
    print(json.dumps(asdict(report)))


@daemon_app.command("uninstall")
def daemon_uninstall(
    dry_run: Annotated[
        bool, typer.Option("--dry-run", help="Print the privileged steps without escalating")
    ] = False,
) -> None:
    """Stop and remove the service and runtime via one sudo prompt; keeps /etc/breezed.env/."""
    resolved_uv = _resolve_uv()

    steps = build_remove_steps(resolved_uv)

    if os.geteuid() == 0:
        try:
            _print_results(remove(resolved_uv))
        except DaemonError as err:
            _fail(err, code=1)
        print(json.dumps({"event": "uninstall_complete", "keeps": _KEEP}))
        return

    _print_step_plan(steps, "The following privileged steps will run:")
    if dry_run:
        print(
            json.dumps(
                {
                    "event": "uninstall_planned",
                    "keeps": _KEEP,
                    "steps": [step.label for step in steps],
                }
            )
        )
        return
    argv = [_resolve_sudo(), str(_resolve_self()), "daemon", "remove", "--uv", resolved_uv]
    _run_sudo(argv)
    print(json.dumps({"event": "uninstall_complete", "keeps": _KEEP}))


@daemon_app.command("remove", hidden=True)
def daemon_remove(
    uv: Annotated[str, typer.Option()],
) -> None:
    """Internal privileged uninstall step, invoked via sudo by `daemon uninstall`."""
    try:
        results = remove(uv)
    except DaemonError as err:
        _fail(err, code=1)
    _print_results(results)


app.add_typer(daemon_app, name="daemon")
