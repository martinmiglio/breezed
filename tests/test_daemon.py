"""Daemon deploy tests: staging, install command planning, and unprivileged status.

stage_install is exercised directly (real copytree + relocation + smoke test
into tmp_path); CLI daemon commands are exercised via CliRunner with staging
dirs injected through --staging-dir.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
from typer.testing import CliRunner

import breezed
from breezed import cli
from breezed.cli import app
from breezed.daemon import (
    EXEC_PATH,
    FINAL_BASE_PYTHON,
    FINAL_RUNTIME,
    DaemonError,
    DaemonStatus,
    InstallerPaths,
    daemon_status,
    stage_install,
    staged_uninstall_commands,
)

UNIT_NAME = "breezed.service"
STAMPED_UNIT = f"""\
# Installed by breezed 9.9.9 on 2026-01-01T00:00:00+00:00; re-run `breezed daemon install`
# to refresh this file.
[Service]
ExecStart={EXEC_PATH} run
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


def test_status_reports_version_drift_when_stamped_unit_differs(paths: InstallerPaths):
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    fake_runner = FakeRunner(
        {"systemctl is-active breezed": "active\n", "systemctl is-enabled breezed": "enabled\n"}
    )

    status = daemon_status(paths, runner=fake_runner, fs=fs)

    assert status.unit_present is True
    assert status.active is True
    assert status.enabled is True
    assert status.unit_version == "9.9.9"
    assert status.binary_version == breezed.__version__
    assert status.unit_version != status.binary_version


def test_status_absent_unit_reports_all_clear(paths: InstallerPaths):
    fs, fake_runner = FakeFileOps(), FakeRunner()

    status = daemon_status(paths, runner=fake_runner, fs=fs)

    assert status == DaemonStatus(
        unit_present=False,
        active=False,
        enabled=False,
        unit_version=None,
        binary_version=breezed.__version__,
    )


def test_status_is_unprivileged_and_runs_no_privileged_commands(paths: InstallerPaths):
    fs, fake_runner = FakeFileOps(), FakeRunner()
    daemon_status(paths, runner=fake_runner, fs=fs)
    for argv in fake_runner.argvs:
        assert argv[0] == "systemctl" and argv[1] in ("is-active", "is-enabled")


def test_status_probe_failure_reports_inactive(paths: InstallerPaths):
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    fake_runner = FakeRunner(errors={"systemctl is-active breezed": "systemctl is-active failed"})

    status = daemon_status(paths, runner=fake_runner, fs=fs)

    assert status.unit_present is True
    assert status.active is False


@pytest.fixture
def staging(tmp_path: Path) -> Path:
    return tmp_path / "stage"


def test_stage_install_stages_unit_env_config_and_runtime(staging: Path):
    stage_install(staging)

    assert (
        (staging / "breezed.service")
        .read_text(encoding="utf-8")
        .startswith("# Installed by breezed")
    )
    assert "IDRAC_HOST=" in (staging / "breezed.env").read_text(encoding="utf-8")
    assert "[settings]" in (staging / "breezed.toml").read_text(encoding="utf-8")
    assert (staging / "runtime" / "bin" / "breezed").exists()
    assert (staging / "runtime-python" / "bin").is_dir()


def test_stage_install_relocates_to_final_opt_paths(staging: Path):
    stage_install(staging)

    cfg = (staging / "runtime" / "pyvenv.cfg").read_text(encoding="utf-8")
    assert f"home = {FINAL_BASE_PYTHON}/bin" in cfg
    shebang = (staging / "runtime" / "bin" / "breezed").read_text(encoding="utf-8").splitlines()[0]
    assert shebang == f"#!{FINAL_RUNTIME}/bin/python3"
    python_link = staging / "runtime" / "bin" / "python3"
    if python_link.is_symlink():
        assert os.readlink(python_link).startswith(f"{FINAL_BASE_PYTHON}/bin/")


def test_staged_runtime_works_standalone(staging: Path):
    stage_install(staging)

    probe = subprocess.run(
        [str(staging / "runtime" / "bin" / "breezed"), "--version"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert probe.returncode == 0, probe.stderr
    assert breezed.__version__ in probe.stdout


def test_stage_install_returns_plain_copy_paste_commands(staging: Path):
    staged = stage_install(staging)

    joined = "\n".join(staged.commands)
    for fragment in (
        "useradd --system",
        f"cp -a {staging}/runtime {FINAL_RUNTIME}",
        f"ln -sfn {FINAL_RUNTIME}/bin/breezed {EXEC_PATH}",
        "install -D",
        "systemctl daemon-reload && sudo systemctl enable --now breezed",
        "sudoedit /etc/breezed.env",
    ):
        assert fragment in joined
    assert all(c.split()[0] in ("sudo", "sudoedit") for c in staged.commands)
    assert staged.staged_files == [
        str(staging / name) for name in ("breezed.service", "breezed.env", "breezed.toml")
    ]


def test_stage_refused_when_running_from_deployed_runtime(
    monkeypatch: pytest.MonkeyPatch,
):
    monkeypatch.setattr(sys, "prefix", "/opt/breezed")
    with pytest.raises(DaemonError, match="deployed runtime"):
        stage_install(Path("/tmp/never-created-by-this-test"))


def test_uninstall_commands_stop_remove_and_keep_state():
    commands = staged_uninstall_commands()

    assert any("systemctl disable --now breezed" in c for c in commands)
    assert any("rm -f" in c and "breezed.service" in c and EXEC_PATH in c for c in commands)
    assert any("daemon-reload" in c for c in commands)


def test_help_lists_daemon_subcommands(cli_runner: CliRunner):
    result = cli_runner.invoke(app, ["daemon", "--help"], catch_exceptions=False)
    assert result.exit_code == 0
    for name in ("install", "status", "uninstall"):
        assert name in result.output


def test_daemon_install_stages_into_dir_and_prints_json_event(cli_runner: CliRunner, staging: Path):
    result = cli_runner.invoke(
        app, ["daemon", "install", "--staging-dir", str(staging)], catch_exceptions=False
    )
    assert result.exit_code == 0
    payload = json.loads(result.stdout.strip().splitlines()[-1])
    assert payload["event"] == "install_staged"
    assert payload["files"] == [
        str(staging / name) for name in ("breezed.service", "breezed.env", "breezed.toml")
    ]
    assert "sudo systemctl" in result.output


def test_daemon_install_start_flag_is_gone(cli_runner: CliRunner):
    result = cli_runner.invoke(app, ["daemon", "install", "--start"])
    assert result.exit_code != 0


def test_daemon_install_deployed_runtime_error_exits_1(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
):
    def boom(staging_dir: Path) -> object:
        raise DaemonError("this is the deployed runtime (/opt/breezed)")

    monkeypatch.setattr(cli, "stage_install", boom)
    result = cli_runner.invoke(app, ["daemon", "install"])
    assert result.exit_code == 1
    assert "deployed runtime" in result.stderr


def test_daemon_uninstall_prints_command_plan(cli_runner: CliRunner):
    result = cli_runner.invoke(app, ["daemon", "uninstall"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout.strip().splitlines()[-1])
    assert payload["event"] == "uninstall_planned"
    assert "/etc/breezed.env" in payload["keeps"]
    assert "systemctl disable --now" in result.output


def test_daemon_status_prints_report_json_exit_0(cli_runner: CliRunner, monkeypatch):
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
