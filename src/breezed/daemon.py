"""Systemd deployment planning, execution, and status.

``daemon install`` stages three small files, then re-executes itself once via
sudo to perform the fixed privileged steps; ``daemon uninstall`` mirrors that
with a single sudo re-exec that removes the service and runtime. The privileged
path (``apply``/``remove``) runs a hard-coded, code-reviewed sequence — no
free-form commands, no shell — and stays output-free, returning step results
for the CLI to render.
"""

import os
import pwd
import re
import shutil
import subprocess
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from enum import StrEnum
from importlib import resources
from pathlib import Path
from typing import Protocol

from breezed import __version__
from breezed.types import DomainError

SERVICE_NAME = "breezed"
SERVICE_USER = "breezed"
EXEC_PATH = "/usr/local/bin/breezed"
UV_TOOL_DIR = "/opt/breezed"
UV_TOOL_BIN_DIR = "/usr/local/bin"
UV_PYTHON_INSTALL_DIR = "/opt/breezed-python"

_UV_ENV = {
    "UV_TOOL_DIR": UV_TOOL_DIR,
    "UV_TOOL_BIN_DIR": UV_TOOL_BIN_DIR,
    "UV_PYTHON_INSTALL_DIR": UV_PYTHON_INSTALL_DIR,
}

_UNIT_STAMP_RE = re.compile(r"^# Installed by breezed (\S+) on ", re.MULTILINE)

_ENV_SKELETON = """\
# breezed environment file — holds the iDRAC secrets.
# The installer never overwrites this file; edit values in place.
IDRAC_HOST=
IDRAC_USER=
IDRAC_PASSWORD=
"""


class DaemonError(DomainError):
    """Deployment failures with actionable messages; never raw OSError text."""


class StepOutcome(StrEnum):
    """Result of a single privileged step; DONE means the step changed state."""

    DONE = "done"
    SKIPPED = "skipped"


@dataclass(frozen=True, slots=True)
class InstallerPaths:
    unit_path: Path = Path("/etc/systemd/system/breezed.service")
    env_path: Path = Path("/etc/breezed.env")
    config_dir: Path = Path("/etc/breezed")

    @property
    def config_path(self) -> Path:
        return self.config_dir / "breezed.toml"


@dataclass(frozen=True, slots=True)
class Step:
    """One privileged operation: a human label and its bound execution."""

    label: str
    run: Callable[[], StepOutcome]


class FileOps(Protocol):
    """Read-only FS seam used by status() and the idempotence checks; fakes replace it in tests."""

    def read_text(self, path: Path) -> str: ...
    def stat(self, path: Path) -> os.stat_result | None: ...  # None when absent


class RealFileOps:
    """The one production FileOps; thin delegation onto pathlib/os."""

    def read_text(self, path: Path) -> str:
        return path.read_text()

    def stat(self, path: Path) -> os.stat_result | None:
        if not path.exists():
            return None
        return path.stat()


class CommandRunner(Protocol):
    """Runs an argv list, optionally with extra env vars merged in; returns stdout."""

    def __call__(self, argv: list[str], *, env: Mapping[str, str] | None = None) -> str: ...


