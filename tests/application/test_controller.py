"""SPEC controller cases 1-12 plus companion coverage; fakes only, no sleeps."""

from breezed.adapters.ipmi import IpmiError
from breezed.application.controller import Controller, ControlState
from breezed.domain.curve import CurvePoint
from breezed.domain.settings import Settings
from breezed.domain.types import EventType, FanPercent, TempC

DEFAULT_CURVE = (
    CurvePoint(TempC(45), FanPercent(6)),
    CurvePoint(TempC(60), FanPercent(8)),
    CurvePoint(TempC(68), FanPercent(12)),
    CurvePoint(TempC(74), FanPercent(18)),
)


def make_settings(
    curve: tuple[CurvePoint, ...] = DEFAULT_CURVE,
) -> Settings:
    return Settings(
        host="h",
        user="u",
        password="",
        curve=curve,
    )


class FakeClock:
    def __init__(self, start: float = 0.0) -> None:
        self.now = start

    def advance(self, seconds: float) -> None:
        self.now += seconds

    def __call__(self) -> float:
        return self.now


class FakeIpmi:
    def __init__(
        self,
        script: list[TempC | IpmiError] | None = None,
        *,
        fail_enable_auto: bool = False,
    ) -> None:
        self.script: list[TempC | IpmiError] = list(script) if script else []
        self.commands: list[str] = []
        self.fail_enable_auto = fail_enable_auto

    def read_max_cpu_temp(self) -> TempC:
        if not self.script:
            raise AssertionError("FakeIpmi script exhausted")
        item = self.script.pop(0)
        if isinstance(item, IpmiError):
            raise item
        return item

    def enable_auto(self) -> None:
        if self.fail_enable_auto:
            raise IpmiError("enable_auto failed")
        self.commands.append("auto")

    def disable_auto(self) -> None:
        self.commands.append("manual")

    def set_manual_pct(self, pct: FanPercent) -> None:
        self.commands.append(f"set:{pct}")


class RecordingSink:
    def __init__(self) -> None:
        self.records: list[tuple[EventType, dict[str, object]]] = []

    def emit(self, event: EventType, /, **fields: object) -> None:
        self.records.append((event, fields))

    def events(self, name: EventType) -> list[dict[str, object]]:
        return [fields for event, fields in self.records if event == name]


def make_controller(
    script: list[TempC | IpmiError],
    *,
    settings: Settings | None = None,
    clock: FakeClock | None = None,
) -> tuple[Controller, FakeIpmi, RecordingSink]:
    ipmi = FakeIpmi(script)
    sink = RecordingSink()
    controller = Controller(
        ipmi,
        ipmi,
        settings if settings is not None else make_settings(),
        sink,
        clock=clock if clock is not None else FakeClock(),
    )
    return controller, ipmi, sink


def read_error() -> IpmiError:
    return IpmiError("sdr failed")


def test_cold_start_no_fan_commands_before_first_read():
    controller, ipmi, _sink = make_controller([read_error(), read_error(), TempC(40)])
    controller.tick()
    controller.tick()
    assert ipmi.commands == []
    controller.tick()
    assert ipmi.commands == ["manual", "set:6"]


def test_under_curve_from_unknown_switches_to_manual_with_pct():
    controller, ipmi, sink = make_controller([TempC(40)])
    controller.tick()
    assert ipmi.commands == ["manual", "set:6"]
    assert sink.events(EventType.MODE_CHANGE) == [
        {"from": "unknown", "to": "manual", "reason": "temp_under_curve"}
    ]
    assert sink.events(EventType.SPEED_CHANGE) == [
        {"fan_pct": 6, "target_pct": 6, "reason": "mode_enter"}
    ]


def test_returns_under_curve_after_hysteresis_resumes_manual():
    controller, ipmi, sink = make_controller([TempC(66), TempC(80), TempC(62)])
    controller.tick()
    controller.tick()
    controller.tick()
    assert ipmi.commands == ["manual", "set:11", "auto", "manual", "set:9"]
    assert sink.events(EventType.MODE_CHANGE)[-1] == {
        "from": "auto",
        "to": "manual",
        "reason": "temp_under_curve",
    }
    assert sink.events(EventType.SPEED_CHANGE)[-2] == {
        "fan_pct": 11,
        "target_pct": 11,
        "reason": "mode_enter",
    }
    assert sink.events(EventType.SPEED_CHANGE)[-1] == {
        "fan_pct": 9,
        "target_pct": 9,
        "reason": "mode_enter",
    }


