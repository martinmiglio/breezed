"""Control-loop state machine: hysteresis, failure fallback, hot-reload intake.

No I/O beyond the TempReader/FanCommander protocol calls; every observable
action flows through the injected EventSink. Hysteresis and AUTO fallback live
here so they apply uniformly to every target the curve produces.
"""

import time
from collections.abc import Callable, Sequence
from typing import Protocol

from breezed.domain.curve import interpolate, validate_curve
from breezed.domain.ports import FanCommander, TempReader
from breezed.domain.settings import Settings
from breezed.domain.types import EventType, FanPercent, IpmiError, OperatingMode, make_fan_pct


class EventSink(Protocol):
    def emit(self, event: EventType, /, **fields: object) -> None: ...


class Controller:
    """Each tick(): read -> decide target via the curve -> command hardware -> emit events."""

    def __init__(
        self,
        reader: TempReader,
        commander: FanCommander,
        settings: Settings,
        sinks: Sequence[EventSink],
        *,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._reader = reader
        self._commander = commander
        self._settings = settings
        self._sinks = sinks
        self._clock = clock
        self._state = OperatingMode.UNKNOWN
        self._last_pct: FanPercent | None = None
        self._failure_streak = 0
        self._write_failures = 0
        self._pending_down: tuple[FanPercent, float] | None = None

    def _emit(self, event: EventType, /, **fields: object) -> None:
        for sink in self._sinks:
            sink.emit(event, **fields)

    def _command(self, operation: str, call: Callable[[], None], *, reason: str) -> bool:
        """Run one commander write; report a failure instead of letting it escape.

        An IPMI write timeout must not kill the control loop: the caller leaves
        its state untouched, so the next tick can retry from the last known-good
        state. Failures are emitted with the operation, reason, and the current
        consecutive-write-failure count.
        """
        try:
            call()
        except IpmiError as exc:
            self._write_failures += 1
            self._emit(
                EventType.IPMI_ERROR,
                operation=operation,
                reason=reason,
                failures=self._write_failures,
                error=str(exc),
            )
            return False
        self._write_failures = 0
        return True

    def _apply_speed(self, pct: FanPercent, target: int, *, reason: str) -> bool:
        if not self._command(
            "set_manual_pct",
            lambda: self._commander.set_manual_pct(pct),
            reason=reason,
        ):
            return False
        self._last_pct = pct
        self._emit(
            EventType.SPEED_CHANGE,
            fan_pct=pct,
            target_pct=target,
            reason=reason,
        )
        return True

    def _force_auto(self, reason: str, **fields: object) -> None:
        if not self._command("enable_auto", self._commander.enable_auto, reason=reason):
            return
        old = self._state.value
        self._state = OperatingMode.AUTO
        self._emit(
            EventType.MODE_CHANGE,
            **{"from": old},
            to=OperatingMode.AUTO.value,
            reason=reason,
            **fields,
        )

    def _enter_manual(self, pct: FanPercent, target: int) -> None:
        if not self._command(
            "disable_auto", self._commander.disable_auto, reason="temp_under_curve"
        ):
            return
        if not self._command(
            "set_manual_pct",
            lambda: self._commander.set_manual_pct(pct),
            reason="temp_under_curve",
        ):
            return
        old = self._state.value
        self._state = OperatingMode.MANUAL
        self._last_pct = pct
        self._pending_down = None
        self._emit(
            EventType.MODE_CHANGE,
            **{"from": old},
            to=OperatingMode.MANUAL.value,
            reason="temp_under_curve",
        )
        self._emit(
            EventType.SPEED_CHANGE,
            fan_pct=pct,
            target_pct=target,
            reason="mode_enter",
        )

    def _gate_downward(self, target: int, pct: FanPercent) -> None:
        if self._pending_down is None:
            self._pending_down = (pct, self._clock())
            self._emit(
                EventType.HYSTERESIS_WAIT,
                target_pct=target,
                hysteresis_s=self._settings.step_down_hysteresis_s,
            )
        elif self._clock() - self._pending_down[1] >= self._settings.step_down_hysteresis_s:
            applied = self._pending_down[0]
            if self._apply_speed(applied, target, reason="hysteresis_elapsed"):
                self._pending_down = None

    def tick(self) -> None:
        try:
            temp = self._reader.read_max_cpu_temp()
        except IpmiError as exc:
            self._failure_streak += 1
            self._emit(EventType.IPMI_ERROR, failures=self._failure_streak, error=str(exc))
            if (
                self._failure_streak >= self._settings.read_failure_limit
                and self._state is not OperatingMode.AUTO
            ):
                self._force_auto("read_failures", failures=self._failure_streak)
            return

        self._failure_streak = 0
        target = interpolate(self._settings.curve, temp)

        if target is None:
            self._pending_down = None
            if self._state is not OperatingMode.AUTO:
                self._force_auto("temp_above_curve", temp_c=temp)
        else:
            try:
                pct = make_fan_pct(target)
            except ValueError as exc:
                self._emit(EventType.CONFIG_ERROR, error=str(exc))
            else:
                if self._state is not OperatingMode.MANUAL:
                    self._enter_manual(pct, target)
                else:
                    assert self._last_pct is not None
                    last_pct = self._last_pct
                    if target > last_pct:
                        self._apply_speed(pct, target, reason="temp_rise")
                        self._pending_down = None
                    elif target < last_pct:
                        self._gate_downward(target, pct)
                    else:
                        self._pending_down = None

        self._emit(
            EventType.POLL,
            temp_c=temp,
            fan_pct=self._last_pct,
            mode=self._state.value,
            target_pct=target,
        )

    def shutdown(self) -> None:
        """Restore AUTO unless already there or no command was ever issued.

        A failing enable_auto is reported as an IPMI_ERROR event, never raised.
        """
        if self._state is not OperatingMode.MANUAL:
            return
        self._force_auto("shutdown")

    def replace_settings(self, new_settings: Settings) -> bool:
        """Swap in hot-reloaded settings; False (plus CONFIG_ERROR event) on invalid curve."""
        try:
            validate_curve(new_settings.curve)
        except ValueError as exc:
            self._emit(EventType.CONFIG_ERROR, error=str(exc))
            return False
        self._settings = new_settings
        self._pending_down = None
        return True


__all__ = ["Controller", "EventSink"]
