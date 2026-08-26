"""Domain type wrappers, validated constructors, and enums.

All business-rule validation lives here (or in curve.py) so adapters hold
zero rules of their own.
"""

from enum import StrEnum
from typing import NewType

TempC = NewType("TempC", int)
FanPercent = NewType("FanPercent", int)


def make_fan_pct(value: int) -> FanPercent:
    """Validated constructor; raises ValueError outside 1..100."""
    if not 1 <= value <= 100:
        msg = f"fan_pct must be in 1..100, got {value}"
        raise ValueError(msg)
    return FanPercent(value)


def make_positive_int(name: str, value: int) -> int:
    """Interval-style setting; raises ValueError('{name} must be > 0, got {value}')."""
    if value <= 0:
        msg = f"{name} must be > 0, got {value}"
        raise ValueError(msg)
    return value


class DomainError(ValueError):
    """Shared base so config and ipmi errors can be caught as one validation family."""


class IpmiError(DomainError):
    """Raised by the IPMI adapter on subprocess / protocol failures."""


class OperatingMode(StrEnum):
    """Domain vocabulary; members serialize via str() straight into JSON logs."""

    UNKNOWN = "unknown"
    AUTO = "auto"
    MANUAL = "manual"


class EventType(StrEnum):
    """Closed log-event vocabulary; log consumers can match exhaustively on it."""

    STARTUP = "startup"
    POLL = "poll"
    MODE_CHANGE = "mode_change"
    SPEED_CHANGE = "speed_change"
    HYSTERESIS_WAIT = "hysteresis_wait"
    CONFIG_RELOAD = "config_reload"
    CONFIG_ERROR = "config_error"
    IPMI_ERROR = "ipmi_error"
    SHUTDOWN = "shutdown"


__all__ = [
    "DomainError",
    "EventType",
    "FanPercent",
    "IpmiError",
    "OperatingMode",
    "TempC",
    "make_fan_pct",
    "make_positive_int",
]
