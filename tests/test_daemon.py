"""Daemon deployment staging, command planning, and status tests."""

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

import breezed
from breezed import cli
from breezed.cli import app
from breezed.daemon import (
    EXEC_PATH,
    DaemonError,
    DaemonStatus,
    InstallerPaths,
    daemon_status,
    install_commands,
    stage_files,
    uninstall_commands,
)

UNIT_NAME = "breezed.service"
STAMPED_UNIT = f"""\
# Installed by breezed 9.9.9 on 2026-01-01T00:00:00+00:00
[Service]
ExecStart={EXEC_PATH} run
"""


class FakeFileOps:
    def __init__(self, files: dict[str, str] | None = None) -> None:
        self.files = {Path(name): body for name, body in (files or {}).items()}

    def read_text(self, path: Path) -> str:
        return self.files[path]

    def stat(self, path: Path) -> os.stat_result | None:
        if path not in self.files:
            return None
        return os.stat_result((0o100644, 0, 0, 0, 0, 0, 0, 0, 0, 0))


class FakeRunner:
    def __init__(
        self,
        outputs: dict[str, str] | None = None,
        errors: dict[str, str] | None = None,
    ) -> None:
        self.argvs: list[list[str]] = []
        self.outputs = outputs or {}
        self.errors = errors or {}

    def __call__(self, argv: list[str]) -> str:
        self.argvs.append(argv)
        key = " ".join(argv)
        if key in self.errors:
            raise DaemonError(self.errors[key])
        return self.outputs.get(key, "")


@pytest.fixture
def paths(tmp_path: Path) -> InstallerPaths:
    return InstallerPaths(
        unit_path=tmp_path / UNIT_NAME,
        env_path=tmp_path / "breezed.env",
        config_dir=tmp_path / "breezed",
    )


@pytest.fixture
def cli_runner() -> CliRunner:
    return CliRunner()


def test_status_reports_version_drift_when_stamped_unit_differs(paths: InstallerPaths) -> None:
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    runner = FakeRunner(
        {"systemctl is-active breezed": "active\n", "systemctl is-enabled breezed": "enabled\n"}
    )

    status = daemon_status(paths, runner=runner, fs=fs)

    assert status.unit_present is True
    assert status.active is True
    assert status.enabled is True
    assert status.unit_version == "9.9.9"
    assert status.binary_version == breezed.__version__


def test_status_absent_unit_reports_all_clear(paths: InstallerPaths) -> None:
    status = daemon_status(paths, runner=FakeRunner(), fs=FakeFileOps())

    assert status == DaemonStatus(False, False, False, None, breezed.__version__)


def test_status_is_unprivileged(paths: InstallerPaths) -> None:
    runner = FakeRunner()
    daemon_status(paths, runner=runner, fs=FakeFileOps())

    assert all(
        argv[:2] in (["systemctl", "is-active"], ["systemctl", "is-enabled"])
        for argv in runner.argvs
    )


def test_status_probe_failure_reports_inactive(paths: InstallerPaths) -> None:
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    runner = FakeRunner(errors={"systemctl is-active breezed": "failed"})

    assert daemon_status(paths, runner=runner, fs=fs).active is False


def test_stage_files_wipes_directory_and_writes_exactly_three_files(tmp_path: Path) -> None:
    staging = tmp_path / "stage"
    staging.mkdir()
    (staging / "obsolete").write_text("old", encoding="utf-8")

    staged = stage_files(staging)

    names = ["breezed.service", "breezed.env", "breezed.toml"]
    assert sorted(path.name for path in staging.iterdir()) == sorted(names)
    assert staged == [str(staging / name) for name in names]
    assert "IDRAC_HOST=" in (staging / "breezed.env").read_text(encoding="utf-8")
    assert "[settings]" in (staging / "breezed.toml").read_text(encoding="utf-8")


