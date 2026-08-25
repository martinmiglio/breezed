"""Opt-in Prometheus metrics: shared state, text renderer, loopback HTTP server.

Metrics are diagnostic sugar (SPEC locked decision): the server binds 127.0.0.1
only and a bind failure degrades to metrics-less operation instead of crashing
fan control.
"""

import logging
import threading
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from breezed.types import FanPercent, OperatingMode, TempC

_CONTENT_TYPE = "text/plain; version=0.0.4; charset=utf-8"


@dataclass
class MetricsState:
    """Mutable BY DECISION: one Lock makes update-all and snapshot-all atomic

    so scrapes never observe torn state. Counters only ever increase.
    """

    _lock: threading.Lock = field(default_factory=threading.Lock, repr=False)
    temp_c: TempC | None = None
    fan_percent: FanPercent | None = None
    mode: OperatingMode = OperatingMode.UNKNOWN
    ipmi_errors_total: int = 0
    polls_total: int = 0

    def record_poll(self, temp_c: TempC, fan_percent: FanPercent, mode: OperatingMode) -> None:
        with self._lock:
            self.temp_c = temp_c
            self.fan_percent = fan_percent
            self.mode = mode
            self.polls_total += 1

    def record_ipmi_error(self) -> None:
        with self._lock:
            self.ipmi_errors_total += 1

    def render(self) -> str:
        """Exact SPEC five-line block; gauge lines omitted before first poll."""
        with self._lock:
            temp_c = self.temp_c
            fan_percent = self.fan_percent
            mode = str(self.mode)
            ipmi_errors_total = self.ipmi_errors_total
            polls_total = self.polls_total
        lines = []
        if temp_c is not None:
            lines.append(f'breezed_temp_c{{sensor="cpu_max"}} {temp_c}')
        if fan_percent is not None:
            lines.append(f"breezed_fan_percent {fan_percent}")
        lines.append(f'breezed_mode{{mode="{mode}"}} 1')
        lines.append(f"breezed_ipmi_errors_total {ipmi_errors_total}")
        lines.append(f"breezed_polls_total {polls_total}")
        return "\n".join(lines) + "\n"


def make_metrics_handler(state: MetricsState) -> type[BaseHTTPRequestHandler]:
    """Bind a BaseHTTPRequestHandler subclass to the shared MetricsState."""

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            if self.path in ("/metrics", "/"):
                body = state.render().encode()
                self.send_response(200)
                self.send_header("Content-Type", _CONTENT_TYPE)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            else:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()

    return Handler


class _MetricsServer(ThreadingHTTPServer):
    daemon_threads = True


def start_metrics_server(port: int, state: MetricsState) -> ThreadingHTTPServer | None:
    """127.0.0.1 bind (hard-coded), daemon_threads=True, serve_forever on a

    daemon thread; returns None on bind OSError (degraded-but-running).
    """
    try:
        server = _MetricsServer(("127.0.0.1", port), make_metrics_handler(state))
    except OSError:
        logging.getLogger("breezed").warning(
            "metrics server could not bind 127.0.0.1:%s; continuing without metrics",
            port,
        )
        return None
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


__all__ = ["MetricsState", "make_metrics_handler", "start_metrics_server"]
