"""Structured JSON logging, the EventSink adapter, and logging setup.

Logs are the machine output (SPEC locked decision): one JSON object per line on
stdout by default, plain human format under --verbose. The event vocabulary is
T2's EventType enum; ty rejects unknown names at every emit site.
"""

import json
import logging
import sys
from datetime import UTC, datetime

from breezed.types import EventType

SPEC_EVENT_NAMES: frozenset[str] = frozenset(e.value for e in EventType)

_STANDARD_RECORD_KEYS = frozenset(
    logging.LogRecord("x", 0, "p", 1, "m", None, None).__dict__.keys()
)
_EXCLUDED_KEYS = _STANDARD_RECORD_KEYS | {"message", "asctime"}


class JsonLogFormatter(logging.Formatter):
    """One JSON object per line: ts, level, logger, event + extra fields.

    Extra fields come straight out of record.__dict__ minus the standard
    LogRecord attribute set; json.dumps(default=str) lets StrEnum members
    serialize as their lowercase string values.
    """

    def format(self, record: logging.LogRecord) -> str:
        payload = {
            key: value for key, value in record.__dict__.items() if key not in _EXCLUDED_KEYS
        }
        if "event" not in payload:
            payload["event"] = record.getMessage()
        ts = datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")
        payload["ts"] = ts
        payload["level"] = record.levelname
        payload["logger"] = record.name
        return json.dumps(payload, default=str) + "\n"


class LoggingEventSink:
    """Structurally satisfies T5's EventSink Protocol; no runtime name guard."""

    def __init__(self, logger: logging.Logger | None = None) -> None:
        self._log = logger if logger is not None else logging.getLogger("breezed")

    def emit(self, event: EventType, /, **fields: object) -> None:
        self._log.info(str(event), extra={"event": str(event), **fields})


def setup_logging(verbose: bool) -> None:
    """Idempotent; root 'breezed' logger -> one StreamHandler(sys.stdout).

    verbose=False -> INFO + JsonLogFormatter; True -> DEBUG +
    Formatter("%(asctime)s %(levelname)s %(message)s"). Never basicConfig().
    """
    logger = logging.getLogger("breezed")
    for handler in list(logger.handlers):
        logger.removeHandler(handler)
    handler = logging.StreamHandler(sys.stdout)
    if verbose:
        logger.setLevel(logging.DEBUG)
        handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    else:
        logger.setLevel(logging.INFO)
        handler.setFormatter(JsonLogFormatter())
        handler.terminator = ""
    logger.addHandler(handler)


__all__ = ["SPEC_EVENT_NAMES", "JsonLogFormatter", "LoggingEventSink", "setup_logging"]
