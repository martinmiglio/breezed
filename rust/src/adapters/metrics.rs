//! Opt-in Prometheus metrics: shared state, text renderer, loopback HTTP server.
//!
//! Metrics are diagnostic sugar: the server binds `127.0.0.1` only and a bind
//! failure degrades to metrics-less operation instead of crashing fan control.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::domain::events::Event;
use crate::domain::ports::EventSink;
use crate::domain::types::{FanPercent, OperatingMode, TempC};

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
const METADATA: &str = "\
# HELP breezed_temp_c Current maximum CPU temperature in degrees Celsius.
# TYPE breezed_temp_c gauge
# HELP breezed_fan_percent Last commanded manual fan speed percentage.
# TYPE breezed_fan_percent gauge
# HELP breezed_mode Current breezed operating mode.
# TYPE breezed_mode gauge
# HELP breezed_ipmi_errors_total Total IPMI errors observed.
# TYPE breezed_ipmi_errors_total counter
# HELP breezed_polls_total Total successful temperature polls.
# TYPE breezed_polls_total counter
";

/// Take a lock even if a previous holder panicked; metrics must keep serving.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
struct MetricsInner {
    temp_c: Option<TempC>,
    fan_percent: Option<FanPercent>,
    mode: OperatingMode,
    ipmi_errors_total: u64,
    polls_total: u64,
}

/// Shared, thread-safe metrics state; update-all and snapshot-all are atomic.
#[derive(Debug)]
pub struct MetricsState {
    inner: Mutex<MetricsInner>,
}

impl Default for MetricsState {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsState {
    /// Create an empty state (mode `unknown`, counters zero).
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MetricsInner {
                temp_c: None,
                fan_percent: None,
                mode: OperatingMode::Unknown,
                ipmi_errors_total: 0,
                polls_total: 0,
            }),
        }
    }

    /// Record one successful poll atomically.
    pub fn record_poll(&self, temp_c: TempC, fan_percent: Option<FanPercent>, mode: OperatingMode) {
        let mut inner = lock(&self.inner);
        inner.temp_c = Some(temp_c);
        inner.fan_percent = fan_percent;
        inner.mode = mode;
        inner.polls_total += 1;
    }

    /// Increment the IPMI error counter.
    pub fn record_ipmi_error(&self) {
        lock(&self.inner).ipmi_errors_total += 1;
    }

    /// Last observed temperature and mode, for diagnostics.
    pub fn last_poll(&self) -> Option<(TempC, Option<FanPercent>, OperatingMode)> {
        let inner = lock(&self.inner);
        inner
            .temp_c
            .map(|temp| (temp, inner.fan_percent, inner.mode))
    }

    /// Total counters as `(polls, ipmi_errors)`.
    pub fn counters(&self) -> (u64, u64) {
        let inner = lock(&self.inner);
        (inner.polls_total, inner.ipmi_errors_total)
    }

    /// Render the Prometheus text exposition.
    pub fn render(&self) -> String {
        let (temp_c, fan_percent, mode, ipmi_errors_total, polls_total) = {
            let inner = lock(&self.inner);
            (
                inner.temp_c,
                inner.fan_percent,
                inner.mode,
                inner.ipmi_errors_total,
                inner.polls_total,
            )
        };
        let mut out = String::from(METADATA);
        if let Some(temp_c) = temp_c {
            let _ = writeln!(out, "breezed_temp_c{{sensor=\"cpu_max\"}} {}", temp_c.0);
        }
        if let Some(fan_percent) = fan_percent {
            let _ = writeln!(out, "breezed_fan_percent {}", fan_percent.get());
        }
        let _ = writeln!(out, "breezed_mode{{mode=\"{mode}\"}} 1");
        let _ = writeln!(out, "breezed_ipmi_errors_total {ipmi_errors_total}");
        let _ = writeln!(out, "breezed_polls_total {polls_total}");
        out
    }
}

impl EventSink for MetricsState {
    fn emit(&self, event: &Event) {
        match event {
            Event::Poll {
                temp_c,
                fan_pct,
                mode,
                ..
            } => self.record_poll(*temp_c, *fan_pct, *mode),
            Event::IpmiError { .. } => self.record_ipmi_error(),
            _ => {}
        }
    }
}

/// Handle for a running metrics server.
pub struct MetricsServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MetricsServer {
    /// The bound loopback port (useful when binding port 0).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting connections and join the server thread.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the blocking accept call with a throwaway connection.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind `127.0.0.1:port` and serve `/metrics` and `/` on a background thread.
///
/// # Errors
///
/// Returns the bind or address lookup error; the caller decides whether to
/// degrade to metrics-less operation.
pub fn start_metrics_server(port: u16, state: Arc<MetricsState>) -> std::io::Result<MetricsServer> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let bound_port = listener.local_addr()?.port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            if thread_shutdown.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(stream) = stream {
                let state = state.clone();
                thread::spawn(move || {
                    let _ = serve_one(stream, &state);
                });
            }
        }
    });
    Ok(MetricsServer {
        port: bound_port,
        shutdown,
        handle: Some(handle),
    })
}

