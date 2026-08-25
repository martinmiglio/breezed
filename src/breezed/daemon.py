"""Idempotent systemd deployment: install, status, and uninstall of the unit.

Every filesystem effect goes through the injected ``FileOps`` seam; every
process invocation goes through the injected ``CommandRunner``. Re-running
``install()`` is the upgrade mechanism: it always re-renders and overwrites
the unit but never touches an existing env file or config.
"""

import datetime as dt
import grp
import os
import pwd
import re
import shutil
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import Protocol

from breezed import __version__
from breezed.types import DomainError

SERVICE_NAME = "breezed"
ENV_MODE = 0o640

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
    """FS seam — every filesystem effect goes through this; fakes replace it in tests."""

    def write_text(self, path: Path, text: str) -> None: ...
    def read_text(self, path: Path) -> str: ...
    def mkdir(self, path: Path) -> None: ...
    def chown(self, path: Path, owner: str, group: str) -> None: ...
    def chmod(self, path: Path, mode: int) -> None: ...
    def stat(self, path: Path) -> os.stat_result | None: ...  # None when absent
    def unlink(self, path: Path) -> None: ...


class RealFileOps:
    """The one production FileOps; thin delegation onto pathlib/os/pwd/grp."""

    def write_text(self, path: Path, text: str) -> None:
        path.write_text(text)

    def read_text(self, path: Path) -> str:
        return path.read_text()

    def mkdir(self, path: Path) -> None:
        path.mkdir(parents=True, exist_ok=True)

    def chown(self, path: Path, owner: str, group: str) -> None:
        uid = pwd.getpwnam(owner).pw_uid
        gid = grp.getgrnam(group).gr_gid
        os.chown(path, uid, gid)

    def chmod(self, path: Path, mode: int) -> None:
        path.chmod(mode)

    def stat(self, path: Path) -> os.stat_result | None:
        if not path.exists():
            return None
        return path.stat()

    def unlink(self, path: Path) -> None:
        path.unlink()


CommandRunner = Callable[[list[str]], str]
"""Runs systemctl and useradd invocations; returns stdout, raises DaemonError on failure."""


