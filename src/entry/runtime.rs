//! Runtime commands: `run`, `set`, `auto`, `status`, `validate`.
//!
//! `run` owns the composition root for the control loop. Errors map onto the
//! exit-code contract: `1` runtime/IPMI, `2` usage/config.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::adapters::config::load_settings;
use crate::adapters::ipmi::IpmiClient;
use crate::adapters::logs::LoggingSink;
use crate::adapters::metrics::{MetricsServer, MetricsState, start_metrics_server};
use crate::adapters::watcher::ConfigWatcher;
use crate::application::controller::Controller;
use crate::domain::curve::{CurvePolicy, interpolate};
use crate::domain::events::Event;
use crate::domain::ports::{
    Clock, EventSink, FanCommander, FanDevice, SpeedPolicy, SystemClock, TempReader,
};
use crate::domain::settings::Settings;
use crate::domain::types::make_fan_pct;
use crate::entry::common::{CliError, write_stdout_line};
use serde_json::{Value, json};

/// Interruptible sleep seam; returns `true` when the stop flag was set.
pub trait Sleeper: Send + Sync {
    /// Wait up to `seconds`, returning early with `true` if `stop` is set.
    fn sleep_interruptible(&self, stop: &AtomicBool, seconds: u64) -> bool;
}

/// Production sleeper: 20 ms slices so SIGINT/SIGTERM stop promptly.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemSleeper;

impl Sleeper for SystemSleeper {
    fn sleep_interruptible(&self, stop: &AtomicBool, seconds: u64) -> bool {
        let total = Duration::from_secs(seconds);
        let started = Instant::now();
        loop {
            if stop.load(Ordering::SeqCst) {
                return true;
            }
            let elapsed = started.elapsed();
            if elapsed >= total {
                return false;
            }
            sleep(total.saturating_sub(elapsed).min(Duration::from_millis(20)));
        }
    }
}

fn emit(sinks: &[Arc<dyn EventSink>], event: &Event) {
    for sink in sinks {
        sink.emit(event);
    }
}

fn log_fatal(log: &LoggingSink, error_name: &str, detail: &str) {
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    log.record(
        "fatal",
        "ERROR",
        &[
            ("error", json!(error_name)),
            ("detail", json!(detail)),
            ("traceback", json!(backtrace)),
        ],
    );
    // Emit the backtrace to stderr as well as the structured record.
    let _ = writeln!(std::io::stderr(), "{backtrace}");
}

fn print_json(value: &Value) -> Result<(), CliError> {
    write_stdout_line(&value.to_string())
}

/// Sinks constructed for one runtime session.
pub struct RuntimeSinks {
    /// Destination for structured logs and fatal diagnostics.
    pub log: Arc<LoggingSink>,
    /// Extra sinks such as metrics or test recorders.
    pub additional: Vec<Arc<dyn EventSink>>,
}

/// Factory for the IPMI device.
pub type DeviceFactory = dyn Fn(&Settings) -> Arc<dyn FanDevice> + Send + Sync;

/// Factory for the runtime sinks.
pub type SinkFactory = dyn Fn(bool) -> RuntimeSinks + Send + Sync;

/// Injected collaborators for the control loop.
///
/// Production wires [`RuntimeDeps::production`]; tests substitute a fake device,
/// sleeper, clock, and sinks to exercise orchestration without real time.
pub struct RuntimeDeps {
    /// Build the IPMI device from validated settings.
    pub build_device: Arc<DeviceFactory>,
    /// Build the log sink and any extra event sinks.
    pub make_sinks: Arc<SinkFactory>,
    /// Interruptible sleep used between ticks.
    pub sleeper: Arc<dyn Sleeper>,
    /// Speed policy injected into the controller.
    pub policy: Arc<dyn SpeedPolicy>,
    /// Clock injected into the controller.
    pub clock: Arc<dyn Clock>,
    /// Whether to install SIGINT/SIGTERM handlers (tests pass `false`).
    pub install_signals: bool,
}

