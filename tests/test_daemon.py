"""T8 daemon deploy tests: fakes injected through the paths/runner/fs seams only.

No real /etc access, no root assumptions, no monkeypatching. CLI daemon
commands are exercised through the cli.deps seam with a fake installer.
"""

import json
import os
from pathlib import Path
from typing import cast

import pytest
from typer.testing import CliRunner

import breezed
from breezed import cli
from breezed.cli import AppDeps, ClientFactory, app
from breezed.daemon import (
    DaemonError,
    DaemonInstaller,
    DaemonStatus,
    InstallerPaths,
    InstallReport,
)
from breezed.ipmi import IpmiClient

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
        self.dirs: list[Path] = []
        self.chowns: list[tuple[str, str, str]] = []
        self.chmods: list[tuple[str, int]] = []
        self.unlinked: list[str] = []

    def write_text(self, path: Path, text: str) -> None:
        self.files[path] = text

    def read_text(self, path: Path) -> str:
        return self.files[path]

    def mkdir(self, path: Path) -> None:
        self.dirs.append(path)

    def chown(self, path: Path, owner: str, group: str) -> None:
        self.chowns.append((str(path), owner, group))

    def chmod(self, path: Path, mode: int) -> None:
        self.chmods.append((str(path), mode))

    def stat(self, path: Path) -> os.stat_result | None:
        if path not in self.files:
            return None
        return os.stat_result((0o100644, 0, 0, 0, 0, 0, 0, 0, 0, 0))

    def unlink(self, path: Path) -> None:
        self.unlinked.append(str(path))
        del self.files[path]


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


def make_installer(
    paths: InstallerPaths,
    fs: FakeFileOps,
    runner: FakeRunner,
    *,
    user_exists: bool = False,
    exec_path: str = "/usr/local/bin/breezed",
) -> DaemonInstaller:
    return DaemonInstaller(
        paths,
        runner=runner,
        fs=fs,
        user_lookup=lambda _name: user_exists,
        exec_path=exec_path,
        require_root=False,
    )


@pytest.fixture
def cli_runner():
    return CliRunner()


def test_install_fresh_creates_everything_and_starts(paths: InstallerPaths):
    fs, runner = FakeFileOps(), FakeRunner()
    installer = make_installer(paths, fs, runner)
    report = installer.install(start=True)

    assert report.created == ("system_user", "unit", "env_file", "config_example")
    assert report.skipped == ()
    assert report.unit_version == breezed.__version__
    assert report.started is True

    assert [
        "useradd",
        "--system",
        "--no-create-home",
        "--shell",
        "/usr/sbin/nologin",
        "breezed",
    ] in runner.argvs
    assert ["systemctl", "daemon-reload"] in runner.argvs
    assert ["systemctl", "enable", "--now", "breezed"] in runner.argvs

    unit = fs.files[paths.unit_path]
    assert f"# Installed by breezed {breezed.__version__} on " in unit
    assert "ExecStart=/usr/local/bin/breezed run" in unit
    assert "{version}" not in unit and "{exec_path}" not in unit

    env = fs.files[paths.env_path]
    assert env.startswith("#")
    assert "IDRAC_HOST=" in env and "IDRAC_PASSWORD=" in env
    assert (str(paths.env_path), "root", "breezed") in fs.chowns
    assert (str(paths.env_path), 0o640) in fs.chmods

    example = Path(__file__).resolve().parents[1] / "deploy" / "breezed.toml.example"
    assert fs.files[paths.config_dir / "breezed.toml"] == example.read_text()

    assert paths.config_dir in fs.dirs


def test_install_is_idempotent_and_preserves_env_and_config(paths: InstallerPaths):
    fs, runner = FakeFileOps(), FakeRunner()
    installer = make_installer(paths, fs, runner, user_exists=True)
    installer.install(start=True)
    env_before = fs.files[paths.env_path]
    config_before = fs.files[paths.config_dir / "breezed.toml"]

    second = installer.install(start=True)

    assert second.created == ("unit",)
    assert second.skipped == ("system_user", "env_file", "config_example")
    assert second.started is True
    assert fs.files[paths.env_path] == env_before
    assert fs.files[paths.config_dir / "breezed.toml"] == config_before
    useradd_runs = [argv for argv in runner.argvs if argv[0] == "useradd"]
    assert useradd_runs == []


def test_existing_env_file_with_secrets_is_never_overwritten(paths: InstallerPaths):
    secret_env = "IDRAC_HOST=10.0.0.9\nIDRAC_USER=admin\nIDRAC_PASSWORD=hunter2\n"
    fs, runner = FakeFileOps({str(paths.env_path): secret_env}), FakeRunner()
    installer = make_installer(paths, fs, runner)
    report = installer.install()

    assert "env_file" in report.skipped
    assert fs.files[paths.env_path] == secret_env
    assert (str(paths.env_path), 0o640) in fs.chmods


def test_root_required_raises_actionable_error_without_side_effects():
    if os.geteuid() == 0:
        pytest.skip("running as root; enforcement path unreachable")
    fs, runner = FakeFileOps(), FakeRunner()
    installer = DaemonInstaller(runner=runner, fs=fs, require_root=True)
    with pytest.raises(DaemonError, match="sudo breezed daemon install"):
        installer.install()
    assert runner.argvs == [] and fs.files == {}