fn serve_one(mut stream: TcpStream, state: &MetricsState) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buffer = [0_u8; 8192];
    let read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    if path == "/metrics" || path == "/" {
        let body = state.render();
        let header = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: {CONTENT_TYPE}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes())?;
        stream.write_all(body.as_bytes())?;
    } else {
        stream.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;
    use crate::domain::events::Event;
    use crate::domain::ports::EventSink;

    const EXPECTED_METADATA: &str = "\
# HELP breezed_temp_c Current maximum CPU temperature in degrees Celsius.
# TYPE breezed_temp_c gauge
# HELP breezed_fan_percent Last commanded manual fan speed percentage.
# TYPE breezed_fan_percent gauge
# HELP breezed_mode Current breezed operating mode.
# TYPE breezed_mode gauge
# HELP breezed_ipmi_errors_total Total IPMI errors observed.
# TYPE breezed_ipmi_errors_total counter
# HELP breezed_polls_total Total successful temperature polls.
# TYPE breezed_polls_total counter
";

    fn populated_state() -> MetricsState {
        let state = MetricsState::new();
        state.record_poll(
            TempC(63),
            Some(FanPercent::from_validated(12)),
            OperatingMode::Manual,
        );
        // Match the original fixture's 42 polls.
        for _ in 0..41 {
            state.record_poll(
                TempC(63),
                Some(FanPercent::from_validated(12)),
                OperatingMode::Manual,
            );
        }
        state
    }

    #[test]
    fn render_exact_documented_lines_from_populated_state() {
        let state = MetricsState::new();
        state.record_poll(
            TempC(63),
            Some(FanPercent::from_validated(12)),
            OperatingMode::Manual,
        );
        let mut expected = String::from(EXPECTED_METADATA);
        expected.push_str(
            "breezed_temp_c{sensor=\"cpu_max\"} 63\n\
             breezed_fan_percent 12\n\
             breezed_mode{mode=\"manual\"} 1\n\
             breezed_ipmi_errors_total 0\n\
             breezed_polls_total 1\n",
        );
        assert_eq!(state.render(), expected);
    }

    #[test]
    fn mode_label_reflects_state() {
        let state = MetricsState::new();
        assert!(state.render().contains("breezed_mode{mode=\"unknown\"} 1"));
        state.record_poll(
            TempC(60),
            Some(FanPercent::from_validated(8)),
            OperatingMode::Auto,
        );
        assert!(state.render().contains("breezed_mode{mode=\"auto\"} 1"));
        state.record_poll(
            TempC(61),
            Some(FanPercent::from_validated(9)),
            OperatingMode::Manual,
        );
        assert!(state.render().contains("breezed_mode{mode=\"manual\"} 1"));
    }

    #[test]
    fn render_omits_gauge_lines_before_first_poll() {
        let state = MetricsState::new();
        let rendered = state.render();
        assert_eq!(
            rendered,
            format!(
                "{EXPECTED_METADATA}breezed_mode{{mode=\"unknown\"}} 1\n\
                 breezed_ipmi_errors_total 0\nbreezed_polls_total 0\n"
            )
        );
        assert!(!rendered.contains("breezed_temp_c{sensor=\"cpu_max\"}"));
        assert!(!rendered.contains("\nbreezed_fan_percent "));
    }

    #[test]
    fn counters_are_monotonic() {
        let state = MetricsState::new();
        let mut history = Vec::new();
        for step in 0..10 {
            if step % 3 == 2 {
                state.record_ipmi_error();
            } else {
                state.record_poll(
                    TempC(50 + step),
                    Some(FanPercent::from_validated(6)),
                    OperatingMode::Manual,
                );
            }
            history.push(state.counters());
        }
        let polls: Vec<u64> = history.iter().map(|&(polls, _)| polls).collect();
        let errors: Vec<u64> = history.iter().map(|&(_, errors)| errors).collect();
        assert_eq!(polls, [1, 2, 2, 3, 4, 4, 5, 6, 6, 7]);
        assert_eq!(errors, [0, 0, 1, 1, 1, 2, 2, 2, 3, 3]);
    }

    fn http_get(port: u16, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn metrics_server_serves_documented_endpoints_on_loopback() {
        let state = Arc::new(populated_state());
        let mut expected = String::from(EXPECTED_METADATA);
        expected.push_str(
            "breezed_temp_c{sensor=\"cpu_max\"} 63\n\
             breezed_fan_percent 12\n\
             breezed_mode{mode=\"manual\"} 1\n\
             breezed_ipmi_errors_total 0\n\
             breezed_polls_total 42\n",
        );
        let mut server = start_metrics_server(0, state).expect("bind");
        assert!(server.port() > 0);
        let response = http_get(server.port(), "/metrics");
        assert!(response.starts_with("HTTP/1.0 200 OK"));
        assert!(response.contains(CONTENT_TYPE));
        assert_eq!(
            response.split("\r\n\r\n").nth(1).unwrap(),
            expected.as_str()
        );
        let root = http_get(server.port(), "/");
        assert_eq!(root.split("\r\n\r\n").nth(1).unwrap(), expected.as_str());
        let missing = http_get(server.port(), "/nope");
        assert!(missing.starts_with("HTTP/1.0 404 Not Found"));
        server.shutdown();
    }

    #[test]
    fn start_metrics_server_returns_none_on_bind_failure() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();
        assert!(start_metrics_server(port, Arc::new(MetricsState::new())).is_err());
    }

    #[test]
    fn event_sink_records_polls_and_errors() {
        let state = MetricsState::new();
        state.emit(&Event::Poll {
            temp_c: TempC(70),
            fan_pct: Some(FanPercent::from_validated(18)),
            mode: OperatingMode::Manual,
            target_pct: Some(18),
        });
        state.emit(&Event::IpmiError {
            operation: None,
            reason: None,
            failures: 1,
            error: "boom".to_owned(),
        });
        assert_eq!(state.counters(), (1, 1));
        assert_eq!(
            state.last_poll(),
            Some((
                TempC(70),
                Some(FanPercent::from_validated(18)),
                OperatingMode::Manual
            ))
        );
    }
}