def test_third_consecutive_failure_forces_auto_once():
    controller, ipmi, sink = make_controller(
        [read_error(), read_error(), read_error(), read_error()]
    )
    controller.tick()
    controller.tick()
    assert ipmi.commands == []
    controller.tick()
    controller.tick()
    assert ipmi.commands == ["auto"]
    assert len(sink.events(EventType.MODE_CHANGE)) == 1
    assert sink.events(EventType.MODE_CHANGE)[0] == {
        "from": "unknown",
        "to": "auto",
        "reason": "read_failures",
        "failures": 3,
    }
    assert len(sink.events(EventType.IPMI_ERROR)) == 4


def test_good_read_resets_failure_streak():
    controller, ipmi, _sink = make_controller(
        [read_error(), read_error(), TempC(40), read_error(), read_error()]
    )
    for _ in range(5):
        controller.tick()
    assert ipmi.commands == ["manual", "set:6"]


def test_upward_speed_change_applies_immediately():
    controller, ipmi, sink = make_controller([TempC(60), TempC(68)])
    controller.tick()
    controller.tick()
    assert ipmi.commands == ["manual", "set:8", "set:12"]
    assert sink.events(EventType.SPEED_CHANGE)[-1] == {
        "fan_pct": 12,
        "target_pct": 12,
        "reason": "temp_rise",
    }


def test_downward_change_applies_only_after_full_hysteresis():
    clock = FakeClock()
    controller, ipmi, sink = make_controller(
        [TempC(68), TempC(60), TempC(60), TempC(60)], clock=clock
    )
    controller.tick()
    controller.tick()
    assert ipmi.commands == ["manual", "set:12"]
    assert len(sink.events(EventType.HYSTERESIS_WAIT)) == 1
    clock.advance(29)
    controller.tick()
    assert ipmi.commands == ["manual", "set:12"]
    assert len(sink.events(EventType.HYSTERESIS_WAIT)) == 1
    clock.advance(1)
    controller.tick()
    assert ipmi.commands == ["manual", "set:12", "set:8"]
    assert sink.events(EventType.SPEED_CHANGE)[-1] == {
        "fan_pct": 8,
        "target_pct": 8,
        "reason": "hysteresis_elapsed",
    }


def test_midwindow_rise_cancels_pending_down_step():
    clock = FakeClock()
    controller, ipmi, sink = make_controller(
        [TempC(68), TempC(60), TempC(70), TempC(60)], clock=clock
    )
    controller.tick()
    controller.tick()
    clock.advance(15)
    controller.tick()
    assert ipmi.commands == ["manual", "set:12", "set:14"]
    clock.advance(100)
    controller.tick()
    assert ipmi.commands == ["manual", "set:12", "set:14"]
    assert len(sink.events(EventType.HYSTERESIS_WAIT)) == 2


def test_equal_target_issues_no_command():
    controller, ipmi, sink = make_controller([TempC(60), TempC(60)])
    controller.tick()
    controller.tick()
    assert ipmi.commands == ["manual", "set:8"]
    speed_events = sink.events(EventType.SPEED_CHANGE)
    assert len(speed_events) == 1
    assert speed_events[0]["reason"] == "mode_enter"
    assert len(sink.events(EventType.POLL)) == 2


def test_shutdown_restores_auto_even_from_manual():
    controller, ipmi, sink = make_controller([TempC(60)])
    controller.tick()
    controller.shutdown()
    assert ipmi.commands == ["manual", "set:8", "auto"]
    assert sink.events(EventType.MODE_CHANGE)[-1] == {
        "from": "manual",
        "to": "auto",
        "reason": "shutdown",
    }
    controller.shutdown()
    assert ipmi.commands == ["manual", "set:8", "auto"]
    assert len(sink.events(EventType.MODE_CHANGE)) == 2


def test_invalid_hot_reload_keeps_last_good_config_and_logs_config_error():
    controller, ipmi, sink = make_controller([TempC(60)])
    bad_settings = make_settings(
        curve=(
            CurvePoint(TempC(60), FanPercent(8)),
            CurvePoint(TempC(50), FanPercent(6)),
        )
    )
    assert controller.replace_settings(bad_settings) is False
    config_errors = sink.events(EventType.CONFIG_ERROR)
    assert len(config_errors) == 1
    assert "strictly ascending" in str(config_errors[0]["error"])
    controller.tick()
    assert ipmi.commands == ["manual", "set:8"]


