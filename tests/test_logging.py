"""Tests for breezed.logs: JSON formatter, sink adapter, setup_logging.

Harness built in-file (T5 conventions): StringIO handler on a fresh logger,
no caplog, no monkeypatching, no sleeps.
"""

import io
import json
import logging
import re
from collections.abc import Iterator

import pytest

from breezed.controller import EventSink
from breezed.logs import (
    SPEC_EVENT_NAMES,
    JsonLogFormatter,
    LoggingEventSink,
    setup_logging,
)
from breezed.types import EventType

TS_SHAPE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")
HUMAN_SHAPE = re.compile(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2},\d+ \w+ .+$")


@pytest.fixture(autouse=True)
def restore_test_logger() -> Iterator[None]:
    logger = logging.getLogger("breezed.test")
    prior_handlers = list(logger.handlers)
    prior_level = logger.level
    prior_propagate = logger.propagate
    yield
    for handler in list(logger.handlers):
        if handler not in prior_handlers:
            logger.removeHandler(handler)
            handler.close()
    for handler in prior_handlers:
        logger.addHandler(handler)
    logger.setLevel(prior_level)
    logger.propagate = prior_propagate


def make_json_logger() -> tuple[logging.Logger, io.StringIO]:
    stream = io.StringIO()
    logger = logging.getLogger("breezed.test")
    for handler in list(logger.handlers):
        logger.removeHandler(handler)
    handler = logging.StreamHandler(stream)
    handler.setFormatter(JsonLogFormatter())
    handler.terminator = ""
    logger.addHandler(handler)
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    return logger, stream


def make_human_logger() -> tuple[logging.Logger, io.StringIO]:
    stream = io.StringIO()
    logger = logging.getLogger("breezed.test")
    for handler in list(logger.handlers):
        logger.removeHandler(handler)
    handler = logging.StreamHandler(stream)
    handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    logger.addHandler(handler)
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    return logger, stream


def emit(
    target: logging.Logger | LoggingEventSink,
    event: EventType,
    **fields: object,
) -> None:
    if isinstance(target, LoggingEventSink):
        target.emit(event, **fields)
    else:
        target.info(str(event), extra={"event": str(event), **fields})


def parsed_lines(stream: io.StringIO) -> list[dict[str, object]]:
    return [json.loads(line) for line in stream.getvalue().splitlines()]


def test_json_output_parses_per_line_with_required_keys() -> None:
    logger, stream = make_json_logger()
    emit(logger, EventType.STARTUP)
    emit(
        logger,
        EventType.MODE_CHANGE,
        **{"from": "manual", "to": "auto", "reason": "temp_above_curve", "temp_c": 80},
    )
    emit(logger, EventType.POLL, temp_c=65, fan_pct=8, mode="manual", target_pct=8)
    records = parsed_lines(stream)
    assert len(records) == 3
    for record in records:
        assert "ts" in record
        assert TS_SHAPE.match(str(record["ts"]))
        assert record["level"] == "INFO"
        assert record["logger"] == "breezed.test"
        assert "event" in record
    assert records[1]["event"] == "mode_change"
    assert records[1]["from"] == "manual"
    assert records[1]["to"] == "auto"
    assert records[1]["reason"] == "temp_above_curve"
    assert records[1]["temp_c"] == 80


def test_extra_fields_surface_as_top_level_keys() -> None:
    logger, stream = make_json_logger()
    emit(logger, EventType.POLL, temp_c=65, fan_pct=8, target_pct=8)
    (record,) = parsed_lines(stream)
    assert record["temp_c"] == 65
    assert record["fan_pct"] == 8
    assert record["target_pct"] == 8
    assert record["event"] == "poll"


def test_verbose_mode_is_not_json() -> None:
    logger, stream = make_human_logger()
    emit(logger, EventType.STARTUP)
    emit(logger, EventType.POLL, temp_c=65)
    lines = stream.getvalue().splitlines()
    assert len(lines) == 2
    for line in lines:
        assert HUMAN_SHAPE.match(line)
    with pytest.raises(json.JSONDecodeError):
        json.loads(lines[0])


def test_formatter_event_fallback_uses_message() -> None:
    logger, stream = make_json_logger()
    logger.info("bare message without event")
    (record,) = parsed_lines(stream)
    assert record["event"] == "bare message without event"


def test_event_vocabulary_matches_spec() -> None:
    spec_names = {
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
    assert {e.value for e in EventType} == spec_names
    assert frozenset(spec_names) == SPEC_EVENT_NAMES


def test_sink_emits_only_spec_event_names() -> None:
    logger, stream = make_json_logger()
    sink: EventSink = LoggingEventSink(logger)
    for event in EventType:
        sink.emit(event, temp_c=63)
    records = parsed_lines(stream)
    assert len(records) == len(EventType)
    for record, event in zip(records, EventType, strict=True):
        assert record["event"] in SPEC_EVENT_NAMES
        assert record["event"] == str(event)


def test_setup_logging_idempotent() -> None:
    logger = logging.getLogger("breezed")
    prior_handlers = list(logger.handlers)
    prior_level = logger.level
    try:
        setup_logging(False)
        setup_logging(False)
        assert len(logger.handlers) == 1
        assert logger.level == logging.INFO
        setup_logging(True)
        assert len(logging.getLogger("breezed").handlers) == 1
        assert logging.getLogger("breezed").level == logging.DEBUG
    finally:
        for handler in list(logger.handlers):
            if handler not in prior_handlers:
                logger.removeHandler(handler)
                handler.close()
        for handler in prior_handlers:
            logger.addHandler(handler)
        logger.setLevel(prior_level)


def test_formatter_includes_exception_payload() -> None:
    logger, stream = make_json_logger()
    try:
        raise RuntimeError("boom")
    except RuntimeError:
        logger.exception(EventType.IPMI_ERROR)
    (record,) = parsed_lines(stream)
    assert "boom" in str(record["exc"])
    assert record["event"] == "ipmi_error"
