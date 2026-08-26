"""Entry package: assembles the runtime app and mounts the daemon sub-app."""

from breezed.entry.daemon_cli import daemon_app
from breezed.entry.runtime import app

app.add_typer(daemon_app, name="daemon")

__all__ = ["app", "daemon_app"]
