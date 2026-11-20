//! Structured JSON logging and the [`EventSink`] adapter.
//!
//! Logs are the machine output: one JSON object per line on stdout by default,
//! human-readable lines under `--verbose`. Field order matches the original
//! Python formatter (`event`, payload fields, then `ts`, `level`, `logger`).

use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{Local, Utc};

use crate::domain::events::Event;
use crate::domain::ports::EventSink;
use crate::domain::types::FanPercent;
use crate::json::Json;

/// Take a lock even if a previous holder panicked; logging must keep working.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The closed event vocabulary as wire strings, useful for log consumers.
pub fn spec_event_names() -> Vec<&'static str> {
    crate::domain::types::EventType::ALL
        .iter()
        .map(|event| event.as_str())
        .collect()
}

/// Current UTC timestamp in the original `2026-01-01T00:00:00Z` shape.
pub fn utc_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn local_timestamp() -> String {
    Local::now().format("%m/%d/%y %H:%M:%S").to_string()
}

/// Serialize one event as a JSON line (without the trailing newline).
pub fn render_json(event: &Event, ts: &str) -> String {
    render_record(
        event.kind().as_str(),
        event.level(),
        event_fields(event),
        ts,
    )
}

/// Serialize an arbitrary record with the same envelope as event lines.
pub fn render_record(
    event_name: &str,
    level: &str,
    fields: Vec<(String, Json)>,
    ts: &str,
) -> String {
    let mut object = Vec::with_capacity(fields.len() + 4);
    object.push(("event".to_owned(), Json::from(event_name)));
    object.extend(fields);
    object.push(("ts".to_owned(), Json::from(ts)));
    object.push(("level".to_owned(), Json::from(level)));
    object.push(("logger".to_owned(), Json::from("breezed")));
    Json::Object(object).to_python_string()
}

/// Render one event in the human-readable `--verbose` shape.
pub fn render_human(event: &Event, ts: &str) -> String {
    format!("[{ts}] {:<8} {}", event.level(), event.kind())
}

fn event_fields(event: &Event) -> Vec<(String, Json)> {
    match event {
        Event::Startup | Event::ConfigReload | Event::Shutdown => Vec::new(),
        Event::Poll {
            temp_c,
            fan_pct,
            mode,
            target_pct,
        } => vec![
            ("temp_c".to_owned(), Json::from(temp_c.0)),
            (
                "fan_pct".to_owned(),
                Json::from(fan_pct.map(FanPercent::get)),
            ),
            ("mode".to_owned(), Json::from(mode.as_str())),
            ("target_pct".to_owned(), Json::from(*target_pct)),
        ],
        Event::ModeChange {
            from,
            to,
            reason,
            temp_c,
            failures,
        } => {
            let mut fields = vec![
                ("from".to_owned(), Json::from(from.as_str())),
                ("to".to_owned(), Json::from(to.as_str())),
                ("reason".to_owned(), Json::from(reason.as_str())),
            ];
            if let Some(temp_c) = temp_c {
                fields.push(("temp_c".to_owned(), Json::from(temp_c.0)));
            }
            if let Some(failures) = failures {
                fields.push(("failures".to_owned(), Json::from(i64::from(*failures))));
            }
            fields
        }
        Event::SpeedChange {
            fan_pct,
            target_pct,
            reason,
        } => vec![
            ("fan_pct".to_owned(), Json::from(fan_pct.get())),
            ("target_pct".to_owned(), Json::from(*target_pct)),
            ("reason".to_owned(), Json::from(reason.as_str())),
        ],
        Event::HysteresisWait {
            target_pct,
            hysteresis_s,
        } => vec![
            ("target_pct".to_owned(), Json::from(*target_pct)),
            ("hysteresis_s".to_owned(), Json::from(*hysteresis_s)),
        ],
        Event::ConfigError { error } => {
            vec![("error".to_owned(), Json::from(error.as_str()))]
        }
        Event::IpmiError {
            operation,
            reason,
            failures,
            error,
        } => {
            let mut fields = Vec::with_capacity(4);
            if let Some(operation) = operation {
                fields.push(("operation".to_owned(), Json::from(operation.as_str())));
            }
            if let Some(reason) = reason {
                fields.push(("reason".to_owned(), Json::from(reason.as_str())));
            }
            fields.push(("failures".to_owned(), Json::from(i64::from(*failures))));
            fields.push(("error".to_owned(), Json::from(error.as_str())));
            fields
        }
    }
}

