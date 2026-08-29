"""Tests for breezed.metrics: MetricsState rendering, monotonic counters, HTTP server.

No sleeps: servers bind synchronously on port 0.
"""

import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer

import pytest

from breezed.adapters.metrics import (
    MetricsState,
    make_metrics_handler,
    start_metrics_server,
)
from breezed.domain.types import FanPercent, OperatingMode, TempC

EXPECTED_METADATA = (
    "# HELP breezed_temp_c Current maximum CPU temperature in degrees Celsius.\n"
    "# TYPE breezed_temp_c gauge\n"
    "# HELP breezed_fan_percent Last commanded manual fan speed percentage.\n"
    "# TYPE breezed_fan_percent gauge\n"
    "# HELP breezed_mode Current breezed operating mode.\n"
    "# TYPE breezed_mode gauge\n"
    "# HELP breezed_ipmi_errors_total Total IPMI errors observed.\n"
    "# TYPE breezed_ipmi_errors_total counter\n"
    "# HELP breezed_polls_total Total successful temperature polls.\n"
    "# TYPE breezed_polls_total counter\n"
)

EXPECTED_BLOCK = EXPECTED_METADATA + (
    'breezed_temp_c{sensor="cpu_max"} 63\n'
    "breezed_fan_percent 12\n"
    'breezed_mode{mode="manual"} 1\n'
    "breezed_ipmi_errors_total 0\n"
    "breezed_polls_total 42\n"
)


def populated_state() -> MetricsState:
    state = MetricsState()
    state.temp_c = TempC(63)
    state.fan_percent = FanPercent(12)
    state.mode = OperatingMode.MANUAL
    state.ipmi_errors_total = 0
    state.polls_total = 42
    return state


def test_render_exact_documented_lines_from_populated_state() -> None:
    assert populated_state().render() == EXPECTED_BLOCK


def test_mode_label_reflects_state() -> None:
    state = MetricsState()
    assert 'breezed_mode{mode="unknown"} 1' in state.render()
    state.record_poll(TempC(60), FanPercent(8), OperatingMode.AUTO)
    assert 'breezed_mode{mode="auto"} 1' in state.render()
    state.record_poll(TempC(61), FanPercent(9), OperatingMode.MANUAL)
    assert 'breezed_mode{mode="manual"} 1' in state.render()
    state.mode = OperatingMode.UNKNOWN
    rendered = state.render()
    assert 'breezed_mode{mode="unknown"} 1' in rendered
    assert "auto" not in rendered


def test_render_omits_gauge_lines_before_first_poll() -> None:
    state = MetricsState()
    rendered = state.render()
    assert rendered == EXPECTED_METADATA + (
        'breezed_mode{mode="unknown"} 1\nbreezed_ipmi_errors_total 0\nbreezed_polls_total 0\n'
    )
    assert 'breezed_temp_c{sensor="cpu_max"}' not in rendered
    assert "\nbreezed_fan_percent " not in rendered


def test_counters_are_monotonic_ints() -> None:
    state = MetricsState()
    history: list[tuple[int, int]] = []
    for step in range(10):
        if step % 3 == 2:
            state.record_ipmi_error()
        else:
            state.record_poll(TempC(50 + step), FanPercent(6), OperatingMode.MANUAL)
        history.append((state.polls_total, state.ipmi_errors_total))
    polls = [p for p, _ in history]
    errors = [e for _, e in history]
    assert polls == [1, 2, 2, 3, 4, 4, 5, 6, 6, 7]
    assert errors == [0, 0, 1, 1, 1, 2, 2, 2, 3, 3]


def test_metrics_server_serves_documented_endpoints_on_loopback() -> None:
    state = populated_state()
    server = start_metrics_server(0, state)
    assert server is not None
    try:
        assert server.daemon_threads is True
        assert server.server_address[0] == "127.0.0.1"
        base = f"http://127.0.0.1:{server.server_address[1]}"
        with urllib.request.urlopen(f"{base}/metrics", timeout=2) as response:
            assert response.status == 200
            assert response.headers["Content-Type"] == ("text/plain; version=0.0.4; charset=utf-8")
            assert response.read().decode() == EXPECTED_BLOCK
        with urllib.request.urlopen(f"{base}/", timeout=2) as response:
            assert response.status == 200
            assert response.read().decode() == EXPECTED_BLOCK
        with pytest.raises(urllib.error.HTTPError) as exc_info:
            urllib.request.urlopen(f"{base}/nope", timeout=2)
        assert exc_info.value.code == 404
    finally:
        server.shutdown()
        server.server_close()


def test_start_metrics_server_returns_none_on_bind_failure() -> None:
    occupied = ThreadingHTTPServer(("127.0.0.1", 0), make_metrics_handler(MetricsState()))
    try:
        assert start_metrics_server(occupied.server_port, MetricsState()) is None
    finally:
        occupied.server_close()


def test_record_poll_updates_all_fields_atomically() -> None:
    state = MetricsState()
    state.record_poll(TempC(70), FanPercent(18), OperatingMode.MANUAL)
    assert state.temp_c == 70
    assert state.fan_percent == 18
    assert state.mode == OperatingMode.MANUAL
    assert state.polls_total == 1
