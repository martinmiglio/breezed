"""Tests for T2 domain types and validated constructors."""

import pytest

from breezed.types import (
    EventType,
    FanPercent,
    OperatingMode,
    make_fan_pct,
    make_positive_int,
)

SPEC_EVENT_NAMES = {
    "startup",
    "poll",
    "mode_change",
    "speed_change",
    "hysteresis_wait",
    "config_reload",
    "config_error",
    "ipmi_error",
    "shutdown",
}


def test_make_fan_pct_boundaries():
    with pytest.raises(ValueError, match="fan_pct must be in 1..100"):
        make_fan_pct(0)
    with pytest.raises(ValueError, match="fan_pct must be in 1..100"):
        make_fan_pct(101)
    assert make_fan_pct(1) == FanPercent(1)
    assert make_fan_pct(100) == FanPercent(100)


def test_make_positive_int_rejects_nonpositive_naming_field():
    with pytest.raises(ValueError, match="poll_interval_s"):
        make_positive_int("poll_interval_s", 0)
    with pytest.raises(ValueError, match="step_down_hysteresis_s must be > 0, got -5"):
        make_positive_int("step_down_hysteresis_s", -5)


def test_make_positive_int_passes_through_positive_values():
    assert make_positive_int("read_failure_limit", 3) == 3


def test_event_type_values_match_spec_vocabulary():
    assert {event.value for event in EventType} == SPEC_EVENT_NAMES


def test_operating_mode_members_serialize_lowercase():
    assert str(OperatingMode.UNKNOWN) == "unknown"
    assert str(OperatingMode.AUTO) == "auto"
    assert str(OperatingMode.MANUAL) == "manual"