/// Event sink that writes one line per event to its writer.
pub struct LoggingSink {
    verbose: bool,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl LoggingSink {
    /// Write JSON (or human text under `verbose`) to stdout.
    pub fn stdout(verbose: bool) -> Self {
        Self::with_writer(verbose, Box::new(io::stdout()))
    }

    /// Write to an arbitrary sink; used by tests.
    pub fn with_writer(verbose: bool, writer: Box<dyn Write + Send>) -> Self {
        Self {
            verbose,
            writer: Mutex::new(writer),
        }
    }

    /// Emit a warning as `event: <message>`.
    pub fn warn(&self, message: &str) {
        self.record(message, "WARNING", Vec::new());
    }

    /// Emit a non-vocabulary diagnostic record (fatal errors, warnings).
    pub fn record(&self, event_name: &str, level: &str, fields: Vec<(String, Json)>) {
        let line = if self.verbose {
            format!("[{ts}] {level:<8} {event_name}", ts = local_timestamp())
        } else {
            render_record(event_name, level, fields, &utc_timestamp())
        };
        self.write_line(&line);
    }

    fn write_line(&self, line: &str) {
        let mut writer = lock(&self.writer);
        let _ = writeln!(writer, "{line}");
        let _ = writer.flush();
    }
}

impl EventSink for LoggingSink {
    fn emit(&self, event: &Event) {
        let line = if self.verbose {
            render_human(event, &local_timestamp())
        } else {
            render_json(event, &utc_timestamp())
        };
        self.write_line(&line);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::domain::events::Event;
    use crate::domain::ports::EventSink;
    use crate::domain::types::FanPercent;
    use crate::domain::types::{EventType, OperatingMode, Operation, Reason, TempC};

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn lines(verbose: bool) -> (LoggingSink, SharedBuffer) {
        let buffer = SharedBuffer::default();
        let sink = LoggingSink::with_writer(verbose, Box::new(buffer.clone()));
        (sink, buffer)
    }

    fn records(buffer: &SharedBuffer) -> Vec<serde_json::Value> {
        String::from_utf8(buffer.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn json_output_parses_per_line_with_required_keys() {
        let (sink, buffer) = lines(false);
        sink.emit(&Event::Startup);
        sink.emit(&Event::ModeChange {
            from: OperatingMode::Manual,
            to: OperatingMode::Auto,
            reason: Reason::TempAboveCurve,
            temp_c: Some(TempC(80)),
            failures: None,
        });
        sink.emit(&Event::Poll {
            temp_c: TempC(65),
            fan_pct: Some(FanPercent::from_validated(8)),
            mode: OperatingMode::Manual,
            target_pct: Some(8),
        });
        let records = records(&buffer);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["event"], "startup");
        assert_eq!(records[0]["level"], "INFO");
        assert_eq!(records[0]["logger"], "breezed");
        assert!(records[0]["ts"].as_str().unwrap().ends_with('Z'));
        assert_eq!(records[1]["event"], "mode_change");
        assert_eq!(records[1]["from"], "manual");
        assert_eq!(records[1]["to"], "auto");
        assert_eq!(records[1]["reason"], "temp_above_curve");
        assert_eq!(records[1]["temp_c"], 80);
    }

    #[test]
    fn json_uses_python_field_order_and_spacing() {
        let line = render_json(
            &Event::SpeedChange {
                fan_pct: FanPercent::from_validated(40),
                target_pct: 40,
                reason: Reason::TempRise,
            },
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(
            line,
            r#"{"event": "speed_change", "fan_pct": 40, "target_pct": 40, "reason": "temp_rise", "ts": "2026-01-01T00:00:00Z", "level": "INFO", "logger": "breezed"}"#
        );
    }

    #[test]
    fn ipmi_error_field_order_matches_original() {
        let write_error = render_json(
            &Event::IpmiError {
                operation: Some(Operation::EnableAuto),
                reason: Some(Reason::ReadFailures),
                failures: 2,
                error: "boom".to_owned(),
            },
            "ts",
        );
        assert!(write_error.starts_with(
            r#"{"event": "ipmi_error", "operation": "enable_auto", "reason": "read_failures", "failures": 2, "error": "boom""#
        ));
        let read_error = render_json(
            &Event::IpmiError {
                operation: None,
                reason: None,
                failures: 1,
                error: "sdr failed".to_owned(),
            },
            "ts",
        );
        assert!(
            read_error
                .starts_with(r#"{"event": "ipmi_error", "failures": 1, "error": "sdr failed""#)
        );
    }

    #[test]
    fn verbose_mode_is_not_json() {
        let (sink, buffer) = lines(true);
        sink.emit(&Event::Startup);
        sink.emit(&Event::Poll {
            temp_c: TempC(40),
            fan_pct: None,
            mode: OperatingMode::Auto,
            target_pct: None,
        });
        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert_eq!(output.lines().count(), 2);
        assert!(output.lines().next().unwrap().contains("startup"));
        assert!(serde_json::from_str::<serde_json::Value>(output.lines().next().unwrap()).is_err());
    }

    #[test]
    fn sink_emits_only_spec_event_names() {
        let (sink, buffer) = lines(false);
        let events = [
            Event::Startup,
            Event::Poll {
                temp_c: TempC(63),
                fan_pct: None,
                mode: OperatingMode::Unknown,
                target_pct: None,
            },
            Event::ModeChange {
                from: OperatingMode::Unknown,
                to: OperatingMode::Manual,
                reason: Reason::TempUnderCurve,
                temp_c: None,
                failures: None,
            },
            Event::SpeedChange {
                fan_pct: FanPercent::from_validated(6),
                target_pct: 6,
                reason: Reason::ModeEnter,
            },
            Event::HysteresisWait {
                target_pct: 6,
                hysteresis_s: 30,
            },
            Event::ConfigReload,
            Event::ConfigError {
                error: "bad".to_owned(),
            },
            Event::IpmiError {
                operation: None,
                reason: None,
                failures: 1,
                error: "bad".to_owned(),
            },
            Event::Shutdown,
        ];
        for event in &events {
            sink.emit(event);
        }
        let records = records(&buffer);
        assert_eq!(records.len(), EventType::ALL.len());
        for (record, event) in records.iter().zip(events.iter()) {
            assert_eq!(record["event"], event.kind().as_str());
        }
        for name in spec_event_names() {
            assert!(EventType::ALL.iter().any(|event| event.as_str() == name));
        }
    }

    #[test]
    fn warn_uses_the_message_as_the_event_name() {
        let (sink, buffer) = lines(false);
        sink.warn("metrics server could not bind 127.0.0.1:9762");
        let record = &records(&buffer)[0];
        assert_eq!(
            record["event"],
            "metrics server could not bind 127.0.0.1:9762"
        );
        assert_eq!(record["level"], "WARNING");
    }

    #[test]
    fn fatal_record_is_structured() {
        let (sink, buffer) = lines(false);
        sink.record(
            "fatal",
            "ERROR",
            vec![
                ("error".to_owned(), Json::from("RuntimeError")),
                ("detail".to_owned(), Json::from("disk on fire")),
                (
                    "traceback".to_owned(),
                    Json::from("Traceback (most recent call last)"),
                ),
            ],
        );
        let record = &records(&buffer)[0];
        assert_eq!(record["event"], "fatal");
        assert_eq!(record["level"], "ERROR");
        assert_eq!(record["error"], "RuntimeError");
        assert_eq!(record["detail"], "disk on fire");
    }
}
