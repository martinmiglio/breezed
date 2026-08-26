"""Runtime configuration schema (domain model).

Holds the validated Settings aggregate and its pydantic validators; all
business rules delegate to breezed.domain.types / breezed.domain.curve. The
TOML source lives in breezed.adapters.config — this module never touches files
or environment.
"""

from typing import TypeIs

from pydantic import BaseModel, ConfigDict, field_validator, model_validator

from breezed.domain.curve import CurvePoint, validate_curve
from breezed.domain.types import TempC, make_fan_pct, make_positive_int

DEFAULT_CURVE: tuple[CurvePoint, ...] = (
    CurvePoint(temp_c=TempC(45), fan_pct=make_fan_pct(6)),
    CurvePoint(temp_c=TempC(60), fan_pct=make_fan_pct(8)),
    CurvePoint(temp_c=TempC(68), fan_pct=make_fan_pct(12)),
    CurvePoint(temp_c=TempC(74), fan_pct=make_fan_pct(18)),
)

_IDENTITY_KEYS = (("host", "IDRAC_HOST"), ("user", "IDRAC_USER"))
_POSITIVE_INT_FIELDS = (
    "poll_interval_s",
    "read_failure_limit",
    "step_down_hysteresis_s",
    "metrics_port",
)
_DEFAULT_TOOL_PATH = "/usr/bin/ipmitool"


def _is_int(value: object) -> TypeIs[int]:
    # TOML has no distinct bool/int ambiguity, but Python's bool is an int and
    # `true` for poll_interval_s would otherwise silently mean 1.
    return isinstance(value, int) and not isinstance(value, bool)


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


__all__ = ["DEFAULT_CURVE", "Settings"]
