"""TOML config loader: structural adapter only.

Reads the file, merges environment overrides, and validates via the domain
Settings schema in breezed.domain.settings. Every business rule lives in the
domain layer; this module only handles key presence, TOML type shape, env/file
precedence, and error wrapping.
"""

import os
import tomllib
from pathlib import Path

from pydantic import ValidationError

from breezed.domain.settings import Settings
from breezed.domain.types import DomainError


class ConfigError(DomainError):
    """Field-naming failure; also wraps TOML decode, OS, and validation errors.

    Messages reference field names, never secret values (password discipline).
    """


_ENV_OVERRIDES = (("host", "IDRAC_HOST"), ("user", "IDRAC_USER"), ("password", "IDRAC_PASSWORD"))


def load_settings(path: str | Path) -> Settings:
    """Binary-mode tomllib load; env wins over file for host/user/password.

    Omitted/empty [[curve]] falls back to DEFAULT_CURVE; unknown keys are ignored
    everywhere. All business rules live in the domain layer.
    """
    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
    except tomllib.TOMLDecodeError as exc:
        msg = f"config: invalid TOML in {path}: {exc}"
        raise ConfigError(msg) from exc
    except OSError as exc:
        msg = f"config: cannot read {path}: {exc}"
        raise ConfigError(msg) from exc

    raw_settings = data.get("settings")
    settings_table = dict(raw_settings) if isinstance(raw_settings, dict) else {}
    for name, env_key in _ENV_OVERRIDES:
        # An empty env value counts as unset, same as absent.
        env_value = os.environ.get(env_key)
        if env_value:
            settings_table[name] = env_value
    try:
        # [[curve]] is a top-level TOML table array, sibling of [settings].
        return Settings.model_validate({**settings_table, "curve": data.get("curve")})
    except ValidationError as exc:
        msg = "; ".join(str(error["msg"]) for error in exc.errors())
        raise ConfigError(msg) from exc


__all__ = ["ConfigError", "load_settings"]
