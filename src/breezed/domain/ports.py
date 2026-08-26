"""Capability Protocols: the app's interface vocabulary.

Adapters (ipmi.py today) depend on these ports; consumers depend on the ports,
never on a concrete adapter. This file owns all Protocols.
"""

from typing import Protocol, runtime_checkable

from breezed.domain.types import FanPercent, TempC


@runtime_checkable
class TempReader(Protocol):
    def read_max_cpu_temp(self) -> TempC: ...


@runtime_checkable
class FanCommander(Protocol):
    def enable_auto(self) -> None: ...
    def disable_auto(self) -> None: ...
    def set_manual_pct(self, pct: FanPercent) -> None: ...


__all__ = ["TempReader", "FanCommander"]