impl RuntimeDeps {
    /// Wire the production adapters.
    pub fn production() -> Self {
        Self {
            build_device: Arc::new(|settings| {
                Arc::new(IpmiClient::new(settings.clone())) as Arc<dyn FanDevice>
            }),
            make_sinks: Arc::new(|verbose| RuntimeSinks {
                log: Arc::new(LoggingSink::stdout(verbose)),
                additional: Vec::new(),
            }),
            policy: Arc::new(CurvePolicy),
            sleeper: Arc::new(SystemSleeper),
            clock: Arc::new(SystemClock),
            install_signals: true,
        }
    }
}

fn load_or_usage(config: &Path) -> Result<Settings, CliError> {
    load_settings(config).map_err(|error| CliError::usage(error.to_string()))
}

fn connect(config: &Path) -> Result<(Settings, IpmiClient), CliError> {
    let settings = load_or_usage(config)?;
    let client = IpmiClient::new(settings.clone());
    Ok((settings, client))
}

/// Run the control loop until SIGINT/SIGTERM.
///
/// # Errors
///
/// Returns a usage error (`2`) for invalid config, or a silent runtime error
/// (`1`) after logging a structured `fatal` event for an unexpected failure.
pub fn run_command(
    config: &Path,
    metrics_port: Option<u16>,
    verbose: bool,
) -> Result<(), CliError> {
    run_with(config, metrics_port, verbose, &RuntimeDeps::production())
}

