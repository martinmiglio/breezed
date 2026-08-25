"""Control-loop state machine: hysteresis, failure fallback, hot-reload intake.

No I/O beyond the TempReader/FanCommander protocol calls; every observable
action flows through the injected EventSink. Hysteresis and AUTO fallback are
central here so every SpeedPolicy strategy inherits them for free.
"""

import time
from collections.abc import Callable
from enum import StrEnum
from typing import Protocol

from breezed.config import Settings
from breezed.curve import validate_curve
from breezed.policy import CurvePolicy
from breezed.ports import FanCommander, IpmiError, SpeedPolicy, TempReader
from breezed.types import EventType, FanPercent, make_fan_pct


class ControlState(StrEnum):
    """This machine's belief about the iDRAC — distinct from T2's OperatingMode."""

    UNKNOWN = "unknown"
    AUTO = "auto"
    MANUAL = "manual"


class EventSink(Protocol):
    def emit(self, event: EventType, /, **fields: object) -> None: ...


class Controller:
    """Each tick(): read -> decide via policy -> command hardware -> emit events."""

    def __init__(
        self,
        reader: TempReader,
        commander: FanCommander,
        settings: Settings,
        sink: EventSink,
        *,
        clock: Callable[[], float] = time.monotonic,
        policy: SpeedPolicy | None = None,
    ) -> None:
        self._reader = reader
        self._commander = commander
        self._settings = settings
        self._sink = sink
        self._clock = clock
        self._policy: SpeedPolicy = policy if policy is not None else CurvePolicy()
        self._state = ControlState.UNKNOWN
        self._last_pct: FanPercent | None = None
        self._failure_streak = 0
        self._pending_down: tuple[FanPercent, float] | None = None

    def _force_auto(self, reason: str, **fields: object) -> None:
        old = self._state.value
        self._commander.enable_auto()
        self._state = ControlState.AUTO
        self._sink.emit(
            EventType.MODE_CHANGE,
            **{"from": old},
            to=ControlState.AUTO.value,
            reason=reason,
            **fields,
        )

    def _enter_manual(self, pct: FanPercent, target: int) -> None:
        old = self._state.value
        self._commander.disable_auto()
        self._commander.set_manual_pct(pct)
        self._state = ControlState.MANUAL
        self._last_pct = pct
        self._pending_down = None
        self._sink.emit(
            EventType.MODE_CHANGE,
            **{"from": old},
            to=ControlState.MANUAL.value,
            reason="temp_under_curve",
        )
        self._sink.emit(
            EventType.SPEED_CHANGE,
            fan_pct=pct,
            target_pct=target,
            reason="mode_enter",
        )

    def _gate_downward(self, target: int, pct: FanPercent) -> None:
        if self._pending_down is None:
            self._pending_down = (pct, self._clock())
            self._sink.emit(
                EventType.HYSTERESIS_WAIT,
                target_pct=target,
                hysteresis_s=self._settings.step_down_hysteresis_s,
            )
        elif self._clock() - self._pending_down[1] >= self._settings.step_down_hysteresis_s:
            applied = self._pending_down[0]
            self._commander.set_manual_pct(applied)
            self._last_pct = applied
            self._pending_down = None
            self._sink.emit(
                EventType.SPEED_CHANGE,
                fan_pct=applied,
                target_pct=target,
                reason="hysteresis_elapsed",
            )

    def tick(self) -> None:
        try:
            temp = self._reader.read_max_cpu_temp()
        except IpmiError as exc:
            self._failure_streak += 1
            self._sink.emit(EventType.IPMI_ERROR, failures=self._failure_streak, error=str(exc))
            if (
                self._failure_streak >= self._settings.read_failure_limit
                and self._state is not ControlState.AUTO
            ):
                self._force_auto("read_failures", failures=self._failure_streak)
            return

        self._failure_streak = 0
        target = self._policy.target_pct(temp, self._settings)

        if target is None:
            self._pending_down = None
            if self._state is not ControlState.AUTO:
                self._force_auto("temp_above_curve", temp_c=temp)
        else:
            try:
                pct = make_fan_pct(target)
            except ValueError as exc:
                self._sink.emit(EventType.CONFIG_ERROR, error=str(exc))
            else:
                if self._state is not ControlState.MANUAL or self._last_pct is None:
                    self._enter_manual(pct, target)
                elif target > self._last_pct:
                    self._commander.set_manual_pct(pct)
                    self._last_pct = pct
                    self._pending_down = None
                    self._sink.emit(
                        EventType.SPEED_CHANGE,
                        fan_pct=pct,
                        target_pct=target,
                        reason="temp_rise",
                    )
                elif target < self._last_pct:
                    self._gate_downward(target, pct)
                else:
                    self._pending_down = None

        self._sink.emit(
            EventType.POLL,
            temp_c=temp,
            fan_pct=self._last_pct,
            mode=self._state.value,
            target_pct=target,
        )

    def shutdown(self) -> None:
        """Restore AUTO unless already there or no command was ever issued."""
        if self._state in (ControlState.AUTO, ControlState.UNKNOWN):
            return
        old = self._state.value
        try:
            self._commander.enable_auto()
        except IpmiError as exc:
            self._sink.emit(EventType.IPMI_ERROR, error=str(exc))
            return
        self._state = ControlState.AUTO
        self._sink.emit(
            EventType.MODE_CHANGE,
            **{"from": old},
            to=ControlState.AUTO.value,
            reason="shutdown",
        )

    def replace_settings(self, new_settings: Settings) -> bool:
        """Swap in hot-reloaded settings; False (plus CONFIG_ERROR event) on invalid curve."""
        try:
            validate_curve(new_settings.curve)
        except ValueError as exc:
            self._sink.emit(EventType.CONFIG_ERROR, error=str(exc))
            return False
        self._settings = new_settings
        self._pending_down = None
        return True


__all__ = ["ControlState", "Controller", "EventSink"]