def test_failure_limit_idempotent_while_already_auto():
    controller, ipmi, _sink = make_controller([TempC(80), read_error(), read_error(), read_error()])
    for _ in range(4):
        controller.tick()
    assert ipmi.commands == ["auto"]


def test_replace_settings_success_uses_new_curve_next_tick():
    controller, ipmi, sink = make_controller(
        [TempC(46)], settings=make_settings(curve=(CurvePoint(TempC(40), FanPercent(5)),))
    )
    new_settings = make_settings(
        curve=(
            CurvePoint(TempC(45), FanPercent(6)),
            CurvePoint(TempC(47), FanPercent(30)),
        )
    )
    assert controller.replace_settings(new_settings) is True
    assert sink.events(EventType.CONFIG_ERROR) == []
    controller.tick()
    assert ipmi.commands == ["manual", "set:18"]


def test_shutdown_swallows_ipmi_error_from_enable_auto():
    ipmi = FakeIpmi([TempC(60)], fail_enable_auto=True)
    sink = RecordingSink()
    controller = Controller(ipmi, ipmi, make_settings(), sink)
    controller.tick()
    controller.shutdown()
    assert sink.events(EventType.IPMI_ERROR) == [{"error": "enable_auto failed"}]
    assert controller._state is ControlState.MANUAL


def test_curve_target_drives_controller_to_manual():
    # Flat curve pins the target at 42 regardless of temperature.
    settings = make_settings(
        curve=(
            CurvePoint(TempC(0), FanPercent(42)),
            CurvePoint(TempC(100), FanPercent(42)),
        )
    )
    controller, ipmi, _sink = make_controller([TempC(30), TempC(85)], settings=settings)
    controller.tick()
    assert ipmi.commands == ["manual", "set:42"]
    controller.tick()
    assert ipmi.commands == ["manual", "set:42"]


def test_poll_event_carries_full_field_set_in_auto():
    controller, ipmi, sink = make_controller([TempC(80)])
    controller.tick()
    assert sink.events(EventType.MODE_CHANGE) == [
        {"from": "unknown", "to": "auto", "reason": "temp_above_curve", "temp_c": 80}
    ]
    assert sink.events(EventType.POLL) == [
        {"temp_c": 80, "fan_pct": None, "mode": "auto", "target_pct": None}
    ]


def test_drifted_stored_pct_hysteresis_applies_pending_value_at_expiry():
    clock = FakeClock()
    controller, ipmi, sink = make_controller(
        [TempC(68), TempC(60), TempC(52), TempC(52)], clock=clock
    )
    controller.tick()  # 68 -> manual 12
    controller.tick()  # 60 -> target 8 < 12: pending (8, t0)
    clock.advance(15)
    controller.tick()  # mid-window: 52 -> target 7, pending keeps 8
    assert ipmi.commands == ["manual", "set:12"]
    clock.advance(20)
    controller.tick()  # expiry applies the stored 8, not the drifted 7
    assert ipmi.commands == ["manual", "set:12", "set:8"]
    assert sink.events(EventType.SPEED_CHANGE)[-1] == {
        "fan_pct": 8,
        "target_pct": 7,
        "reason": "hysteresis_elapsed",
    }


def test_replace_settings_clears_pending_down_step():
    clock = FakeClock()
    controller, ipmi, sink = make_controller([TempC(68), TempC(60), TempC(60)], clock=clock)
    controller.tick()
    controller.tick()
    assert len(sink.events(EventType.HYSTERESIS_WAIT)) == 1

    assert controller.replace_settings(make_settings()) is True
    assert controller._pending_down is None

    clock.advance(100)
    controller.tick()
    assert "set:8" not in ipmi.commands
    assert len(sink.events(EventType.HYSTERESIS_WAIT)) == 2


def test_out_of_range_curve_target_emits_config_error_and_skips_command():
    # Flat curve pins the target at 101, outside the FanPercent 1..100 range.
    settings = make_settings(
        curve=(
            CurvePoint(TempC(0), FanPercent(101)),
            CurvePoint(TempC(100), FanPercent(101)),
        )
    )
    controller, ipmi, sink = make_controller([TempC(40)], settings=settings)
    controller.tick()
    assert ipmi.commands == []
    assert sink.events(EventType.CONFIG_ERROR) == [{"error": "fan_pct must be in 1..100, got 101"}]
    assert len(sink.events(EventType.POLL)) == 1
    assert sink.events(EventType.MODE_CHANGE) == []