fn run_with(
    config: &Path,
    metrics_port: Option<u16>,
    verbose: bool,
    deps: &RuntimeDeps,
) -> Result<(), CliError> {
    let settings = load_or_usage(config)?;

    let RuntimeSinks { log, additional } = (deps.make_sinks)(verbose);
    let mut sinks: Vec<Arc<dyn EventSink>> = vec![log.clone()];
    sinks.extend(additional);

    let selected_port = metrics_port.or(settings.metrics_port());
    let mut server: Option<MetricsServer> = None;
    if let Some(port) = selected_port {
        let state = Arc::new(MetricsState::new());
        sinks.push(state.clone());
        match start_metrics_server(port, state) {
            Ok(started) => server = Some(started),
            Err(_) => log.warn(&format!(
                "metrics server could not bind 127.0.0.1:{port}; continuing without metrics"
            )),
        }
    }

    let mut watcher = match ConfigWatcher::new(config) {
        Ok(watcher) => watcher,
        Err(error) => {
            log_fatal(&log, "ConfigError", &error.to_string());
            return Err(CliError::silent(1));
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    if deps.install_signals {
        register_signals(&stop);
    }

    emit(&sinks, &Event::Startup);

    let device = (deps.build_device)(&settings);
    let mut controller = Controller::new(
        device,
        deps.policy.clone(),
        settings.clone(),
        sinks.clone(),
        deps.clock.clone(),
    );
    let poll_interval = u64::try_from(settings.poll_interval_s()).unwrap_or(10);
    let failure = run_tick_loop(
        &mut controller,
        &mut watcher,
        deps,
        &stop,
        poll_interval,
        &sinks,
    );

    controller.shutdown();
    if let Some(server) = server.as_mut() {
        server.shutdown();
    }
    emit(&sinks, &Event::Shutdown);

    if let Some(error) = failure {
        log_fatal(&log, "DomainError", &error.to_string());
        return Err(CliError::silent(1));
    }
    Ok(())
}

fn run_tick_loop(
    controller: &mut Controller,
    watcher: &mut ConfigWatcher,
    deps: &RuntimeDeps,
    stop: &AtomicBool,
    poll_interval: u64,
    sinks: &[Arc<dyn EventSink>],
) -> Option<crate::domain::types::DomainError> {
    let mut poll_interval = poll_interval;
    loop {
        if deps.sleeper.sleep_interruptible(stop, poll_interval) {
            return None;
        }
        if let Err(error) = controller.tick() {
            return Some(error);
        }
        if watcher.changed() {
            match watcher.reload() {
                Err(error) => emit(
                    sinks,
                    &Event::ConfigError {
                        error: error.to_string(),
                    },
                ),
                Ok(new_settings) => {
                    poll_interval = u64::try_from(new_settings.poll_interval_s()).unwrap_or(10);
                    controller.replace_settings(new_settings);
                    emit(sinks, &Event::ConfigReload);
                }
            }
        }
    }
}

fn register_signals(stop: &Arc<AtomicBool>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::flag;

    let _ = flag::register(SIGINT, stop.clone());
    let _ = flag::register(SIGTERM, stop.clone());
}

/// One-shot fixed manual fan percentage.
///
/// # Errors
///
/// Exit code `2` for an out-of-range percentage or bad config; `1` on IPMI
/// failure.
pub fn set_command(pct: i64, config: &Path) -> Result<(), CliError> {
    let pct_valid = make_fan_pct(pct).map_err(|error| CliError::usage(error.to_string()))?;
    let (_, client) = connect(config)?;
    client
        .disable_auto()
        .and_then(|()| client.set_manual_pct(pct_valid))
        .map_err(|error| CliError::runtime(error.to_string()))?;
    print_json(&json!({"event": "speed_change", "fan_pct": pct}))
}

/// Hand fan control back to the iDRAC.
///
/// # Errors
///
/// Exit code `2` for bad config; `1` on IPMI failure.
pub fn auto_command(config: &Path) -> Result<(), CliError> {
    let (_, client) = connect(config)?;
    client
        .enable_auto()
        .map_err(|error| CliError::runtime(error.to_string()))?;
    print_json(&json!({"event": "mode_change", "to": "auto"}))
}

/// One-shot sensor status.
///
/// # Errors
///
/// Exit code `2` for bad config; `1` on IPMI failure.
pub fn status_command(config: &Path, verbose: bool) -> Result<(), CliError> {
    let (settings, client) = connect(config)?;
    let temp = client
        .read_max_cpu_temp()
        .map_err(|error| CliError::runtime(error.to_string()))?;
    let rpms = client
        .read_fan_rpms()
        .map_err(|error| CliError::runtime(error.to_string()))?;
    let target = interpolate(settings.curve().points(), temp)
        .map_err(|error| CliError::runtime(error.to_string()))?;
    if verbose {
        let mut text = String::new();
        let _ = writeln!(text, "CPU max: {}C", temp.0);
        for (name, rpm) in &rpms {
            let _ = writeln!(text, "{name}: {rpm} RPM");
        }
        match target {
            None => text.push_str("Curve target: auto (above curve)"),
            Some(target) => {
                let _ = write!(text, "Curve target: {target}% (manual)");
            }
        }
        write_stdout_line(&text)?;
    } else {
        print_json(&json!({
            "temp_c": temp.0,
            "fan_rpms": rpms,
            "target_pct": target,
        }))?;
    }
    Ok(())
}

/// Validate a config file and print a JSON summary; `probe` adds a live read.
///
/// # Errors
///
/// Exit code `2` for an invalid config; `1` when `--probe` cannot read the
/// iDRAC.
pub fn validate_command(path: &Path, probe: bool) -> Result<(), CliError> {
    let settings = match load_settings(path) {
        Ok(settings) => settings,
        Err(error) => {
            let message = error.to_string();
            print_json(&json!({"valid": false, "error": message}))?;
            return Err(CliError::usage(message));
        }
    };
    let mut summary = json!({
        "valid": true,
        "path": path.to_string_lossy(),
        "host": settings.host(),
        "poll_interval_s": settings.poll_interval_s(),
        "curve_points": settings.curve().len(),
        "metrics_port": settings.metrics_port(),
    });
    if probe {
        let client = IpmiClient::new(settings.clone());
        let temp = match client.read_max_cpu_temp() {
            Ok(temp) => temp,
            Err(error) => {
                let message = error.to_string();
                summary["probe"] = json!({"ok": false, "error": message});
                print_json(&summary)?;
                return Err(CliError::runtime(message));
            }
        };
        let target = interpolate(settings.curve().points(), temp)
            .map_err(|error| CliError::runtime(error.to_string()))?;
        summary["probe"] = json!({
            "ok": true,
            "temp_c": temp.0,
            "target_pct": target,
            "would_auto": target.is_none(),
        });
    }
    print_json(&summary)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::assert_is_empty)]

    use std::collections::VecDeque;
    use std::fs;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::domain::curve::CurvePolicy;
    use crate::domain::events::Event;
    use crate::domain::ports::{FanCommander, SpeedPolicy, TempReader};
    use crate::domain::settings::Settings;
    use crate::domain::test_support::curve;
    use crate::domain::types::{DomainError, EventType, FanPercent, TempC};
    use crate::entry::common::CliError;

    const CONFIG: &str = r#"
[settings]
host = "169.254.0.1"
user = "root"
poll_interval_s = 1

[[curve]]
temp_c = 45
fan_pct = 6

[[curve]]
temp_c = 60
fan_pct = 8

[[curve]]
temp_c = 68
fan_pct = 12

[[curve]]
temp_c = 74
fan_pct = 18
"#;

    const FLAT_CONFIG: &str = r#"
[settings]
host = "169.254.0.1"
user = "root"
poll_interval_s = 1

[[curve]]
temp_c = 40
fan_pct = 20

[[curve]]
temp_c = 100
fan_pct = 20
"#;

    struct FakeDevice {
        temps: Mutex<VecDeque<TempC>>,
        commands: Mutex<Vec<String>>,
    }

    impl FakeDevice {
        fn new(temps: &[i32]) -> Arc<Self> {
            Arc::new(Self {
                temps: Mutex::new(temps.iter().map(|&temp| TempC(temp)).collect()),
                commands: Mutex::new(Vec::new()),
            })
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl TempReader for FakeDevice {
        fn read_max_cpu_temp(&self) -> Result<TempC, crate::domain::types::IpmiError> {
            let mut temps = self.temps.lock().unwrap();
            Ok(temps.pop_front().unwrap_or(TempC(46)))
        }
    }

    impl FanCommander for FakeDevice {
        fn enable_auto(&self) -> Result<(), crate::domain::types::IpmiError> {
            self.commands.lock().unwrap().push("auto".to_owned());
            Ok(())
        }

        fn disable_auto(&self) -> Result<(), crate::domain::types::IpmiError> {
            self.commands.lock().unwrap().push("manual".to_owned());
            Ok(())
        }

        fn set_manual_pct(&self, pct: FanPercent) -> Result<(), crate::domain::types::IpmiError> {
            self.commands
                .lock()
                .unwrap()
                .push(format!("set:{}", pct.get()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<Event>>,
    }

    impl EventSink for Recorder {
        fn emit(&self, event: &Event) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    impl Recorder {
        fn names(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.kind().as_str())
                .collect()
        }

        fn has_speed(&self, pct: i32) -> bool {
            self.events.lock().unwrap().iter().any(
                |event| matches!(event, Event::SpeedChange { fan_pct, .. } if fan_pct.get() == pct),
            )
        }
    }

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

    impl SharedBuffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    enum SleepStep {
        Continue,
        Stop,
        Run(Box<dyn FnOnce() + Send>),
    }

    struct ScriptedSleeper {
        steps: Mutex<VecDeque<SleepStep>>,
    }

    impl ScriptedSleeper {
        fn new(steps: Vec<SleepStep>) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(steps.into()),
            })
        }
    }

    impl Sleeper for ScriptedSleeper {
        fn sleep_interruptible(&self, _stop: &AtomicBool, _seconds: u64) -> bool {
            match self.steps.lock().unwrap().pop_front() {
                Some(SleepStep::Stop) | None => true,
                Some(SleepStep::Continue) => false,
                Some(SleepStep::Run(action)) => {
                    action();
                    false
                }
            }
        }
    }

    struct FailingPolicy;

    impl SpeedPolicy for FailingPolicy {
        fn target(
            &self,
            _curve: &[crate::domain::curve::CurvePoint],
            _temp_c: TempC,
        ) -> Result<Option<i32>, DomainError> {
            Err(DomainError::new("policy exploded"))
        }
    }

    struct Harness {
        deps: RuntimeDeps,
        recorder: Arc<Recorder>,
        log: SharedBuffer,
        device: Arc<FakeDevice>,
    }

    fn harness(temps: &[i32], steps: Vec<SleepStep>, policy: Arc<dyn SpeedPolicy>) -> Harness {
        let device = FakeDevice::new(temps);
        let recorder = Arc::new(Recorder::default());
        let log = SharedBuffer::default();
        let (recorder_for_sinks, log_for_sinks) = (recorder.clone(), log.clone());
        let device_for_factory = device.clone();
        let deps = RuntimeDeps {
            build_device: Arc::new(move |_settings: &Settings| {
                device_for_factory.clone() as Arc<dyn FanDevice>
            }),
            make_sinks: Arc::new(move |verbose| RuntimeSinks {
                log: Arc::new(LoggingSink::with_writer(
                    verbose,
                    Box::new(log_for_sinks.clone()),
                )),
                additional: vec![recorder_for_sinks.clone()],
            }),
            policy,
            sleeper: ScriptedSleeper::new(steps),
            clock: Arc::new(SystemClock),
            install_signals: false,
        };
        Harness {
            deps,
            recorder,
            log,
            device,
        }
    }

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("breezed.toml");
        fs::write(&path, body).unwrap();
        path
    }

    fn bump_mtime(path: &Path) {
        let stamp = SystemTime::now() + Duration::from_secs(2);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
    }

    fn run(h: &Harness, config: &Path) -> Result<(), CliError> {
        run_with(config, None, false, &h.deps)
    }

    #[test]
    fn startup_tick_and_shutdown_happen_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), CONFIG);
        let h = harness(
            &[50],
            vec![SleepStep::Continue, SleepStep::Stop],
            Arc::new(CurvePolicy),
        );

        run(&h, &config).unwrap();

        assert_eq!(
            h.recorder.names(),
            [
                "startup",
                "mode_change",
                "speed_change",
                "poll",
                "mode_change",
                "shutdown",
            ]
        );
        assert_eq!(h.device.commands(), ["manual", "set:7", "auto"]);
    }

    #[test]
    fn immediate_stop_still_emits_startup_and_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), CONFIG);
        let h = harness(&[50], vec![SleepStep::Stop], Arc::new(CurvePolicy));

        run(&h, &config).unwrap();

        assert_eq!(h.recorder.names(), ["startup", "shutdown"]);
        assert!(h.device.commands().is_empty());
    }

    #[test]
    fn reload_applies_new_settings_and_emits_config_reload() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), CONFIG);
        let edit_path = config.clone();
        let h = harness(
            &[50, 50, 50],
            vec![
                SleepStep::Continue,
                SleepStep::Run(Box::new(move || {
                    fs::write(&edit_path, FLAT_CONFIG).unwrap();
                    bump_mtime(&edit_path);
                })),
                SleepStep::Continue,
                SleepStep::Stop,
            ],
            Arc::new(CurvePolicy),
        );

        run(&h, &config).unwrap();

        let names = h.recorder.names();
        assert!(names.contains(&"config_reload"), "{names:?}");
        assert!(h.recorder.has_speed(20), "{:?}", h.recorder.names());
        assert_eq!(h.device.commands(), ["manual", "set:7", "set:20", "auto"]);
    }

    #[test]
    fn invalid_reload_emits_config_error_and_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), CONFIG);
        let edit_path = config.clone();
        let h = harness(
            &[50, 50, 50],
            vec![
                SleepStep::Continue,
                SleepStep::Run(Box::new(move || {
                    fs::write(&edit_path, "[settings]\nhost = ").unwrap();
                    bump_mtime(&edit_path);
                })),
                SleepStep::Continue,
                SleepStep::Stop,
            ],
            Arc::new(CurvePolicy),
        );

        run(&h, &config).unwrap();

        let names = h.recorder.names();
        assert!(names.contains(&"config_error"), "{names:?}");
        assert!(!names.contains(&"config_reload"), "{names:?}");
        assert_eq!(h.device.commands(), ["manual", "set:7", "auto"]);
    }

    #[test]
    fn policy_error_logs_fatal_after_shutdown_and_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), CONFIG);
        let h = harness(&[50], vec![SleepStep::Continue], Arc::new(FailingPolicy));

        let error = run(&h, &config).unwrap_err();

        assert_eq!(error.code(), 1);
        assert_eq!(h.recorder.names(), ["startup", "shutdown"]);
        let log = h.log.text();
        assert!(log.contains(r#""event":"fatal""#), "{log}");
        assert!(log.contains("policy exploded"), "{log}");
        assert!(log.contains(r#""level":"ERROR""#), "{log}");
    }

    #[test]
    fn system_sleeper_handles_huge_intervals_without_overflow() {
        let stop = AtomicBool::new(true);
        assert!(SystemSleeper.sleep_interruptible(&stop, u64::MAX));
    }

    #[test]
    fn invalid_initial_config_exits_2_without_starting() {
        let h = harness(&[50], vec![SleepStep::Stop], Arc::new(CurvePolicy));
        let error = run(&h, Path::new("definitely-missing.toml")).unwrap_err();
        assert_eq!(error.code(), 2);
        assert!(h.recorder.names().is_empty());
    }

    #[test]
    fn metrics_bind_failure_degrades_and_logs_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();
        let config = write_config(
            dir.path(),
            &CONFIG.replace(
                "poll_interval_s = 1",
                &format!("poll_interval_s = 1\nmetrics_port = {port}"),
            ),
        );
        let h = harness(&[50], vec![SleepStep::Stop], Arc::new(CurvePolicy));

        run(&h, &config).unwrap();

        let log = h.log.text();
        assert!(log.contains("could not bind"), "{log}");
        assert!(log.contains(r#""level":"WARNING""#), "{log}");
        assert_eq!(h.recorder.names(), ["startup", "shutdown"]);
    }

    #[test]
    fn out_of_range_config_metrics_port_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(
            dir.path(),
            &CONFIG.replace(
                "poll_interval_s = 1",
                "poll_interval_s = 1\nmetrics_port = 70000",
            ),
        );
        let h = harness(&[50], vec![SleepStep::Stop], Arc::new(CurvePolicy));

        let error = run(&h, &config).unwrap_err();

        assert_eq!(error.code(), 2);
        assert!(h.recorder.names().is_empty());
    }

    #[test]
    fn curve_policy_is_used_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), CONFIG);
        let h = harness(
            &[52],
            vec![SleepStep::Continue, SleepStep::Stop],
            Arc::new(CurvePolicy),
        );
        run(&h, &config).unwrap();
        assert_eq!(h.device.commands(), ["manual", "set:7", "auto"]);
    }

    #[test]
    fn test_curve_helper_builds_expected_curve() {
        let curve = curve(&[(45, 6), (60, 8)]);
        assert_eq!(curve.points().len(), 2);
        assert_eq!(curve.points()[1].temp_c, TempC(60));
        assert!(EventType::ALL.iter().any(|event| event.as_str() == "poll"));
    }
}
