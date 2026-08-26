"""Unprivileged systemd deployment staging and status.

``stage_install`` copies the runtime into a staging dir, relocates it to its
final /opt paths in-process, and returns the exact privileged commands for the
user to run themselves — breezed spawns no sudo/pkexec. ``daemon_status`` is
the only execution path and stays unprivileged.
"""

import datetime as dt
import os
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
EXEC_PATH = "/usr/local/bin/breezed"
ENV_MODE = 0o640
STAGING_DIR = Path("/tmp/breezed-install")
FINAL_RUNTIME = Path("/opt/breezed")
FINAL_BASE_PYTHON = Path("/opt/breezed-python")

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


def _venv_home(venv: Path) -> Path:
    """The base interpreter bin dir, recorded in pyvenv.cfg as ``home``."""
    for line in (venv / "pyvenv.cfg").read_text(encoding="utf-8").splitlines():
        if line.startswith("home"):
            _, _, value = line.partition("=")
            return Path(value.strip())
    msg = f"pyvenv.cfg in {venv} has no home entry"
    raise DaemonError(msg)


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


def _render_unit() -> str:
    template = _read_packaged("breezed.service.template")
    installed_at = dt.datetime.now(dt.UTC).isoformat(timespec="seconds")
    return (
        template.replace("{version}", __version__)
        .replace("{installed_at}", installed_at)
        .replace("{exec_path}", EXEC_PATH)
    )


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


def _ensure_not_deployed_runtime(source_prefix: Path) -> None:
    """Refuse staging when the source prefix is inside the deploy target.

    The live incident: /usr/local/bin/breezed shadowed ~/.local/bin, so the
    stager ran from /opt/breezed and would have copied the deployed runtime
    onto itself.
    """
    prefix = source_prefix.resolve()
    for target in (FINAL_RUNTIME, FINAL_BASE_PYTHON):
        resolved = target.resolve()
        if prefix == resolved or resolved in prefix.parents:
            msg = (
                f"this is the deployed runtime ({resolved}); stage installs from a "
                "development install instead (e.g. ~/.local/bin/breezed)"
            )
            raise DaemonError(msg)


def _default_source_prefix() -> Path:
    """sys.prefix, but only when it actually holds an installed breezed runtime.

    Under test runners the active prefix is pytest's own environment with no
    ``bin/breezed``, so staging must never silently copy it.
    """
    prefix = Path(sys.prefix).resolve()
    if not (prefix / "bin" / SERVICE_NAME).exists():
        msg = "no installed breezed runtime found; generate installs from `uv tool install .`"
        raise DaemonError(msg)
    return prefix


def _relocate_runtime(staged_venv: Path, src_venv: Path) -> None:
    """Point the staged venv at its final /opt locations.

    Must match the printed install commands exactly: the runtime lands at
    /opt/breezed and the base interpreter at /opt/breezed-python. Shebangs are
    rewritten only for entry points that referenced the source venv's own
    interpreter; foreign scripts (and test fakes) pass through untouched.
    """
    cfg = staged_venv / "pyvenv.cfg"
    lines = cfg.read_text(encoding="utf-8").splitlines(keepends=True)
    rewritten = [
        f"home = {FINAL_BASE_PYTHON}/bin\n" if line.startswith("home") else line for line in lines
    ]
    cfg.write_text("".join(rewritten), encoding="utf-8")

    venv_bin_marker = f"{src_venv}/bin"
    for entry in (staged_venv / "bin").iterdir():
        if entry.is_symlink() and entry.name.startswith("python"):
            # python/python3 point into the base install; retarget to its final home.
            link_target = os.readlink(entry)
            name = Path(link_target).name
            entry.unlink()
            os.symlink(f"{FINAL_BASE_PYTHON}/bin/{name}", entry)
            continue
        if not entry.is_file():
            continue
        if entry.name.startswith(("activate", "Activate")):
            continue
        with open(entry, "rb") as f:
            if f.read(2) != b"#!":
                continue
        lines = entry.read_text(encoding="utf-8").splitlines(keepends=True)
        if venv_bin_marker not in lines[0]:
            continue
        lines[0] = f"#!{FINAL_RUNTIME}/bin/python3\n"
        entry.write_text("".join(lines), encoding="utf-8")