def test_status_reports_version_drift_when_stamped_unit_differs(
    paths: InstallerPaths,
):
    fs = FakeFileOps({str(paths.unit_path): STAMPED_UNIT})
    runner = FakeRunner(
        {"systemctl is-active breezed": "active\n", "systemctl is-enabled breezed": "enabled\n"}
    )
    installer = make_installer(paths, fs, runner)

    status = installer.status()

    assert status.unit_present is True
    assert status.active is True
    assert status.enabled is True
    assert status.unit_version == "9.9.9"
    assert status.binary_version == breezed.__version__
    assert status.unit_version != status.binary_version


def test_status_absent_unit_reports_all_clear(paths: InstallerPaths):
    fs, runner = FakeFileOps(), FakeRunner()
    installer = make_installer(paths, fs, runner)

    status = installer.status()

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

    installer = make_installer(paths, fs, runner)

    status = installer.status()
    assert status.unit_present is True
    assert status.active is False


def test_uninstall_disables_removes_unit_and_reloads_but_keeps_user_env_config(
    paths: InstallerPaths,
):
    fs = FakeFileOps(
        {
            str(paths.unit_path): STAMPED_UNIT,
            str(paths.env_path): "IDRAC_HOST=10.0.0.9\n",
            str(paths.config_dir / "breezed.toml"): '[settings]\nhost = "169.254.0.1"\n',
        }
    )
    runner = FakeRunner()
    installer = make_installer(paths, fs, runner)

    removed = installer.uninstall()

    assert removed is True
    assert ["systemctl", "disable", "--now", "breezed"] in runner.argvs
    assert ["systemctl", "daemon-reload"] in runner.argvs
    assert str(paths.unit_path) in fs.unlinked
    assert paths.env_path in fs.files
    assert paths.config_dir / "breezed.toml" in fs.files


def test_uninstall_without_unit_is_a_noop_except_reload(paths: InstallerPaths):
    fs, runner = FakeFileOps(), FakeRunner()
    installer = make_installer(paths, fs, runner)

    assert installer.uninstall() is False
    assert runner.argvs == [["systemctl", "daemon-reload"]]
    assert fs.unlinked == []


class FakeInstaller:
    def __init__(
        self,
        *,
        install_report: InstallReport | None = None,
        status_report: DaemonStatus | None = None,
        error: DaemonError | None = None,
        removed: bool = True,
    ) -> None:
        self.install_report = install_report or InstallReport(("unit",), (), "0.1.0", False)
        self.status_report = status_report or DaemonStatus(True, True, True, "0.1.0", "0.1.0")
        self.error = error
        self.removed = removed
        self.start_arg: bool | None = None

    def install(self, *, start: bool = False) -> InstallReport:
        self.start_arg = start
        if self.error is not None:
            raise self.error
        return self.install_report

    def status(self) -> DaemonStatus:
        if self.error is not None:
            raise self.error
        return self.status_report

    def uninstall(self) -> bool:
        if self.error is not None:
            raise self.error
        return self.removed


@pytest.fixture
def install_installer():
    original = cli.deps

    def install(installer: FakeInstaller) -> FakeInstaller:
        cli.deps = AppDeps(
            build_client=cast(ClientFactory, lambda _settings: cast(IpmiClient, None)),
            sleep_interruptible=lambda _event, _timeout: True,
            build_installer=lambda: cast(DaemonInstaller, installer),
        )
        return installer

    yield install
    cli.deps = original


def test_help_lists_daemon_subcommands(cli_runner: CliRunner):
    result = cli_runner.invoke(app, ["daemon", "--help"], catch_exceptions=False)
    assert result.exit_code == 0
    for name in ("install", "status", "uninstall"):
        assert name in result.output


def test_daemon_install_prints_report_json_exit_0(cli_runner: CliRunner, install_installer):
    installer = install_installer(FakeInstaller())
    result = cli_runner.invoke(app, ["daemon", "install", "--start"], catch_exceptions=False)
    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert set(payload) == {"created", "skipped", "unit_version", "started"}
    assert payload["created"] == ["unit"]
    assert payload["unit_version"] == "0.1.0"
    assert payload["started"] is False
    assert installer.start_arg is True


def test_daemon_install_daemon_error_exits_1_on_stderr(cli_runner: CliRunner, install_installer):
    install_installer(
        FakeInstaller(error=DaemonError("must run as root; run: sudo breezed daemon install"))
    )
    result = cli_runner.invoke(app, ["daemon", "install"])
    assert result.exit_code == 1
    assert "sudo breezed daemon install" in result.stderr
    assert result.stdout.strip() == ""


def test_daemon_status_prints_report_json_exit_0(cli_runner: CliRunner, install_installer):
    install_installer(
        FakeInstaller(status_report=DaemonStatus(True, False, True, "0.1.0", "0.2.0"))
    )
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


def test_daemon_uninstall_prints_json_exit_0(cli_runner: CliRunner, install_installer):
    install_installer(FakeInstaller(removed=True))
    result = cli_runner.invoke(app, ["daemon", "uninstall"], catch_exceptions=False)
    assert result.exit_code == 0
    assert json.loads(result.stdout) == {"event": "uninstalled", "unit_removed": True}
