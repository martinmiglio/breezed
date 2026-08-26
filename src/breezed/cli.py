"""Typer CLI: run/set/auto/status/validate plus the locked exit-code contract.

0 ok, 1 runtime error (IpmiError), 2 usage/config error (ConfigError,
out-of-range PCT). Errors go to stderr prefixed "breezed:"; stdout stays
reserved for JSON/machine output. Kept free of module-level side effects beyond
``app`` and ``deps`` so T8 can mount ``app.add_typer(...)`` later.
"""

import json
import signal
import threading
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Annotated, NoReturn

import rich.console
import typer

from breezed.config import ConfigError, Settings, load_settings
from breezed.controller import Controller, EventSink
from breezed.curve import interpolate
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


def _install_signal_handlers(
    stop_event: threading.Event, reload_requested: threading.Event
) -> None:
    def stop(_signum: int, _frame: object) -> None:
        stop_event.set()

    def reload(_signum: int, _frame: object) -> None:
        reload_requested.set()

    try:
        if threading.current_thread() is not threading.main_thread():
            return
        signal.signal(signal.SIGINT, stop)
        signal.signal(signal.SIGTERM, stop)
        sighup = getattr(signal, "SIGHUP", None)
        if sighup is not None:
            signal.signal(sighup, reload)
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
    """Run the control loop until SIGINT/SIGTERM; SIGHUP hot-reloads the config."""
    setup_logging(verbose)
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
    reload_requested = threading.Event()
    _install_signal_handlers(stop_event, reload_requested)

    sink.emit(EventType.STARTUP)
    try:
        client = deps.build_client(current_settings)
        controller = Controller(client, client, current_settings, sink)
        while not deps.sleep_interruptible(stop_event, current_settings.poll_interval_s):
            controller.tick()
            if reload_requested.is_set():
                reload_requested.clear()
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
