"""Unprivileged systemd deployment planning and status.

The installer writes three small files into a staging dir and returns the
exact privileged commands for the user to run themselves — breezed spawns no
sudo/pkexec, and uv manages /opt directly so no runtime is ever copied by us.
``daemon_status`` is the only execution path and stays unprivileged.
"""

import os
import re
import shutil
import subprocess
from collections.abc import Callable
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import Protocol

from breezed import __version__
from breezed.types import DomainError

SERVICE_NAME = "breezed"
EXEC_PATH = "/usr/local/bin/breezed"
STAGING_DIR = Path("/tmp/breezed-install")

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


@dataclass(frozen=True, slots=True)
class InstallerPaths:
    unit_path: Path = Path("/etc/systemd/system/breezed.service")
    env_path: Path = Path("/etc/breezed.env")
    config_dir: Path = Path("/etc/breezed")


class FileOps(Protocol):
    """Read-only FS seam used by status(); fakes replace it in tests."""

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


CommandRunner = Callable[[list[str]], str]
"""Runs systemctl invocations for probes; returns stdout, raises DaemonError on failure."""


def _run_systemctl(argv: list[str]) -> str:
    """Default CommandRunner: thin wrapper around subprocess.run (sanctioned site).

    Executes ``argv`` verbatim and returns stdout; raises ``DaemonError`` on a
    non-zero exit (expected for probes — callers that query catch it), missing
    binary, or timeout.
    """
    try:
        completed = subprocess.run(argv, capture_output=True, text=True, check=False, timeout=30)
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
    runner: CommandRunner = _run_systemctl,
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


def stage_files(staging_dir: Path = STAGING_DIR) -> list[str]:
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


def install_commands() -> list[str]:
    """Privileged commands turning the staged files into a running service.

    uv owns /opt/breezed and /opt/breezed-python via pinned env vars, so one
    tool-install line replaces any manual runtime copying.
    """
    s = str(STAGING_DIR)
    return [
        "sudo useradd --system --no-create-home --shell /usr/sbin/nologin breezed  "
        "# skip if user already exists",
        "sudo env UV_TOOL_DIR=/opt/breezed UV_TOOL_BIN_DIR=/usr/local/bin"
        ' UV_PYTHON_INSTALL_DIR=/opt/breezed-python "$HOME/.local/bin/uv" tool install'
        " ~/Projects/breezed --reinstall",
        f"sudo install -D -o root -g root -m 0644 {s}/breezed.service "
        "/etc/systemd/system/breezed.service",
        f'sudo install -D -o "$USER" -g "$USER" -m 0664 {s}/breezed.toml '
        "/etc/breezed/breezed.toml  # skip if a tuned config exists",
        f"sudo install -D -o root -g breezed -m 0640 {s}/breezed.env "
        "/etc/breezed.env  # skip if secrets already set",
        "sudoedit /etc/breezed.env                                                          "
        "# IDRAC_HOST / IDRAC_USER / IDRAC_PASSWORD",
        "sudo systemctl daemon-reload",
        "sudo systemctl enable --now breezed",
        f"{EXEC_PATH} --version",
    ]


def uninstall_commands() -> list[str]:
    """Privileged commands removing breezed; /etc/breezed.env and /etc/breezed/ are kept."""
    return [
        "sudo systemctl disable --now breezed",
        f"sudo rm -f /etc/systemd/system/{SERVICE_NAME}.service",
        'sudo env UV_TOOL_DIR=/opt/breezed UV_TOOL_BIN_DIR=/usr/local/bin "$HOME/.local/bin/uv"'
        " tool uninstall breezed || sudo rm -rf /opt/breezed /opt/breezed-python",
        "sudo systemctl daemon-reload",
    ]


__all__ = [
    "EXEC_PATH",
    "STAGING_DIR",
    "CommandRunner",
    "DaemonError",
    "DaemonStatus",
    "FileOps",
    "InstallerPaths",
    "RealFileOps",
    "SERVICE_NAME",
    "daemon_status",
    "install_commands",
    "stage_files",
    "uninstall_commands",
]
