"""mtime_ns-tracked config hot-reload.

The caller owns the last-good Settings: reload() either returns a fresh
Settings or raises ConfigError, leaving the previous value untouched.
"""

from pathlib import Path

from breezed.adapters.config import ConfigError, load_settings
from breezed.domain.settings import Settings

__all__ = ["ConfigWatcher"]


def _cannot_read(path: str | Path, exc: OSError) -> ConfigError:
    msg = f"config: cannot read {path}: {exc}"
    return ConfigError(msg)


class ConfigWatcher:
    """Reloads only when the file's mtime_ns moved; a vanished file counts as changed."""

    def __init__(self, path: str | Path) -> None:
        self._path = Path(path)
        try:
            self._mtime_ns = self._path.stat().st_mtime_ns
        except OSError as exc:
            raise _cannot_read(path, exc) from exc

    def changed(self) -> bool:
        try:
            return self._path.stat().st_mtime_ns != self._mtime_ns
        except OSError:
            return True

    def reload(self) -> Settings:
        # Stat before load so the recorded mtime never describes a version newer
        # than what was parsed: a write racing the load leaves the file's mtime
        # ahead of _mtime_ns, and changed() schedules another reload (the TOCTOU
        # gap is benign by construction, not eliminated).
        try:
            mtime_ns = self._path.stat().st_mtime_ns
        except OSError:
            mtime_ns = None
        settings = load_settings(self._path)
        if mtime_ns is not None:
            self._mtime_ns = mtime_ns
        return settings