def test_staged_unit_is_static_and_has_expected_exec_start(tmp_path: Path) -> None:
    staging = tmp_path / "stage"
    stage_files(staging)

    unit = (staging / "breezed.service").read_text(encoding="utf-8")
    assert "{" not in unit and "}" not in unit
    assert (
        "ExecStart=/usr/local/bin/breezed run --config /etc/breezed/breezed.toml "
        "--metrics-port 9762" in unit
    )


def test_install_commands_use_uv_opt_layout_and_protect_existing_state() -> None:
    commands = "\n".join(install_commands())

    assert "UV_TOOL_DIR=/opt/breezed" in commands
    assert "UV_TOOL_BIN_DIR=/usr/local/bin" in commands
    assert "UV_PYTHON_INSTALL_DIR=/opt/breezed-python" in commands
    assert "tool install ~/Projects/breezed --reinstall" in commands
    assert "skip if a tuned config exists" in commands
    assert "skip if secrets already set" in commands
    assert "sudo systemctl enable --now breezed" in commands


def test_uninstall_commands_remove_uv_runtime_and_keep_configuration() -> None:
    commands = uninstall_commands()
    joined = "\n".join(commands)

    assert commands[0] == "sudo systemctl disable --now breezed"
    assert "sudo rm -f /etc/systemd/system/breezed.service" in commands
    assert "UV_TOOL_DIR=/opt/breezed UV_TOOL_BIN_DIR=/usr/local/bin" in joined
    assert "tool uninstall breezed || sudo rm -rf /opt/breezed /opt/breezed-python" in joined
    assert commands[-1] == "sudo systemctl daemon-reload"


def test_help_lists_daemon_subcommands(cli_runner: CliRunner) -> None:
    result = cli_runner.invoke(app, ["daemon", "--help"], catch_exceptions=False)
    assert result.exit_code == 0
    assert all(name in result.output for name in ("install", "status", "uninstall"))


def test_daemon_install_prints_json_then_commands(cli_runner: CliRunner, tmp_path: Path) -> None:
    staging = tmp_path / "stage"
    result = cli_runner.invoke(
        app, ["daemon", "install", "--staging-dir", str(staging)], catch_exceptions=False
    )

    assert result.exit_code == 0
    payload = json.loads(result.stdout.splitlines()[0])
    assert payload == {
        "event": "install_staged",
        "files": [
            str(staging / name) for name in ("breezed.service", "breezed.env", "breezed.toml")
        ],
    }
    assert "Review, then run these commands" in result.output


def test_daemon_install_start_flag_is_gone(cli_runner: CliRunner) -> None:
    assert cli_runner.invoke(app, ["daemon", "install", "--start"]).exit_code != 0


def test_daemon_install_staging_error_exits_1(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
) -> None:
    def boom(_staging_dir: Path) -> list[str]:
        raise DaemonError("cannot stage files")

    monkeypatch.setattr(cli, "stage_files", boom)
    result = cli_runner.invoke(app, ["daemon", "install"])
    assert result.exit_code == 1
    assert "cannot stage files" in result.stderr


def test_daemon_uninstall_prints_plan_and_retained_state(cli_runner: CliRunner) -> None:
    result = cli_runner.invoke(app, ["daemon", "uninstall"], catch_exceptions=False)

    assert result.exit_code == 0
    payload = json.loads(result.stdout.strip().splitlines()[-1])
    assert payload == {
        "event": "uninstall_planned",
        "keeps": ["/etc/breezed.env", "/etc/breezed"],
    }
    assert "systemctl disable --now" in result.output
    assert "/etc/breezed.env and /etc/breezed/" in result.output
    assert "remain:" in result.output


def test_daemon_status_prints_report_json_exit_0(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        cli, "daemon_status", lambda: DaemonStatus(True, False, True, None, "0.2.0")
    )

    result = cli_runner.invoke(app, ["daemon", "status"], catch_exceptions=False)

    assert result.exit_code == 0
    assert json.loads(result.stdout) == {
        "unit_present": True,
        "active": False,
        "enabled": True,
        "unit_version": None,
        "binary_version": "0.2.0",
    }
