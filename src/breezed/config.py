"""TOML config loader: structural adapter only.

Every business rule (positive ints, fan_pct bounds, curve validity) delegates to
the domain layer in breezed.types / breezed.curve. This module only handles key
presence, TOML type shape, env/file precedence, and error wrapping.
"""

import os
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypeIs

from breezed.curve import CurvePoint, validate_curve
from breezed.types import (
    Celsius,
    DomainError,
    make_fan_pct,
    make_positive_int,
)


class ConfigError(DomainError):
    """Field-naming failure; also wraps tomllib.TOMLDecodeError and OSError.

    Messages reference field names, never secret values (password discipline).
    """


DEFAULT_CURVE: tuple[CurvePoint, ...] = (
    CurvePoint(temp_c=Celsius(45), fan_pct=make_fan_pct(6)),
    CurvePoint(temp_c=Celsius(60), fan_pct=make_fan_pct(8)),
    CurvePoint(temp_c=Celsius(68), fan_pct=make_fan_pct(12)),
    CurvePoint(temp_c=Celsius(74), fan_pct=make_fan_pct(18)),
)


@dataclass(frozen=True, slots=True)
class Settings:
    host: str
    user: str
    password: str
    curve: tuple[CurvePoint, ...]
    poll_interval_s: int = 10
    read_failure_limit: int = 3
    step_down_hysteresis_s: int = 30
    metrics_port: int | None = None
    ipmitool_path: str = "/usr/bin/ipmitool"


def _is_int(value: object) -> TypeIs[int]:
    return isinstance(value, int) and not isinstance(value, bool)


def _type_name(value: object) -> str:
    return type(value).__name__


def _require_table(data: dict[str, Any], key: str) -> dict[str, Any]:
    value = data.get(key, {})
    if not isinstance(value, dict):
        msg = f"{key}: expected a table"
        raise ConfigError(msg)
    return value


def _require_str(table: dict[str, Any], key: str) -> str | None:
    if key not in table:
        return None
    value = table[key]
    if not isinstance(value, str):
        msg = f"{key}: expected a string, got {_type_name(value)}"
        raise ConfigError(msg)
    return value


def _require_int(table: dict[str, Any], key: str) -> int | None:
    if key not in table:
        return None
    value = table[key]
    if not _is_int(value):
        msg = f"{key}: expected an integer, got {_type_name(value)}"
        raise ConfigError(msg)
    return value


def _positive_or_default(table: dict[str, Any], key: str, default: int) -> int:
    raw = _require_int(table, key)
    if raw is None:
        return default
    try:
        return make_positive_int(key, raw)
    except ValueError as exc:
        raise ConfigError(str(exc)) from exc


def _metrics_port(table: dict[str, Any]) -> int | None:
    raw = _require_int(table, "metrics_port")
    if raw is None:
        return None
    try:
        return make_positive_int("metrics_port", raw)
    except ValueError as exc:
        raise ConfigError(str(exc)) from exc


def _secret(table: dict[str, Any], key: str, env_key: str) -> str | None:
    env_value = os.environ.get(env_key)
    if env_value:
        return env_value
    return _require_str(table, key)


def _identity(settings_table: dict[str, Any]) -> tuple[str, str]:
    host = _secret(settings_table, "host", "IDRAC_HOST")
    user = _secret(settings_table, "user", "IDRAC_USER")
    missing = [
        f"{name}: missing (set [settings].{name} or {env})"
        for name, value, env in (
            ("host", host, "IDRAC_HOST"),
            ("user", user, "IDRAC_USER"),
        )
        if value is None
    ]
    if missing:
        raise ConfigError("; ".join(missing))
    return host or "", user or ""


def _curve_row(index: int, row: dict[str, Any]) -> CurvePoint:
    temp_raw = row.get("temp_c")
    pct_raw = row.get("fan_pct")
    if not _is_int(temp_raw):
        msg = f"curve[{index}].temp_c: expected an integer, got {_type_name(temp_raw)}"
        raise ConfigError(msg)
    if not _is_int(pct_raw):
        msg = f"curve[{index}].fan_pct: expected an integer, got {_type_name(pct_raw)}"
        raise ConfigError(msg)
    try:
        fan_pct = make_fan_pct(pct_raw)
    except ValueError as exc:
        msg = f"curve[{index}].{exc}"
        raise ConfigError(msg) from exc
    return CurvePoint(temp_c=Celsius(temp_raw), fan_pct=fan_pct)


def _curve(data: dict[str, Any]) -> tuple[CurvePoint, ...]:
    rows = data.get("curve")
    if rows is None:
        return DEFAULT_CURVE
    if not isinstance(rows, list) or any(not isinstance(r, dict) for r in rows):
        msg = "curve: expected an array of tables"
        raise ConfigError(msg)
    if not rows:
        return DEFAULT_CURVE
    points = tuple(_curve_row(i, r) for i, r in enumerate(rows))
    try:
        return validate_curve(points)
    except ValueError as exc:
        raise ConfigError(str(exc)) from exc


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

    settings_table = _require_table(data, "settings")
    host, user = _identity(settings_table)
    password = _secret(settings_table, "password", "IDRAC_PASSWORD") or ""
    ipmitool_path = _require_str(settings_table, "ipmitool_path") or "/usr/bin/ipmitool"
    return Settings(
        host=host,
        user=user,
        password=password,
        curve=_curve(data),
        poll_interval_s=_positive_or_default(settings_table, "poll_interval_s", 10),
        read_failure_limit=_positive_or_default(settings_table, "read_failure_limit", 3),
        step_down_hysteresis_s=_positive_or_default(settings_table, "step_down_hysteresis_s", 30),
        metrics_port=_metrics_port(settings_table),
        ipmitool_path=ipmitool_path,
    )


class ConfigWatcher:
    """mtime_ns-tracked hot-reload helper; caller owns the last-good Settings."""

    def __init__(self, path: str | Path) -> None:
        self._path = Path(path)
        self._mtime_ns = self._path.stat().st_mtime_ns

    def changed(self) -> bool:
        try:
            return self._path.stat().st_mtime_ns != self._mtime_ns
        except OSError:
            return True

    def reload(self) -> Settings:
        try:
            mtime_ns = self._path.stat().st_mtime_ns
        except OSError:
            mtime_ns = None
        settings = load_settings(self._path)
        if mtime_ns is not None:
            self._mtime_ns = mtime_ns
        return settings


__all__ = ["ConfigError", "ConfigWatcher", "DEFAULT_CURVE", "Settings", "load_settings"]
