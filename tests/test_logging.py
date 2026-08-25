"""Tests for breezed.logs: JSON formatter, sink adapter, setup_logging.

Harness built in-file (T5 conventions): StringIO handler on a fresh logger,
no caplog, no monkeypatching, no sleeps.
"""

import io
import json
import logging
import re
from collections.abc import Iterator
from contextlib import contextmanager

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


@contextmanager
def restore_logger(name: str) -> Iterator[None]:
    logger = logging.getLogger(name)
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


@pytest.fixture(autouse=True)
def restore_test_logger() -> Iterator[None]:
    with restore_logger("breezed.test"):
        yield


def make_logger(json: bool) -> tuple[logging.Logger, io.StringIO]:
    stream = io.StringIO()
    logger = logging.getLogger("breezed.test")
    for handler in list(logger.handlers):
        logger.removeHandler(handler)
    handler = logging.StreamHandler(stream)
    if json:
        handler.setFormatter(JsonLogFormatter())
        handler.terminator = ""
    else:
        handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    logger.addHandler(handler)
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    return logger, stream


def parsed_lines(stream: io.StringIO) -> list[dict[str, object]]:
    return [json.loads(line) for line in stream.getvalue().splitlines()]


def test_json_output_parses_per_line_with_required_keys() -> None:
    logger, stream = make_logger(True)
    logger.info(str(EventType.STARTUP), extra={"event": str(EventType.STARTUP)})
    logger.info(
        str(EventType.MODE_CHANGE),
        extra={
            "event": str(EventType.MODE_CHANGE),
            **{"from": "manual", "to": "auto", "reason": "temp_above_curve", "temp_c": 80},
        },
    )
    logger.info(
        str(EventType.POLL),
        extra={
            "event": str(EventType.POLL),
            "temp_c": 65,
            "fan_pct": 8,
            "mode": "manual",
            "target_pct": 8,
        },
    )
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


def test_verbose_mode_is_not_json() -> None:
    logger, stream = make_logger(False)
    logger.info(str(EventType.STARTUP))
    logger.info(str(EventType.POLL))
    lines = stream.getvalue().splitlines()
    assert len(lines) == 2
    for line in lines:
        assert HUMAN_SHAPE.match(line)
    with pytest.raises(json.JSONDecodeError):
        json.loads(lines[0])


def test_formatter_event_fallback_uses_message() -> None:
    logger, stream = make_logger(True)
    logger.info("bare message without event")
    (record,) = parsed_lines(stream)
    assert record["event"] == "bare message without event"


def test_sink_emits_only_spec_event_names() -> None:
    logger, stream = make_logger(True)
    sink: EventSink = LoggingEventSink(logger)
    for event in EventType:
        sink.emit(event, temp_c=63)
    records = parsed_lines(stream)
    assert len(records) == len(EventType)
    for record, event in zip(records, EventType, strict=True):
        assert record["event"] in SPEC_EVENT_NAMES
        assert record["event"] == str(event)


def test_setup_logging_idempotent() -> None:
    with restore_logger("breezed"):
        setup_logging(False)
        setup_logging(False)
        assert len(logging.getLogger("breezed").handlers) == 1
        assert logging.getLogger("breezed").level == logging.INFO
        setup_logging(True)
        assert len(logging.getLogger("breezed").handlers) == 1
        assert logging.getLogger("breezed").level == logging.DEBUG


def test_formatter_includes_exception_payload() -> None:
    logger, stream = make_logger(True)
    try:
        raise RuntimeError("boom")
    except RuntimeError:
        logger.exception(EventType.IPMI_ERROR)
    (record,) = parsed_lines(stream)
    assert "boom" in str(record["exc"])
    assert record["event"] == "ipmi_error"
