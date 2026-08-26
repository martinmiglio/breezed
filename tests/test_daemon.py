"""Daemon deployment staging, execution, and status tests."""

import json
import os
from collections.abc import Mapping
from pathlib import Path

import pytest
from typer.testing import CliRunner

import breezed
from breezed import cli
from breezed.cli import app
from breezed.daemon import (
    EXEC_PATH,
    UV_PYTHON_INSTALL_DIR,
    UV_TOOL_BIN_DIR,
    UV_TOOL_DIR,
    DaemonError,
    DaemonStatus,
    InstallerPaths,
    StepOutcome,
    apply,
    build_remove_steps,
    build_steps,
    daemon_status,
    remove,
    stage_files,
)

UNIT_NAME = "breezed.service"
STAMPED_UNIT = f"""\
# Installed by breezed 9.9.9 on 2026-01-01T00:00:00+00:00
[Service]
ExecStart={EXEC_PATH} run
"""

UV_ENV = {
    "UV_TOOL_DIR": UV_TOOL_DIR,
    "UV_TOOL_BIN_DIR": UV_TOOL_BIN_DIR,
    "UV_PYTHON_INSTALL_DIR": UV_PYTHON_INSTALL_DIR,
}


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
        self.argvs: list[tuple[list[str], Mapping[str, str] | None]] = []
        self.outputs = outputs or {}
        self.errors = errors or {}

    def __call__(self, argv: list[str], *, env: Mapping[str, str] | None = None) -> str:
        self.argvs.append((argv, env))
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


def _stage(tmp_path: Path) -> Path:
    staging = tmp_path / "stage"
    staging.mkdir()
    for name in ("breezed.service", "breezed.env", "breezed.toml"):
        (staging / name).write_text("x", encoding="utf-8")
    return staging


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
        for argv, _env in runner.argvs
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


def test_build_steps_labels_and_order(tmp_path: Path) -> None:
    steps = build_steps(tmp_path, "martin", "/usr/local/bin/uv", "/home/martin/Projects/breezed")

    assert [step.label for step in steps] == [
        "ensure system user 'breezed'",
        "uv tool install runtime under /opt",
        "install /etc/systemd/system/breezed.service",
        "install /etc/breezed/breezed.toml (first run only)",
        "install /etc/breezed.env (first run only)",
        "systemctl daemon-reload",
        "systemctl enable --now breezed",
    ]


def test_build_remove_steps_labels_and_order() -> None:
    steps = build_remove_steps("/usr/local/bin/uv")

    assert [step.label for step in steps] == [
        "systemctl disable --now breezed",
        "remove /etc/systemd/system/breezed.service",
        "uv tool uninstall breezed",
        "systemctl daemon-reload",
    ]


