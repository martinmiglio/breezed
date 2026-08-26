"""Daemon deployment subcommands: install/apply/status/uninstall/remove.

Thin CLI shell over breezed.entry.daemon; all privileged logic lives there.
Re-executes itself once via sudo for the fixed, code-reviewed install steps.
"""

import getpass
import json
import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import asdict
from pathlib import Path
from typing import Annotated

import typer

from breezed.entry.common import _fail
from breezed.entry.daemon import (
    DaemonError,
    Step,
    StepOutcome,
    apply,
    build_remove_steps,
    build_steps,
    daemon_status,
    remove,
    stage_files,
)

daemon_app = typer.Typer(add_completion=False, pretty_exceptions_enable=False)

_KEEP = ["/etc/breezed.env", "/etc/breezed"]


def _resolve_self() -> Path:
    return Path(os.path.realpath(sys.argv[0]))


def _resolve_sudo() -> str:
    sudo = shutil.which("sudo")
    if sudo is None:
        _fail(DaemonError("sudo not found; `daemon install` needs a single sudo prompt"), code=1)
    return sudo


def _resolve_uv() -> str:
    uv = shutil.which("uv")
    if uv is None:
        _fail(DaemonError("uv not found on PATH; install uv first"), code=1)
    return uv


def _print_step_plan(steps: list[Step], note: str) -> None:
    typer.secho(note, fg=typer.colors.YELLOW, err=True)
    for step in steps:
        typer.secho(f"  • {step.label}", fg=typer.colors.CYAN, bold=True, err=True)


def _print_results(results: list[tuple[str, StepOutcome]]) -> None:
    for label, outcome in results:
        if outcome is StepOutcome.SKIPPED:
            typer.secho(f"  · {label}", dim=True, err=True)
        else:
            typer.secho(f"  ✔ {label}", fg=typer.colors.GREEN, err=True)


def _run_sudo(argv: list[str]) -> None:
    completed = subprocess.run(argv, check=False)
    if completed.returncode != 0:
        raise typer.Exit(code=completed.returncode or 1)


@daemon_app.command("install")
def daemon_install(
    staging_dir: Annotated[
        Path | None, typer.Option("--staging-dir", help="Override the private staging directory")
    ] = None,
    source: Annotated[
        Path | None, typer.Option("--source", help="uv source spec (defaults to current checkout)")
    ] = None,
    dry_run: Annotated[
        bool, typer.Option("--dry-run", help="Print the privileged steps without escalating")
    ] = False,
) -> None:
    """Stage the unit/env/config, then install the system service via one sudo prompt."""
    resolved_source = str((source or Path.cwd()).resolve())
    resolved_uv = _resolve_uv()
    owner = getpass.getuser()
    stage_dir = staging_dir or Path(tempfile.mkdtemp(prefix="breezed-install-"))
    try:
        stage_files(stage_dir)
    except DaemonError as err:
        _fail(err, code=1)

    steps = build_steps(stage_dir, owner, resolved_uv, resolved_source)

    if os.geteuid() == 0:
        try:
            _print_results(apply(stage_dir, owner, resolved_uv, resolved_source))
        except DaemonError as err:
            _fail(err, code=1)
        print(json.dumps({"event": "install_complete"}))
        return

    _print_step_plan(steps, "The following privileged steps will run:")
    if dry_run:
        print(json.dumps({"event": "install_planned", "steps": [step.label for step in steps]}))
        return
    argv = [
        _resolve_sudo(),
        str(_resolve_self()),
        "daemon",
        "apply",
        "--staging-dir",
        str(stage_dir),
        "--owner",
        owner,
        "--uv",
        resolved_uv,
        "--source",
        resolved_source,
    ]
    _run_sudo(argv)
    print(json.dumps({"event": "install_complete"}))


@daemon_app.command("apply", hidden=True)
def daemon_apply(
    staging_dir: Annotated[Path, typer.Option()],
    owner: Annotated[str, typer.Option()],
    uv: Annotated[str, typer.Option()],
    source: Annotated[str, typer.Option()],
) -> None:
    """Internal privileged install step, invoked via sudo by `daemon install`."""
    try:
        results = apply(staging_dir, owner, uv, source)
    except DaemonError as err:
        _fail(err, code=1)
    _print_results(results)


@daemon_app.command("status")
def daemon_status_command() -> None:
    try:
        report = daemon_status()
    except DaemonError as err:
        _fail(err, code=1)
    print(json.dumps(asdict(report)))


@daemon_app.command("uninstall")
def daemon_uninstall(
    dry_run: Annotated[
        bool, typer.Option("--dry-run", help="Print the privileged steps without escalating")
    ] = False,
) -> None:
    """Stop and remove the service and runtime via one sudo prompt; keeps /etc/breezed.env/."""
    resolved_uv = _resolve_uv()

    steps = build_remove_steps(resolved_uv)

    if os.geteuid() == 0:
        try:
            _print_results(remove(resolved_uv))
        except DaemonError as err:
            _fail(err, code=1)
        print(json.dumps({"event": "uninstall_complete", "keeps": _KEEP}))
        return

    _print_step_plan(steps, "The following privileged steps will run:")
    if dry_run:
        print(
            json.dumps(
                {
                    "event": "uninstall_planned",
                    "keeps": _KEEP,
                    "steps": [step.label for step in steps],
                }
            )
        )
        return
    argv = [_resolve_sudo(), str(_resolve_self()), "daemon", "remove", "--uv", resolved_uv]
    _run_sudo(argv)
    print(json.dumps({"event": "uninstall_complete", "keeps": _KEEP}))


@daemon_app.command("remove", hidden=True)
def daemon_remove(
    uv: Annotated[str, typer.Option()],
) -> None:
    """Internal privileged uninstall step, invoked via sudo by `daemon uninstall`."""
    try:
        results = remove(uv)
    except DaemonError as err:
        _fail(err, code=1)
    _print_results(results)
