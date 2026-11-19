"""Capability Protocols: the app's interface vocabulary.

Adapters (ipmi.py today) depend on these ports; consumers depend on the ports,
never on a concrete adapter. This file owns all Protocols.
"""

from collections.abc import Sequence
from typing import Protocol, runtime_checkable

from breezed.domain.curve import CurvePoint
from breezed.domain.types import EventType, FanPercent, TempC


@runtime_checkable
class TempReader(Protocol):
    def read_max_cpu_temp(self) -> TempC: ...


@runtime_checkable
class FanCommander(Protocol):
    def enable_auto(self) -> None: ...
    def disable_auto(self) -> None: ...
    def set_manual_pct(self, pct: FanPercent) -> None: ...


@runtime_checkable
class EventSink(Protocol):
    def emit(self, event: EventType, /, **fields: object) -> None: ...


@runtime_checkable
class SpeedPolicy(Protocol):
    """Maps a curve and a temperature to a fan target (None means AUTO).

    A plain callable conforms structurally; ``curve.interpolate`` is the
    default policy wired in ``entry``.
    """

    def __call__(self, curve: Sequence[CurvePoint], temp_c: TempC) -> int | None: ...


__all__ = ["EventSink", "FanCommander", "SpeedPolicy", "TempReader"]
