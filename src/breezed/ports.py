"""Capability Protocols: the app's interface vocabulary.

Adapters (ipmi.py today) depend on these ports; consumers depend on the ports,
never on a concrete adapter. This file owns all Protocols.
"""

from typing import Protocol, runtime_checkable

from breezed.config import Settings
from breezed.types import DomainError, FanPercent, TempC


class IpmiError(DomainError):
    """Messages carry short context plus an optional stderr snippet; never full argv."""


@runtime_checkable
class TempReader(Protocol):
    def read_max_cpu_temp(self) -> TempC: ...


@runtime_checkable
class FanCommander(Protocol):
    def enable_auto(self) -> None: ...
    def disable_auto(self) -> None: ...
    def set_manual_pct(self, pct: FanPercent) -> None: ...


class SpeedPolicy(Protocol):
    """Stateless w.r.t. config on purpose — current Settings passed each call."""

    def target_pct(self, temp_c: TempC, settings: Settings) -> int | None: ...


__all__ = ["IpmiError", "TempReader", "FanCommander", "SpeedPolicy"]