def _install_commands(staging_dir: Path) -> list[str]:
    s = str(staging_dir)
    return [
        "sudo useradd --system --no-create-home --shell /usr/sbin/nologin breezed  # if missing",
        f"sudo rm -rf {FINAL_RUNTIME} && sudo cp -a {s}/runtime {FINAL_RUNTIME}",
        f"sudo rm -rf {FINAL_BASE_PYTHON} && sudo cp -a {s}/runtime-python {FINAL_BASE_PYTHON}",
        f"sudo ln -sfn {FINAL_RUNTIME}/bin/breezed {EXEC_PATH}",
        f"sudo install -D {s}/breezed.service /etc/systemd/system/breezed.service",
        f"sudo install -D {s}/breezed.toml /etc/breezed/breezed.toml  # skip if tuning kept",
        f"sudo install -D -m {ENV_MODE:o} {s}/breezed.env /etc/breezed.env  # skip if secrets exist",  # noqa: E501
        "sudoedit /etc/breezed.env  # set IDRAC_HOST / IDRAC_USER / IDRAC_PASSWORD",
        "sudo systemctl daemon-reload && sudo systemctl enable --now breezed",
    ]


def staged_uninstall_commands() -> list[str]:
    """Privileged commands that stop and remove breezed; keeps user/env/config."""
    return [
        "sudo systemctl disable --now breezed",
        f"sudo rm -f /etc/systemd/system/{SERVICE_NAME}.service {EXEC_PATH}",
        "sudo systemctl daemon-reload",
    ]


@dataclass(frozen=True, slots=True)
class StagedInstall:
    commands: list[str]
    staged_files: list[str]


def stage_install(
    staging_dir: Path = STAGING_DIR, *, source_prefix: Path | None = None
) -> StagedInstall:
    """Copy and relocate the runtime into staging_dir; no privileged side effects.

    The staged runtime is relocated to its final /opt paths up front, so plain
    copy-paste commands are all the privilege escalation the user needs. A smoke
    test runs the staged ``breezed --version`` before anything is recommended
    for sudo. ``source_prefix`` defaults to sys.prefix but is only accepted when
    it actually contains an installed breezed runtime; tests inject a minimal
    fake prefix instead.
    """
    src_venv = (source_prefix if source_prefix is not None else _default_source_prefix()).resolve()
    _ensure_not_deployed_runtime(src_venv)
    home = _venv_home(src_venv)

    shutil.rmtree(staging_dir, ignore_errors=True)
    runtime = staging_dir / "runtime"
    base_python = staging_dir / "runtime-python"
    try:
        shutil.copytree(src_venv, runtime, symlinks=True)
        shutil.copytree(home.resolve().parent, base_python, symlinks=True)
    except OSError as exc:
        msg = f"cannot stage runtime into {staging_dir}: {exc}"
        raise DaemonError(msg) from exc
    _relocate_runtime(runtime, src_venv)

    staged_files = [
        str(staging_dir / name) for name in ("breezed.service", "breezed.env", "breezed.toml")
    ]
    try:
        (staging_dir / "breezed.service").write_text(_render_unit(), encoding="utf-8")
        (staging_dir / "breezed.env").write_text(_ENV_SKELETON, encoding="utf-8")
        (staging_dir / "breezed.toml").write_text(
            _read_packaged("breezed.toml.example"), encoding="utf-8"
        )
        probe = subprocess.run(
            [str(runtime / "bin" / SERVICE_NAME), "--version"],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except OSError as exc:
        msg = f"staging incomplete in {staging_dir}: {exc}"
        raise DaemonError(msg) from exc
    if probe.returncode != 0:
        msg = (
            "staged runtime failed its smoke test "
            f"({SERVICE_NAME} --version rc={probe.returncode}): {_first_line(probe.stderr)}"
        )
        raise DaemonError(msg)
    return StagedInstall(commands=_install_commands(staging_dir), staged_files=staged_files)


__all__ = [
    "EXEC_PATH",
    "CommandRunner",
    "DaemonError",
    "DaemonStatus",
    "FileOps",
    "InstallerPaths",
    "RealFileOps",
    "SERVICE_NAME",
    "STAGING_DIR",
    "StagedInstall",
    "daemon_status",
    "stage_install",
    "staged_uninstall_commands",
]
