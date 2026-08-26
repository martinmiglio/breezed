"""TOML config loader: structural adapter only.

Every business rule (positive ints, fan_pct bounds, curve validity) delegates to
the domain layer in breezed.types / breezed.curve. This module only handles key
presence, TOML type shape, env/file precedence, and error wrapping.
"""

import os
import tomllib
from pathlib import Path
from typing import TypeIs

from pydantic import BaseModel, ConfigDict, ValidationError, field_validator, model_validator

from breezed.curve import CurvePoint, validate_curve
from breezed.types import DomainError, TempC, make_fan_pct, make_positive_int


class ConfigError(DomainError):
    """Field-naming failure; also wraps TOML decode, OS, and validation errors.

    Messages reference field names, never secret values (password discipline).
    """


DEFAULT_CURVE: tuple[CurvePoint, ...] = (
    CurvePoint(temp_c=TempC(45), fan_pct=make_fan_pct(6)),
    CurvePoint(temp_c=TempC(60), fan_pct=make_fan_pct(8)),
    CurvePoint(temp_c=TempC(68), fan_pct=make_fan_pct(12)),
    CurvePoint(temp_c=TempC(74), fan_pct=make_fan_pct(18)),
)


def _is_int(value: object) -> TypeIs[int]:
    # TOML has no distinct bool/int ambiguity, but Python's bool is an int and
    # `true` for poll_interval_s would otherwise silently mean 1.
    return isinstance(value, int) and not isinstance(value, bool)


_IDENTITY_KEYS = (("host", "IDRAC_HOST"), ("user", "IDRAC_USER"))
_ENV_OVERRIDES = (*_IDENTITY_KEYS, ("password", "IDRAC_PASSWORD"))
_POSITIVE_INT_FIELDS = (
    "poll_interval_s",
    "read_failure_limit",
    "step_down_hysteresis_s",
    "metrics_port",
)
_DEFAULT_TOOL_PATH = "/usr/bin/ipmitool"


def _curve_row(index: int, row: dict[str, object]) -> CurvePoint:
    def require(column: str) -> int:
        value = row.get(column)
        if not _is_int(value):
            got = type(value).__name__
            msg = f"curve[{index}].{column}: expected an integer, got {got}"
            raise ValueError(msg)
        return value

    temp_raw = require("temp_c")
    pct_raw = require("fan_pct")
    try:
        fan_pct = make_fan_pct(pct_raw)
    except ValueError as exc:
        msg = f"curve[{index}].{exc}"
        raise ValueError(msg) from exc
    return CurvePoint(temp_c=TempC(temp_raw), fan_pct=fan_pct)


class Settings(BaseModel):
    """Validated runtime configuration; frozen so consumers can share it safely."""

    model_config = ConfigDict(frozen=True)

    host: str
    user: str
    password: str = ""
    curve: tuple[CurvePoint, ...] = DEFAULT_CURVE
    poll_interval_s: int = 10
    read_failure_limit: int = 3
    step_down_hysteresis_s: int = 30
    metrics_port: int | None = None
    ipmitool_path: str = _DEFAULT_TOOL_PATH

    @model_validator(mode="before")
    @classmethod
    def _require_identity(cls, data: object) -> object:
        if not isinstance(data, dict):
            return data
        missing = []
        for name, env_key in _IDENTITY_KEYS:
            value = data.get(name)
            if isinstance(value, str) and value:
                continue
            missing.append(f"{name}: missing (set [settings].{name} or {env_key})")
        if missing:
            msg = "; ".join(missing)
            raise ValueError(msg)
        return data

    @model_validator(mode="before")
    @classmethod
    def _check_int_fields(cls, data: object) -> object:
        if not isinstance(data, dict):
            return data
        for key in _POSITIVE_INT_FIELDS:
            value = data.get(key)
            if value is None:
                continue
            if not _is_int(value):
                got = type(value).__name__
                msg = f"{key}: expected an integer, got {got}"
                raise ValueError(msg)
            data[key] = make_positive_int(key, value)
        return data

    @field_validator("curve", mode="before")
    @classmethod
    def _parse_curve(cls, value: object) -> object:
        if value is None or (isinstance(value, list) and not value):
            return DEFAULT_CURVE
        # Programmatic construction (tests, callers) passes validated points directly.
        if isinstance(value, tuple) and all(isinstance(p, CurvePoint) for p in value):
            return value
        if not isinstance(value, list) or any(not isinstance(r, dict) for r in value):
            msg = "curve: expected an array of tables"
            raise ValueError(msg)
        points = tuple(_curve_row(i, r) for i, r in enumerate(value))
        try:
            return validate_curve(points)
        except ValueError as exc:
            msg = f"curve: {exc}"
            raise ValueError(msg) from exc

    @field_validator("ipmitool_path", mode="before")
    @classmethod
    def _empty_tool_path_falls_back(cls, value: object) -> object:
        return value or _DEFAULT_TOOL_PATH


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


__all__ = ["ConfigError", "DEFAULT_CURVE", "Settings", "load_settings"]
