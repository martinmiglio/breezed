"""Structured JSON logging, the EventSink adapter, and logging setup.

Logs are the machine output: one JSON object per line on stdout by default,
rich-formatted human output under --verbose. The event vocabulary is the
EventType enum; ty rejects unknown names at every emit site.
"""

import json
import logging
import sys
from datetime import UTC, datetime

from rich.logging import RichHandler

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
    serialize as their lowercase string values. The trailing newline this
    formatter appends pairs with setup_logging's handler terminator="", so
    exactly one newline separates JSON objects on stdout.
    """

    def format(self, record: logging.LogRecord) -> str:
        payload = {
            key: value for key, value in record.__dict__.items() if key not in _EXCLUDED_KEYS
        }
        if "event" not in payload:
            payload["event"] = record.getMessage()
        if record.exc_info:
            payload["exc"] = self.formatException(record.exc_info)
        ts = datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")
        payload["ts"] = ts
        payload["level"] = record.levelname
        payload["logger"] = record.name
        return json.dumps(payload, default=str) + "\n"


class LoggingEventSink:
    """Structurally satisfies the EventSink Protocol; no runtime name guard."""

    def __init__(self, logger: logging.Logger | None = None) -> None:
        self._log = logger if logger is not None else logging.getLogger("breezed")

    def emit(self, event: EventType, /, **fields: object) -> None:
        self._log.info(str(event), extra={"event": str(event), **fields})


def setup_logging(verbose: bool) -> None:
    """Idempotent; root 'breezed' logger -> one StreamHandler(sys.stdout).

    verbose=False -> INFO + JsonLogFormatter (byte-stable machine output, rich
    must never touch this path); True -> DEBUG + RichHandler for colors and
    readable formatting. Never basicConfig().
    """
    logger = logging.getLogger("breezed")
    for handler in list(logger.handlers):
        logger.removeHandler(handler)
    if verbose:
        logger.setLevel(logging.DEBUG)
        logger.addHandler(RichHandler(rich_tracebacks=True, show_path=False))
        return
    handler = logging.StreamHandler(sys.stdout)
    logger.setLevel(logging.INFO)
    handler.setFormatter(JsonLogFormatter())
    handler.terminator = ""
    logger.addHandler(handler)


__all__ = ["SPEC_EVENT_NAMES", "JsonLogFormatter", "LoggingEventSink", "setup_logging"]