def _run_systemctl(argv: list[str]) -> str:
    """Default CommandRunner: thin wrapper around subprocess.run (sanctioned site).

    Executes ``argv`` verbatim and returns stdout; raises ``DaemonError`` on a
    non-zero exit (expected for ``is-active``/``is-enabled`` probes — callers
    that query catch it), missing binary, or timeout.
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


UserLookup = Callable[[str], bool]


def _user_exists(name: str) -> bool:
    try:
        pwd.getpwnam(name)
    except KeyError:
        return False
    return True


def _first_line(text: str) -> str:
    stripped = text.strip().splitlines()
    return stripped[0][:200] if stripped else ""


def _read_packaged(name: str) -> str:
    candidate = resources.files("breezed").joinpath("templates").joinpath(name)
    if candidate.is_file():
        return candidate.read_text(encoding="utf-8")
    fallback = Path(__file__).resolve().parents[2] / "deploy" / name
    if fallback.is_file():
        return fallback.read_text(encoding="utf-8")
    msg = f"packaged resource missing: {name}"
    raise DaemonError(msg) from None


@dataclass(frozen=True, slots=True)
class InstallReport:
    created: tuple[str, ...]
    skipped: tuple[str, ...]
    unit_version: str
    started: bool


@dataclass(frozen=True, slots=True)
class DaemonStatus:
    unit_present: bool
    active: bool
    enabled: bool
    unit_version: str | None
    binary_version: str


class DaemonInstaller:
    _default_paths: InstallerPaths = InstallerPaths()
    _default_fs: FileOps = RealFileOps()

    def __init__(
        self,
        paths: InstallerPaths = _default_paths,
        *,
        runner: CommandRunner = _run_systemctl,
        fs: FileOps = _default_fs,
        user_lookup: UserLookup = _user_exists,
        exec_path: str | None = None,
        require_root: bool = True,
    ) -> None:
        self._paths = paths
        self._run = runner
        self._fs = fs
        self._user_exists = user_lookup
        self._exec_path = (
            exec_path or shutil.which(SERVICE_NAME) or str(Path(sys.argv[0]).resolve())
        )
        self._require_root = require_root

    def install(self, *, start: bool = False) -> InstallReport:
        self._check_root("install")
        created: list[str] = []
        skipped: list[str] = []

        if self._user_exists(SERVICE_NAME):
            skipped.append("system_user")
        else:
            self._run(
                [
                    "useradd",
                    "--system",
                    "--no-create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    SERVICE_NAME,
                ]
            )
            created.append("system_user")

        self._fs.write_text(self._paths.unit_path, self._render_unit())
        created.append("unit")

        self._fs.mkdir(self._paths.config_dir)
        if self._fs.stat(self._paths.env_path) is None:
            self._fs.write_text(self._paths.env_path, _ENV_SKELETON)
            created.append("env_file")
        else:
            skipped.append("env_file")
        self._fs.chown(self._paths.env_path, "root", SERVICE_NAME)
        self._fs.chmod(self._paths.env_path, ENV_MODE)

        config_path = self._paths.config_dir / "breezed.toml"
        if self._fs.stat(config_path) is None:
            self._fs.write_text(config_path, _read_packaged("breezed.toml.example"))
            created.append("config_example")
        else:
            skipped.append("config_example")

        self._run(["systemctl", "daemon-reload"])
        started = False
        if start:
            self._run(["systemctl", "enable", "--now", SERVICE_NAME])
            started = True
        return InstallReport(tuple(created), tuple(skipped), __version__, started)

    def status(self) -> DaemonStatus:
        unit_present = self._fs.stat(self._paths.unit_path) is not None
        unit_version: str | None = None
        if unit_present:
            try:
                unit_text = self._fs.read_text(self._paths.unit_path)
            except OSError:
                unit_text = ""
            match = _UNIT_STAMP_RE.search(unit_text)
            if match is not None:
                unit_version = match.group(1)
        return DaemonStatus(
            unit_present=unit_present,
            active=self._probe_flag(["systemctl", "is-active", SERVICE_NAME], {"active"}),
            enabled=self._probe_flag(
                ["systemctl", "is-enabled", SERVICE_NAME], {"enabled", "enabled-runtime"}
            ),
            unit_version=unit_version,
            binary_version=__version__,
        )

    def uninstall(self) -> bool:
        self._check_root("uninstall")
        removed = False
        if self._fs.stat(self._paths.unit_path) is not None:
            self._run(["systemctl", "disable", "--now", SERVICE_NAME])
            self._fs.unlink(self._paths.unit_path)
            removed = True
        self._run(["systemctl", "daemon-reload"])
        return removed

    def _check_root(self, command: str) -> None:
        if self._require_root and os.geteuid() != 0:
            msg = f"breezed daemon {command} must run as root; run: sudo breezed daemon {command}"
            raise DaemonError(msg)

    def _render_unit(self) -> str:
        template = _read_packaged("breezed.service.template")
        installed_at = dt.datetime.now(dt.UTC).isoformat(timespec="seconds")
        return (
            template.replace("{version}", __version__)
            .replace("{installed_at}", installed_at)
            .replace("{exec_path}", self._exec_path)
        )

    def _probe_flag(self, argv: list[str], ok_values: set[str]) -> bool:
        try:
            output = self._run(argv)
        except DaemonError:
            return False
        return output.strip() in ok_values


__all__ = [
    "DaemonError",
    "DaemonInstaller",
    "DaemonStatus",
    "FileOps",
    "InstallReport",
    "InstallerPaths",
    "RealFileOps",
    "CommandRunner",
    "UserLookup",
]