def _run_command(argv: list[str], *, env: Mapping[str, str] | None = None) -> str:
    """Default CommandRunner: subprocess.run with env overrides; raises DaemonError on failure."""
    full_env = None if env is None else {**os.environ, **env}
    try:
        completed = subprocess.run(
            argv, capture_output=True, text=True, check=False, timeout=600, env=full_env
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        msg = f"{' '.join(argv)} failed to run: {exc}"
        raise DaemonError(msg) from exc
    if completed.returncode != 0:
        snippet = _first_line(completed.stderr)
        msg = f"{' '.join(argv)} failed (rc={completed.returncode}): {snippet}"
        raise DaemonError(msg)
    return completed.stdout


def _first_line(text: str) -> str:
    stripped = text.strip().splitlines()
    return stripped[0][:200] if stripped else ""


def _read_packaged(name: str) -> str:
    candidate = resources.files("breezed").joinpath("templates").joinpath(name)
    if not candidate.is_file():
        msg = f"packaged resource missing: {name}"
        raise DaemonError(msg)
    return candidate.read_text(encoding="utf-8")


def _user_exists(name: str) -> bool:
    try:
        pwd.getpwnam(name)
    except KeyError:
        return False
    return True


def _require_root() -> None:
    if os.geteuid() != 0:
        msg = "apply/remove must run as root; they are invoked via sudo by `breezed daemon install`"
        raise DaemonError(msg)


@dataclass(frozen=True, slots=True)
class DaemonStatus:
    unit_present: bool
    active: bool
    enabled: bool
    unit_version: str | None
    binary_version: str


def _probe_flag(runner: CommandRunner, argv: list[str], ok_values: set[str]) -> bool:
    try:
        output = runner(argv)
    except DaemonError:
        return False
    return output.strip() in ok_values


_default_paths: InstallerPaths = InstallerPaths()
_default_fs: FileOps = RealFileOps()


def daemon_status(
    paths: InstallerPaths = _default_paths,
    *,
    runner: CommandRunner = _run_command,
    fs: FileOps = _default_fs,
) -> DaemonStatus:
    unit_present = fs.stat(paths.unit_path) is not None
    unit_version: str | None = None
    if unit_present:
        try:
            unit_text = fs.read_text(paths.unit_path)
        except OSError:
            unit_text = ""
        match = _UNIT_STAMP_RE.search(unit_text)
        if match is not None:
            unit_version = match.group(1)
    return DaemonStatus(
        unit_present=unit_present,
        active=_probe_flag(runner, ["systemctl", "is-active", SERVICE_NAME], {"active"}),
        enabled=_probe_flag(
            runner, ["systemctl", "is-enabled", SERVICE_NAME], {"enabled", "enabled-runtime"}
        ),
        unit_version=unit_version,
        binary_version=__version__,
    )


def stage_files(staging_dir: Path) -> list[str]:
    """Wipe staging_dir and write the unit, env skeleton, and example config."""
    shutil.rmtree(staging_dir, ignore_errors=True)
    staged = {
        staging_dir / "breezed.service": _read_packaged("breezed.service.template"),
        staging_dir / "breezed.env": _ENV_SKELETON,
        staging_dir / "breezed.toml": _read_packaged("breezed.toml.example"),
    }
    try:
        staging_dir.mkdir(parents=True)
        for path, content in staged.items():
            path.write_text(content, encoding="utf-8")
    except OSError as exc:
        msg = f"cannot write staged files into {staging_dir}: {exc}"
        raise DaemonError(msg) from exc
    return [str(path) for path in staged]


# --- privileged step implementations (each is a fixed, argv-list-only operation) ---


def _ensure_service_user(runner: CommandRunner) -> StepOutcome:
    if _user_exists(SERVICE_USER):
        return StepOutcome.SKIPPED
    runner(
        ["useradd", "--system", "--no-create-home", "--shell", "/usr/sbin/nologin", SERVICE_USER]
    )
    return StepOutcome.DONE


def _install_runtime(runner: CommandRunner, uv: str, source: str) -> StepOutcome:
    runner([uv, "tool", "install", source, "--reinstall"], env=_UV_ENV)
    return StepOutcome.DONE


def _install_unit(runner: CommandRunner, staging_dir: Path, paths: InstallerPaths) -> StepOutcome:
    runner(
        [
            "install",
            "-D",
            "-o",
            "root",
            "-g",
            "root",
            "-m",
            "0644",
            str(staging_dir / "breezed.service"),
            str(paths.unit_path),
        ]
    )
    return StepOutcome.DONE


def _install_config_if_absent(
    runner: CommandRunner,
    fs: FileOps,
    staging_dir: Path,
    owner: str,
    paths: InstallerPaths,
) -> StepOutcome:
    if fs.stat(paths.config_path) is not None:
        return StepOutcome.SKIPPED
    runner(
        [
            "install",
            "-D",
            "-o",
            owner,
            "-g",
            owner,
            "-m",
            "0664",
            str(staging_dir / "breezed.toml"),
            str(paths.config_path),
        ]
    )
    return StepOutcome.DONE


def _install_env_if_absent(
    runner: CommandRunner, fs: FileOps, staging_dir: Path, paths: InstallerPaths
) -> StepOutcome:
    if fs.stat(paths.env_path) is not None:
        return StepOutcome.SKIPPED
    runner(
        [
            "install",
            "-D",
            "-o",
            "root",
            "-g",
            SERVICE_USER,
            "-m",
            "0640",
            str(staging_dir / "breezed.env"),
            str(paths.env_path),
        ]
    )
    return StepOutcome.DONE


def _daemon_reload(runner: CommandRunner) -> StepOutcome:
    runner(["systemctl", "daemon-reload"])
    return StepOutcome.DONE


def _enable_start(runner: CommandRunner) -> StepOutcome:
    runner(["systemctl", "enable", "--now", SERVICE_NAME])
    return StepOutcome.DONE


def _disable_now(runner: CommandRunner, fs: FileOps, paths: InstallerPaths) -> StepOutcome:
    if fs.stat(paths.unit_path) is None:
        return StepOutcome.SKIPPED
    runner(["systemctl", "disable", "--now", SERVICE_NAME])
    return StepOutcome.DONE


def _rm_unit(runner: CommandRunner, paths: InstallerPaths) -> StepOutcome:
    runner(["rm", "-f", str(paths.unit_path)])
    return StepOutcome.DONE


def _uninstall_runtime(runner: CommandRunner, uv: str) -> StepOutcome:
    try:
        runner([uv, "tool", "uninstall", SERVICE_NAME], env=_UV_ENV)
    except DaemonError:
        runner(["rm", "-rf", UV_TOOL_DIR, UV_PYTHON_INSTALL_DIR])
    return StepOutcome.DONE


def build_steps(
    staging_dir: Path,
    owner: str,
    uv: str,
    source: str,
    *,
    paths: InstallerPaths = _default_paths,
    runner: CommandRunner = _run_command,
    fs: FileOps = _default_fs,
) -> list[Step]:
    """The fixed install sequence; consumed by both the dry-run printer and apply()."""
    return [
        Step("ensure system user 'breezed'", lambda: _ensure_service_user(runner)),
        Step("uv tool install runtime under /opt", lambda: _install_runtime(runner, uv, source)),
        Step(
            "install /etc/systemd/system/breezed.service",
            lambda: _install_unit(runner, staging_dir, paths),
        ),
        Step(
            "install /etc/breezed/breezed.toml (first run only)",
            lambda: _install_config_if_absent(runner, fs, staging_dir, owner, paths),
        ),
        Step(
            "install /etc/breezed.env (first run only)",
            lambda: _install_env_if_absent(runner, fs, staging_dir, paths),
        ),
        Step("systemctl daemon-reload", lambda: _daemon_reload(runner)),
        Step("systemctl enable --now breezed", lambda: _enable_start(runner)),
    ]


def build_remove_steps(
    uv: str,
    *,
    paths: InstallerPaths = _default_paths,
    runner: CommandRunner = _run_command,
    fs: FileOps = _default_fs,
) -> list[Step]:
    """The fixed uninstall sequence; /etc/breezed.env and /etc/breezed/ are kept."""
    return [
        Step("systemctl disable --now breezed", lambda: _disable_now(runner, fs, paths)),
        Step("remove /etc/systemd/system/breezed.service", lambda: _rm_unit(runner, paths)),
        Step("uv tool uninstall breezed", lambda: _uninstall_runtime(runner, uv)),
        Step("systemctl daemon-reload", lambda: _daemon_reload(runner)),
    ]


def apply(
    staging_dir: Path,
    owner: str,
    uv: str,
    source: str,
    *,
    paths: InstallerPaths = _default_paths,
    runner: CommandRunner = _run_command,
    fs: FileOps = _default_fs,
) -> list[tuple[str, StepOutcome]]:
    """Execute the install sequence as root; returns (label, outcome) per step."""
    _require_root()
    return [
        (step.label, step.run())
        for step in build_steps(staging_dir, owner, uv, source, paths=paths, runner=runner, fs=fs)
    ]


def remove(
    uv: str,
    *,
    paths: InstallerPaths = _default_paths,
    runner: CommandRunner = _run_command,
    fs: FileOps = _default_fs,
) -> list[tuple[str, StepOutcome]]:
    """Execute the uninstall sequence as root; returns (label, outcome) per step."""
    _require_root()
    return [
        (step.label, step.run())
        for step in build_remove_steps(uv, paths=paths, runner=runner, fs=fs)
    ]


__all__ = [
    "EXEC_PATH",
    "SERVICE_NAME",
    "SERVICE_USER",
    "UV_TOOL_DIR",
    "UV_TOOL_BIN_DIR",
    "UV_PYTHON_INSTALL_DIR",
    "CommandRunner",
    "DaemonError",
    "DaemonStatus",
    "FileOps",
    "InstallerPaths",
    "RealFileOps",
    "Step",
    "StepOutcome",
    "apply",
    "build_remove_steps",
    "build_steps",
    "daemon_status",
    "remove",
    "stage_files",
]