def test_apply_full_run_argv_and_env(
    tmp_path: Path, paths: InstallerPaths, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    monkeypatch.setattr("breezed.daemon._user_exists", lambda _name: False)
    staging = _stage(tmp_path)
    runner = FakeRunner()

    apply(
        staging,
        "martin",
        "/usr/local/bin/uv",
        "/home/martin/Projects/breezed",
        paths=paths,
        runner=runner,
        fs=FakeFileOps(),
    )

    assert [argv for argv, _env in runner.argvs] == [
        ["useradd", "--system", "--no-create-home", "--shell", "/usr/sbin/nologin", "breezed"],
        ["/usr/local/bin/uv", "tool", "install", "/home/martin/Projects/breezed", "--reinstall"],
        [
            "install",
            "-D",
            "-o",
            "root",
            "-g",
            "root",
            "-m",
            "0644",
            str(staging / "breezed.service"),
            str(paths.unit_path),
        ],
        [
            "install",
            "-D",
            "-o",
            "martin",
            "-g",
            "martin",
            "-m",
            "0664",
            str(staging / "breezed.toml"),
            str(paths.config_path),
        ],
        [
            "install",
            "-D",
            "-o",
            "root",
            "-g",
            "breezed",
            "-m",
            "0640",
            str(staging / "breezed.env"),
            str(paths.env_path),
        ],
        ["systemctl", "daemon-reload"],
        ["systemctl", "enable", "--now", "breezed"],
    ]
    assert runner.argvs[1][1] == UV_ENV


def test_apply_skips_user_config_and_env_when_present(
    tmp_path: Path, paths: InstallerPaths, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    monkeypatch.setattr("breezed.daemon._user_exists", lambda _name: True)
    staging = _stage(tmp_path)
    fs = FakeFileOps({str(paths.config_path): "tuned", str(paths.env_path): "secrets"})
    runner = FakeRunner()

    results = apply(
        staging,
        "martin",
        "/usr/local/bin/uv",
        "/home/martin/Projects/breezed",
        paths=paths,
        runner=runner,
        fs=fs,
    )

    outcomes = dict(results)
    assert outcomes["ensure system user 'breezed'"] is StepOutcome.SKIPPED
    assert outcomes["install /etc/breezed/breezed.toml (first run only)"] is StepOutcome.SKIPPED
    assert outcomes["install /etc/breezed.env (first run only)"] is StepOutcome.SKIPPED
    assert [argv for argv, _env in runner.argvs] == [
        ["/usr/local/bin/uv", "tool", "install", "/home/martin/Projects/breezed", "--reinstall"],
        [
            "install",
            "-D",
            "-o",
            "root",
            "-g",
            "root",
            "-m",
            "0644",
            str(staging / "breezed.service"),
            str(paths.unit_path),
        ],
        ["systemctl", "daemon-reload"],
        ["systemctl", "enable", "--now", "breezed"],
    ]


def test_remove_runs_steps(paths: InstallerPaths, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    runner = FakeRunner()
    fs = FakeFileOps({str(paths.unit_path): "unit"})

    remove("/usr/local/bin/uv", paths=paths, runner=runner, fs=fs)

    assert [argv for argv, _env in runner.argvs] == [
        ["systemctl", "disable", "--now", "breezed"],
        ["rm", "-f", str(paths.unit_path)],
        ["/usr/local/bin/uv", "tool", "uninstall", "breezed"],
        ["systemctl", "daemon-reload"],
    ]


def test_remove_skips_disable_when_unit_absent(
    paths: InstallerPaths, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    runner = FakeRunner()

    results = remove("/usr/local/bin/uv", paths=paths, runner=runner, fs=FakeFileOps())

    assert dict(results)["systemctl disable --now breezed"] is StepOutcome.SKIPPED
    assert [argv for argv, _env in runner.argvs] == [
        ["rm", "-f", str(paths.unit_path)],
        ["/usr/local/bin/uv", "tool", "uninstall", "breezed"],
        ["systemctl", "daemon-reload"],
    ]


def test_remove_runtime_falls_back_to_rm_on_uninstall_failure(
    paths: InstallerPaths, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    runner = FakeRunner(errors={"/usr/local/bin/uv tool uninstall breezed": "boom"})
    fs = FakeFileOps({str(paths.unit_path): "unit"})

    remove("/usr/local/bin/uv", paths=paths, runner=runner, fs=fs)

    assert ["rm", "-rf", "/opt/breezed", "/opt/breezed-python"] in [
        argv for argv, _env in runner.argvs
    ]


def test_apply_requires_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 1000)

    with pytest.raises(DaemonError, match="root"):
        apply(tmp_path, "martin", "/usr/local/bin/uv", "src", fs=FakeFileOps(), runner=FakeRunner())


def test_remove_requires_root(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 1000)

    with pytest.raises(DaemonError, match="root"):
        remove("/usr/local/bin/uv", fs=FakeFileOps(), runner=FakeRunner())


def test_help_lists_daemon_subcommands(cli_runner: CliRunner) -> None:
    result = cli_runner.invoke(app, ["daemon", "--help"], catch_exceptions=False)
    assert result.exit_code == 0
    assert all(name in result.output for name in ("install", "status", "uninstall"))
    assert "│ apply" not in result.output
    assert "│ remove" not in result.output


def test_daemon_install_builds_sudo_argv(
    cli_runner: CliRunner, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(cli.os, "geteuid", lambda: 1000)
    monkeypatch.setattr(cli, "_resolve_uv", lambda: "/home/martin/.local/bin/uv")
    monkeypatch.setattr(cli.getpass, "getuser", lambda: "martin")
    monkeypatch.setattr(cli, "_resolve_sudo", lambda: "/usr/bin/sudo")
    monkeypatch.setattr(cli, "_resolve_self", lambda: Path("/home/martin/.local/bin/breezed"))
    captured: list[list[str]] = []
    monkeypatch.setattr(cli, "_run_sudo", lambda argv: captured.append(argv))
    staging = tmp_path / "stage"
    source = str(tmp_path.resolve())

    result = cli_runner.invoke(
        app,
        ["daemon", "install", "--staging-dir", str(staging), "--source", source],
        catch_exceptions=False,
    )

    assert result.exit_code == 0
    assert json.loads(result.stdout) == {"event": "install_complete"}
    assert captured == [
        [
            "/usr/bin/sudo",
            "/home/martin/.local/bin/breezed",
            "daemon",
            "apply",
            "--staging-dir",
            str(staging),
            "--owner",
            "martin",
            "--uv",
            "/home/martin/.local/bin/uv",
            "--source",
            source,
        ]
    ]


def test_daemon_install_dry_run_prints_plan_without_sudo(
    cli_runner: CliRunner, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(cli.os, "geteuid", lambda: 1000)
    monkeypatch.setattr(cli, "_resolve_uv", lambda: "/usr/local/bin/uv")
    monkeypatch.setattr(cli.getpass, "getuser", lambda: "martin")

    def boom(_argv: list[str]) -> None:
        raise AssertionError("sudo must not run in dry-run")

    monkeypatch.setattr(cli, "_run_sudo", boom)

    result = cli_runner.invoke(
        app,
        ["daemon", "install", "--staging-dir", str(tmp_path / "stage"), "--dry-run"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["event"] == "install_planned"
    assert "uv tool install runtime under /opt" in payload["steps"]
    assert "systemctl enable --now breezed" in payload["steps"]
    assert "privileged steps" in result.stderr


def test_daemon_install_as_root_applies_in_process(
    cli_runner: CliRunner, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(cli.os, "geteuid", lambda: 0)
    monkeypatch.setattr(cli, "_resolve_uv", lambda: "/usr/local/bin/uv")
    monkeypatch.setattr(cli.getpass, "getuser", lambda: "martin")
    monkeypatch.setattr(
        cli,
        "apply",
        lambda staging, owner, uv, source: [
            ("ensure system user 'breezed'", StepOutcome.DONE),
            ("uv tool install runtime under /opt", StepOutcome.DONE),
        ],
    )

    def boom(_argv: list[str]) -> None:
        raise AssertionError("sudo must not run as root")

    monkeypatch.setattr(cli, "_run_sudo", boom)

    result = cli_runner.invoke(
        app, ["daemon", "install", "--staging-dir", str(tmp_path / "stage")], catch_exceptions=False
    )

    assert result.exit_code == 0
    assert json.loads(result.stdout) == {"event": "install_complete"}
    assert "✔" in result.stderr


def test_daemon_install_start_flag_is_gone(cli_runner: CliRunner) -> None:
    assert cli_runner.invoke(app, ["daemon", "install", "--start"]).exit_code != 0


def test_daemon_install_staging_error_exits_1(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(cli, "_resolve_uv", lambda: "/usr/local/bin/uv")
    monkeypatch.setattr(cli.getpass, "getuser", lambda: "martin")

    def boom(_staging_dir: Path) -> list[str]:
        raise DaemonError("cannot stage files")

    monkeypatch.setattr(cli, "stage_files", boom)
    result = cli_runner.invoke(app, ["daemon", "install"])
    assert result.exit_code == 1
    assert "cannot stage files" in result.stderr


def test_daemon_uninstall_builds_sudo_argv(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(cli.os, "geteuid", lambda: 1000)
    monkeypatch.setattr(cli, "_resolve_uv", lambda: "/usr/local/bin/uv")
    monkeypatch.setattr(cli, "_resolve_sudo", lambda: "/usr/bin/sudo")
    monkeypatch.setattr(cli, "_resolve_self", lambda: Path("/usr/local/bin/breezed"))
    captured: list[list[str]] = []
    monkeypatch.setattr(cli, "_run_sudo", lambda argv: captured.append(argv))

    result = cli_runner.invoke(app, ["daemon", "uninstall"], catch_exceptions=False)

    assert result.exit_code == 0
    assert json.loads(result.stdout) == {
        "event": "uninstall_complete",
        "keeps": ["/etc/breezed.env", "/etc/breezed"],
    }
    assert captured == [
        ["/usr/bin/sudo", "/usr/local/bin/breezed", "daemon", "remove", "--uv", "/usr/local/bin/uv"]
    ]


def test_daemon_uninstall_dry_run_prints_plan_and_retained_state(
    cli_runner: CliRunner, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(cli.os, "geteuid", lambda: 1000)
    monkeypatch.setattr(cli, "_resolve_uv", lambda: "/usr/local/bin/uv")

    def boom(_argv: list[str]) -> None:
        raise AssertionError("sudo must not run in dry-run")

    monkeypatch.setattr(cli, "_run_sudo", boom)

    result = cli_runner.invoke(app, ["daemon", "uninstall", "--dry-run"], catch_exceptions=False)

    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["event"] == "uninstall_planned"
    assert payload["keeps"] == ["/etc/breezed.env", "/etc/breezed"]
    assert "uv tool uninstall breezed" in payload["steps"]
    assert "systemctl disable --now breezed" in result.stderr


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
