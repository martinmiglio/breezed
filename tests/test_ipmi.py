"""IPMI client tests: SPEC ipmi cases 1-5 plus fan-rpm and exact-argv coverage."""

import subprocess
from pathlib import Path

import pytest

from breezed.config import Settings
from breezed.ipmi import IpmiClient, IpmiError
from breezed.ports import FanCommander, TempReader
from breezed.types import TempC, make_fan_pct

FIXTURES = Path(__file__).parent / "fixtures"
PASSWORD = "hunter2-secret"


def make_settings() -> Settings:
    return Settings(host="169.254.0.1", user="root", password=PASSWORD, curve=())


class FakeRunner:
    """Records argv; pops one canned response per call."""

    def __init__(self, *responses: subprocess.CompletedProcess[str]) -> None:
        self.calls: list[list[str]] = []
        self.responses = list(responses)

    def __call__(self, args: list[str]) -> subprocess.CompletedProcess[str]:
        self.calls.append(list(args))
        return self.responses.pop(0)


def ok(stdout: str) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(args=[], returncode=0, stdout=stdout, stderr="")


def make_client(runner: FakeRunner) -> IpmiClient:
    return IpmiClient(make_settings(), runner=runner)


def test_parses_sdr_fixture_to_max_cpu_temp() -> None:
    runner = FakeRunner(ok((FIXTURES / "sdr_temperature.txt").read_text()))
    assert make_client(runner).read_max_cpu_temp() == TempC(108)


def test_read_fan_rpms_parses_sdr_fixture() -> None:
    runner = FakeRunner(ok((FIXTURES / "sdr_fan.txt").read_text()))
    assert make_client(runner).read_fan_rpms() == [
        ("FAN_1", 4320),
        ("FAN_2", 4320),
        ("FAN_3", 4080),
        ("FAN_4", 0),
    ]


def test_unparseable_nonempty_sdr_raises() -> None:
    runner = FakeRunner(ok("sensor gibberish without any addresses\n"))
    with pytest.raises(IpmiError):
        make_client(runner).read_max_cpu_temp()


def test_empty_output_with_zero_rc_raises_ipmi_error() -> None:
    runner = FakeRunner(ok(""))
    with pytest.raises(IpmiError, match="empty output"):
        make_client(runner).read_max_cpu_temp()


def test_nonzero_exit_code_raises_ipmi_error_with_stderr_snippet() -> None:
    runner = FakeRunner(
        subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="",
            stderr="unable to establish IPMI v2 connection\n",
        )
    )
    with pytest.raises(
        IpmiError, match=r"rc=1.*unable to establish IPMI v2 connection"
    ) as exc_info:
        make_client(runner).read_max_cpu_temp()
    assert PASSWORD not in str(exc_info.value)


def test_commands_emit_exact_argv() -> None:
    prefix = [
        "/usr/bin/ipmitool",
        "-I",
        "lanplus",
        "-H",
        "169.254.0.1",
        "-U",
        "root",
        "-P",
        PASSWORD,
    ]
    auto_runner, manual_runner, speed_runner, hex_runner = (
        FakeRunner(ok("ok\n")),
        FakeRunner(ok("ok\n")),
        FakeRunner(ok("ok\n")),
        FakeRunner(ok("ok\n")),
    )
    client_auto, client_manual, client_speed, client_hex = (
        make_client(auto_runner),
        make_client(manual_runner),
        make_client(speed_runner),
        make_client(hex_runner),
    )
    client_auto.enable_auto()
    client_manual.disable_auto()
    client_speed.set_manual_pct(make_fan_pct(100))
    client_hex.set_manual_pct(make_fan_pct(10))
    assert auto_runner.calls == [[*prefix, "raw", "0x30", "0x30", "0x01", "0x01"]]
    assert manual_runner.calls == [[*prefix, "raw", "0x30", "0x30", "0x01", "0x00"]]
    assert speed_runner.calls == [[*prefix, "raw", "0x30", "0x30", "0x02", "0xff", "0x64"]]
    assert hex_runner.calls == [[*prefix, "raw", "0x30", "0x30", "0x02", "0xff", "0x0a"]]


def test_password_never_appears_in_raised_messages_even_when_stderr_echoes_it() -> None:
    hostile = subprocess.CompletedProcess(
        args=[],
        returncode=1,
        stdout="",
        stderr=f"-P {PASSWORD} rejected by host 169.254.0.1\nsecond line {PASSWORD}\n",
    )
    runner = FakeRunner(hostile)
    with pytest.raises(IpmiError) as exc_info:
        make_client(runner).enable_auto()
    assert PASSWORD not in str(exc_info.value)
    assert PASSWORD not in repr(exc_info.value)


def test_client_structurally_satisfies_ports_without_inheriting() -> None:
    client = make_client(FakeRunner(ok("ok\n")))
    assert isinstance(client, TempReader)
    assert isinstance(client, FanCommander)
    assert TempReader not in IpmiClient.__mro__
    assert FanCommander not in IpmiClient.__mro__
