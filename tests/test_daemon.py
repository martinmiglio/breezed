"""T8 daemon deploy tests: fakes injected through the paths/runner/fs seams only.

No real /etc access, no root assumptions. CLI daemon commands are exercised by
monkeypatching the module-level renderers.
"""

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

import breezed
from breezed import cli
from breezed.cli import app
from breezed.daemon import (
    DaemonError,
    DaemonStatus,
    InstallerPaths,
    daemon_status,
)

UNIT_NAME = "breezed.service"
STAMPED_UNIT = """\
# Installed by breezed 9.9.9 on 2026-01-01T00:00:00+00:00; re-run `breezed daemon install`
# to refresh this file.
[Service]
ExecStart=/usr/local/bin/breezed run
"""


class FakeFileOps:
    def __init__(self, files: dict[str, str] | None = None) -> None:
        self.files: dict[Path, str] = {Path(name): body for name, body in (files or {}).items()}

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
def cli_runner():
    return CliRunner()


def test_status_reports_version_drift_when_stamped_unit_differs(
    paths: InstallerPaths,
):
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
    assert status.unit_version != status.binary_version


def test_status_absent_unit_reports_all_clear(paths: InstallerPaths):
    fs, runner = FakeFileOps(), FakeRunner()

    status = daemon_status(paths, runner=runner, fs=fs)

    assert status == DaemonStatus(
        unit_present=False,
        active=False,
        enabled=False,
        unit_version=None,
        binary_version=breezed.__version__,
    )


def test_status_probe_failure_reports_inactive(paths: InstallerPaths):
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    runner = FakeRunner(errors={"systemctl is-active breezed": "systemctl is-active failed"})

    status = daemon_status(paths, runner=runner, fs=fs)
    assert status.unit_present is True
    assert status.active is False


def test_help_lists_daemon_subcommands(cli_runner: CliRunner):
    result = cli_runner.invoke(app, ["daemon", "--help"], catch_exceptions=False)
    assert result.exit_code == 0
    for name in ("install", "status", "uninstall"):
        assert name in result.output


def test_daemon_install_prints_rendered_script_exit_0(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
):
    seen: dict[bool, bool] = {}

    def fake_render(start: bool) -> str:
        seen[start] = True
        return f"# install script start={start}"

    monkeypatch.setattr(cli, "render_install_script", fake_render)
    result = cli_runner.invoke(app, ["daemon", "install", "--start"], catch_exceptions=False)
    assert result.exit_code == 0
    assert "# install script start=True" in result.stdout
    assert seen[True] is True


def test_daemon_install_daemon_error_exits_1_on_stderr(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
):
    def explode(*_args: object, **_kwargs: object) -> str:
        raise DaemonError("no such template")

    monkeypatch.setattr(cli, "render_install_script", explode)
    result = cli_runner.invoke(app, ["daemon", "install"])
    assert result.exit_code == 1
    assert "no such template" in result.stderr


def test_daemon_uninstall_prints_rendered_script_exit_0(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setattr(cli, "render_uninstall_script", lambda: "# uninstall script")
    result = cli_runner.invoke(app, ["daemon", "uninstall"], catch_exceptions=False)
    assert result.exit_code == 0
    assert "# uninstall script" in result.stdout


def test_daemon_status_prints_report_json_exit_0(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
):
    def fake_status(**kwargs: object) -> DaemonStatus:
        return DaemonStatus(True, False, True, "0.1.0", "0.2.0")

    monkeypatch.setattr(cli, "daemon_status", fake_status)
    result = cli_runner.invoke(app, ["daemon", "status"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload == {
        "unit_present": True,
        "active": False,
        "enabled": True,
        "unit_version": "0.1.0",
        "binary_version": "0.2.0",
    }
