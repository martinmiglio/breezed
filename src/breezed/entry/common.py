"""Shared CLI helpers used by both the runtime and daemon entrypoints."""

from typing import NoReturn

import typer


def _fail(err: Exception, *, code: int) -> NoReturn:
    typer.secho(f"breezed: {err}", fg=typer.colors.RED, err=True)
    raise typer.Exit(code=code) from err
