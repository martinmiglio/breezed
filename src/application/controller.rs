//! Control-loop state machine: hysteresis, failure fallback, hot-reload intake.
//!
//! No I/O beyond the injected ports; every observable action flows through the
//! injected event sinks. Hysteresis and AUTO fallback live here so they apply
//! uniformly to every target the policy produces.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::domain::events::Event;
use crate::domain::ports::{Clock, EventSink, FanDevice, SpeedPolicy};
use crate::domain::settings::Settings;
use crate::domain::types::{
    DomainError, FanPercent, IpmiError, OperatingMode, Operation, Reason, TempC, make_fan_pct,
};

/// Reads → decides → commands hardware → emits events, once per [`Controller::tick`].
pub struct Controller {
    device: Arc<dyn FanDevice>,
    policy: Arc<dyn SpeedPolicy>,
    settings: Settings,
    sinks: Vec<Arc<dyn EventSink>>,
    clock: Arc<dyn Clock>,
    state: OperatingMode,
    last_pct: Option<FanPercent>,
    failure_streak: u32,
    write_failures: u32,
    pending_down: Option<(FanPercent, f64)>,
}

impl Controller {
    /// Wire a controller from its ports.
    pub fn new(
        device: Arc<dyn FanDevice>,
        policy: Arc<dyn SpeedPolicy>,
        settings: Settings,
        sinks: Vec<Arc<dyn EventSink>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            device,
            policy,
            settings,
            sinks,
            clock,
            state: OperatingMode::Unknown,
            last_pct: None,
            failure_streak: 0,
            write_failures: 0,
            pending_down: None,
        }
    }

    fn emit(&self, event: &Event) {
        for sink in &self.sinks {
            sink.emit(event);
        }
    }

    /// Run one commander write, reporting failures instead of letting them
    /// escape. A failed write leaves caller state untouched so the next tick can
    /// retry from the last known-good state.
    fn handle_write(
        &mut self,
        operation: Operation,
        reason: Reason,
        result: Result<(), IpmiError>,
    ) -> bool {
        match result {
            Ok(()) => {
                self.write_failures = 0;
                true
            }
            Err(error) => {
                self.write_failures += 1;
                self.emit(&Event::IpmiError {
                    operation: Some(operation),
                    reason: Some(reason),
                    failures: self.write_failures,
                    error: error.message().to_owned(),
                });
                false
            }
        }
    }

    fn apply_speed(&mut self, pct: FanPercent, target: i32, reason: Reason) -> bool {
        let result = self.device.set_manual_pct(pct);
        if !self.handle_write(Operation::SetManualPct, reason, result) {
            return false;
        }
        self.last_pct = Some(pct);
        self.emit(&Event::SpeedChange {
            fan_pct: pct,
            target_pct: target,
            reason,
        });
        true
    }

    fn force_auto(&mut self, reason: Reason, temp_c: Option<TempC>, failures: Option<u32>) {
        let result = self.device.enable_auto();
        if !self.handle_write(Operation::EnableAuto, reason, result) {
            return;
        }
        let old = self.state;
        self.state = OperatingMode::Auto;
        self.emit(&Event::ModeChange {
            from: old,
            to: OperatingMode::Auto,
            reason,
            temp_c,
            failures,
        });
    }

    fn enter_manual(&mut self, pct: FanPercent, target: i32) {
        let result = self.device.disable_auto();
        if !self.handle_write(Operation::DisableAuto, Reason::TempUnderCurve, result) {
            return;
        }
        let result = self.device.set_manual_pct(pct);
        if !self.handle_write(Operation::SetManualPct, Reason::TempUnderCurve, result) {
            return;
        }
        let old = self.state;
        self.state = OperatingMode::Manual;
        self.last_pct = Some(pct);
        self.pending_down = None;
        self.emit(&Event::ModeChange {
            from: old,
            to: OperatingMode::Manual,
            reason: Reason::TempUnderCurve,
            temp_c: None,
            failures: None,
        });
        self.emit(&Event::SpeedChange {
            fan_pct: pct,
            target_pct: target,
            reason: Reason::ModeEnter,
        });
    }

    fn gate_downward(&mut self, target: i32, pct: FanPercent) {
        let now = self.clock.monotonic();
        match self.pending_down {
            None => {
                self.pending_down = Some((pct, now));
                self.emit(&Event::HysteresisWait {
                    target_pct: target,
                    hysteresis_s: self.settings.step_down_hysteresis_s(),
                });
            }
            Some((applied, started)) => {
                #[allow(clippy::cast_precision_loss)]
                let window = self.settings.step_down_hysteresis_s() as f64;
                if now - started >= window
                    && self.apply_speed(applied, target, Reason::HysteresisElapsed)
                {
                    self.pending_down = None;
                }
            }
        }
    }

    /// Perform one control-loop iteration.
    ///
    /// # Errors
    ///
    /// Propagates an invalid-curve error from the speed policy.
    pub fn tick(&mut self) -> Result<(), DomainError> {
        let temp = match self.device.read_max_cpu_temp() {
            Ok(temp) => temp,
            Err(error) => {
                self.failure_streak += 1;
                self.emit(&Event::IpmiError {
                    operation: None,
                    reason: None,
                    failures: self.failure_streak,
                    error: error.message().to_owned(),
                });
                let limit = u32::try_from(self.settings.read_failure_limit()).unwrap_or(u32::MAX);
                if self.failure_streak >= limit && self.state != OperatingMode::Auto {
                    self.force_auto(Reason::ReadFailures, None, Some(self.failure_streak));
                }
                return Ok(());
            }
        };

        self.failure_streak = 0;
        let target = self.policy.target(self.settings.curve().points(), temp)?;

        if let Some(target) = target {
            match make_fan_pct(i64::from(target)) {
                Err(error) => self.emit(&Event::ConfigError {
                    error: error.to_string(),
                }),
                Ok(pct) => {
                    if self.state != OperatingMode::Manual {
                        self.enter_manual(pct, target);
                    } else if let Some(last_pct) = self.last_pct {
                        match target.cmp(&last_pct.get()) {
                            Ordering::Greater => {
                                self.apply_speed(pct, target, Reason::TempRise);
                                self.pending_down = None;
                            }
                            Ordering::Less => self.gate_downward(target, pct),
                            Ordering::Equal => self.pending_down = None,
                        }
                    } else {
                        // Invariant: manual mode always records a last percentage.
                        self.enter_manual(pct, target);
                    }
                }
            }
        } else {
            self.pending_down = None;
            if self.state != OperatingMode::Auto {
                self.force_auto(Reason::TempAboveCurve, Some(temp), None);
            }
        }

        self.emit(&Event::Poll {
            temp_c: temp,
            fan_pct: self.last_pct,
            mode: self.state,
            target_pct: target,
        });
        Ok(())
    }

    /// Restore AUTO unless already there or no command was ever issued.
    pub fn shutdown(&mut self) {
        if self.state != OperatingMode::Manual {
            return;
        }
        self.force_auto(Reason::Shutdown, None, None);
    }

    /// Swap in hot-reloaded settings. The config adapter guarantees the curve is
    /// already validated, so this cannot fail.
    pub fn replace_settings(&mut self, new_settings: Settings) {
        self.settings = new_settings;
        self.pending_down = None;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::assert_is_empty)]

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    use super::*;
    use crate::domain::curve::{CurvePoint, CurvePolicy};
    use crate::domain::ports::{FanCommander, TempReader};
    use crate::domain::settings::Settings;
    use crate::domain::test_support::{curve, settings};
    use crate::domain::types::{EventType, TempC};

    struct FakeClock {
        now: Mutex<f64>,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(0.0),
            })
        }

        fn advance(&self, seconds: f64) {
            *self.now.lock().unwrap() += seconds;
        }
    }

    impl Clock for FakeClock {
        fn monotonic(&self) -> f64 {
            *self.now.lock().unwrap()
        }
    }

    #[derive(Clone)]
    enum ReadStep {
        Temp(TempC),
        Error(IpmiError),
    }

    #[derive(Default)]
    struct FakeState {
        script: Vec<ReadStep>,
        commands: Vec<String>,
    }

    struct FakeIpmi {
        state: Mutex<FakeState>,
        fail_enable_auto: AtomicBool,
        fail_disable_auto: AtomicBool,
        fail_set_manual: AtomicBool,
    }

    impl FakeIpmi {
        fn new(script: Vec<ReadStep>) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeState {
                    script,
                    commands: Vec::new(),
                }),
                fail_enable_auto: AtomicBool::new(false),
                fail_disable_auto: AtomicBool::new(false),
                fail_set_manual: AtomicBool::new(false),
            })
        }

        fn commands(&self) -> Vec<String> {
            self.state.lock().unwrap().commands.clone()
        }

        fn set_fail_enable_auto(&self, value: bool) {
            self.fail_enable_auto.store(value, Ordering::SeqCst);
        }

        fn set_fail_disable_auto(&self, value: bool) {
            self.fail_disable_auto.store(value, Ordering::SeqCst);
        }

        fn set_fail_set_manual(&self, value: bool) {
            self.fail_set_manual.store(value, Ordering::SeqCst);
        }
    }

    impl TempReader for FakeIpmi {
        fn read_max_cpu_temp(&self) -> Result<TempC, IpmiError> {
            let mut state = self.state.lock().unwrap();
            match state.script.remove(0) {
                ReadStep::Temp(temp) => Ok(temp),
                ReadStep::Error(error) => Err(error),
            }
        }
    }

    impl FanCommander for FakeIpmi {
        fn enable_auto(&self) -> Result<(), IpmiError> {
            if self.fail_enable_auto.load(Ordering::SeqCst) {
                return Err(IpmiError::new("enable_auto failed"));
            }
            self.state.lock().unwrap().commands.push("auto".to_owned());
            Ok(())
        }

        fn disable_auto(&self) -> Result<(), IpmiError> {
            if self.fail_disable_auto.load(Ordering::SeqCst) {
                return Err(IpmiError::new("disable_auto failed"));
            }
            self.state
                .lock()
                .unwrap()
                .commands
                .push("manual".to_owned());
            Ok(())
        }

        fn set_manual_pct(&self, pct: FanPercent) -> Result<(), IpmiError> {
            if self.fail_set_manual.load(Ordering::SeqCst) {
                return Err(IpmiError::new(format!(
                    "set_manual_pct {} failed",
                    pct.get()
                )));
            }
            self.state
                .lock()
                .unwrap()
                .commands
                .push(format!("set:{}", pct.get()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        records: Mutex<Vec<Event>>,
    }

    impl EventSink for RecordingSink {
        fn emit(&self, event: &Event) {
            self.records.lock().unwrap().push(event.clone());
        }
    }

    impl RecordingSink {
        fn events(&self, kind: EventType) -> Vec<Event> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.kind() == kind)
                .cloned()
                .collect()
        }

        fn all(&self) -> Vec<Event> {
            self.records.lock().unwrap().clone()
        }
    }

    struct RecordingPolicy {
        target: AtomicI32,
        calls: Mutex<Vec<(Vec<CurvePoint>, TempC)>>,
    }

    impl RecordingPolicy {
        fn new(target: i32) -> Arc<Self> {
            Arc::new(Self {
                target: AtomicI32::new(target),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl SpeedPolicy for RecordingPolicy {
        fn target(&self, curve: &[CurvePoint], temp_c: TempC) -> Result<Option<i32>, DomainError> {
            self.calls.lock().unwrap().push((curve.to_vec(), temp_c));
            Ok(Some(self.target.load(Ordering::SeqCst)))
        }
    }

    struct Harness {
        controller: Controller,
        ipmi: Arc<FakeIpmi>,
        sink: Arc<RecordingSink>,
    }

    fn harness(
        script: Vec<ReadStep>,
        settings: Settings,
        policy: Arc<dyn SpeedPolicy>,
        clock: Arc<FakeClock>,
    ) -> Harness {
        let ipmi = FakeIpmi::new(script);
        let sink = Arc::new(RecordingSink::default());
        let controller = Controller::new(ipmi.clone(), policy, settings, vec![sink.clone()], clock);
        Harness {
            controller,
            ipmi,
            sink,
        }
    }

    fn default_harness(script: Vec<ReadStep>) -> Harness {
        harness(script, settings(), Arc::new(CurvePolicy), FakeClock::new())
    }

    fn temps(values: &[i32]) -> Vec<ReadStep> {
        values
            .iter()
            .map(|&value| ReadStep::Temp(TempC(value)))
            .collect()
    }

    fn errors(count: usize) -> Vec<ReadStep> {
        (0..count)
            .map(|_| ReadStep::Error(IpmiError::new("sdr failed")))
            .collect()
    }

    fn mode_changes(sink: &RecordingSink) -> Vec<Event> {
        sink.events(EventType::ModeChange)
    }

    fn speed_changes(sink: &RecordingSink) -> Vec<Event> {
        sink.events(EventType::SpeedChange)
    }

    fn ipmi_errors(sink: &RecordingSink) -> Vec<Event> {
        sink.events(EventType::IpmiError)
    }

    fn hysteresis_waits(sink: &RecordingSink) -> Vec<Event> {
        sink.events(EventType::HysteresisWait)
    }

    fn polls(sink: &RecordingSink) -> Vec<Event> {
        sink.events(EventType::Poll)
    }

    #[test]
    fn cold_start_no_fan_commands_before_first_read() {
        let mut h = default_harness([errors(2), temps(&[40])].concat());
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert!(h.ipmi.commands().is_empty());
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:6"]);
    }

    #[test]
    fn under_curve_from_unknown_switches_to_manual_with_pct() {
        let mut h = default_harness(temps(&[40]));
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:6"]);
        assert_eq!(
            mode_changes(&h.sink),
            [Event::ModeChange {
                from: OperatingMode::Unknown,
                to: OperatingMode::Manual,
                reason: Reason::TempUnderCurve,
                temp_c: None,
                failures: None,
            }]
        );
        assert_eq!(
            speed_changes(&h.sink),
            [Event::SpeedChange {
                fan_pct: FanPercent::from_validated(6),
                target_pct: 6,
                reason: Reason::ModeEnter,
            }]
        );
    }

    #[test]
    fn returns_under_curve_after_hysteresis_resumes_manual() {
        let mut h = default_harness(temps(&[66, 80, 62]));
        for _ in 0..3 {
            h.controller.tick().unwrap();
        }
        assert_eq!(
            h.ipmi.commands(),
            ["manual", "set:11", "auto", "manual", "set:9"]
        );
        assert_eq!(
            mode_changes(&h.sink).last(),
            Some(&Event::ModeChange {
                from: OperatingMode::Auto,
                to: OperatingMode::Manual,
                reason: Reason::TempUnderCurve,
                temp_c: None,
                failures: None,
            })
        );
        let speeds = speed_changes(&h.sink);
        assert_eq!(
            speeds[speeds.len() - 2],
            Event::SpeedChange {
                fan_pct: FanPercent::from_validated(11),
                target_pct: 11,
                reason: Reason::ModeEnter,
            }
        );
        assert_eq!(
            speeds.last().unwrap(),
            &Event::SpeedChange {
                fan_pct: FanPercent::from_validated(9),
                target_pct: 9,
                reason: Reason::ModeEnter,
            }
        );
    }

    #[test]
    fn third_consecutive_failure_forces_auto_once() {
        let mut h = default_harness(errors(4));
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert!(h.ipmi.commands().is_empty());
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["auto"]);
        assert_eq!(
            mode_changes(&h.sink),
            [Event::ModeChange {
                from: OperatingMode::Unknown,
                to: OperatingMode::Auto,
                reason: Reason::ReadFailures,
                temp_c: None,
                failures: Some(3),
            }]
        );
        assert_eq!(ipmi_errors(&h.sink).len(), 4);
    }

    #[test]
    fn good_read_resets_failure_streak() {
        let mut h = default_harness([errors(2), temps(&[40]), errors(2)].concat());
        for _ in 0..5 {
            h.controller.tick().unwrap();
        }
        assert_eq!(h.ipmi.commands(), ["manual", "set:6"]);
    }

    #[test]
    fn upward_speed_change_applies_immediately() {
        let mut h = default_harness(temps(&[60, 68]));
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:8", "set:12"]);
        assert_eq!(
            speed_changes(&h.sink).last(),
            Some(&Event::SpeedChange {
                fan_pct: FanPercent::from_validated(12),
                target_pct: 12,
                reason: Reason::TempRise,
            })
        );
    }

    #[test]
    fn downward_change_applies_only_after_full_hysteresis() {
        let clock = FakeClock::new();
        let mut h = harness(
            temps(&[68, 60, 60, 60]),
            settings(),
            Arc::new(CurvePolicy),
            clock.clone(),
        );
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12"]);
        assert_eq!(hysteresis_waits(&h.sink).len(), 1);
        clock.advance(29.0);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12"]);
        assert_eq!(hysteresis_waits(&h.sink).len(), 1);
        clock.advance(1.0);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12", "set:8"]);
        assert_eq!(
            speed_changes(&h.sink).last(),
            Some(&Event::SpeedChange {
                fan_pct: FanPercent::from_validated(8),
                target_pct: 8,
                reason: Reason::HysteresisElapsed,
            })
        );
    }

    #[test]
    fn midwindow_rise_cancels_pending_down_step() {
        let clock = FakeClock::new();
        let mut h = harness(
            temps(&[68, 60, 70, 60]),
            settings(),
            Arc::new(CurvePolicy),
            clock.clone(),
        );
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        clock.advance(15.0);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12", "set:14"]);
        clock.advance(100.0);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12", "set:14"]);
        assert_eq!(hysteresis_waits(&h.sink).len(), 2);
    }

    #[test]
    fn equal_target_issues_no_command() {
        let mut h = default_harness(temps(&[60, 60]));
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:8"]);
        let speeds = speed_changes(&h.sink);
        assert_eq!(speeds.len(), 1);
        assert!(matches!(
            speeds[0],
            Event::SpeedChange {
                reason: Reason::ModeEnter,
                ..
            }
        ));
        assert_eq!(polls(&h.sink).len(), 2);
    }

    #[test]
    fn shutdown_restores_auto_even_from_manual() {
        let mut h = default_harness(temps(&[60]));
        h.controller.tick().unwrap();
        h.controller.shutdown();
        assert_eq!(h.ipmi.commands(), ["manual", "set:8", "auto"]);
        assert_eq!(
            mode_changes(&h.sink).last(),
            Some(&Event::ModeChange {
                from: OperatingMode::Manual,
                to: OperatingMode::Auto,
                reason: Reason::Shutdown,
                temp_c: None,
                failures: None,
            })
        );
        h.controller.shutdown();
        assert_eq!(h.ipmi.commands(), ["manual", "set:8", "auto"]);
        assert_eq!(mode_changes(&h.sink).len(), 2);
    }

    #[test]
    fn failure_limit_idempotent_while_already_auto() {
        let mut h = default_harness([temps(&[80]), errors(3)].concat());
        for _ in 0..4 {
            h.controller.tick().unwrap();
        }
        assert_eq!(h.ipmi.commands(), ["auto"]);
    }

    #[test]
    fn replace_settings_success_uses_new_curve_next_tick() {
        let mut h = harness(
            temps(&[46]),
            Settings::for_tests(curve(&[(40, 5)])),
            Arc::new(CurvePolicy),
            FakeClock::new(),
        );
        let new_settings = Settings::for_tests(curve(&[(45, 6), (47, 30)]));
        h.controller.replace_settings(new_settings);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:18"]);
    }

    #[test]
    fn shutdown_tolerates_ipmi_error_from_enable_auto() {
        let mut h = default_harness(temps(&[60]));
        h.ipmi.set_fail_enable_auto(true);
        h.controller.tick().unwrap();
        h.controller.shutdown();
        assert_eq!(
            ipmi_errors(&h.sink),
            [Event::IpmiError {
                operation: Some(Operation::EnableAuto),
                reason: Some(Reason::Shutdown),
                failures: 1,
                error: "enable_auto failed".to_owned(),
            }]
        );
        h.controller.shutdown();
        assert_eq!(
            ipmi_errors(&h.sink).last(),
            Some(&Event::IpmiError {
                operation: Some(Operation::EnableAuto),
                reason: Some(Reason::Shutdown),
                failures: 2,
                error: "enable_auto failed".to_owned(),
            })
        );
    }

    #[test]
    fn read_failure_fallback_write_timeout_is_non_fatal_and_retried() {
        let mut h = default_harness([temps(&[60]), errors(5)].concat());
        h.ipmi.set_fail_enable_auto(true);
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert_eq!(mode_changes(&h.sink).len(), 1);
        assert_eq!(
            ipmi_errors(&h.sink).last(),
            Some(&Event::IpmiError {
                operation: Some(Operation::EnableAuto),
                reason: Some(Reason::ReadFailures),
                failures: 1,
                error: "enable_auto failed".to_owned(),
            })
        );
        h.controller.tick().unwrap();
        assert!(matches!(
            ipmi_errors(&h.sink).last(),
            Some(Event::IpmiError { failures: 2, .. })
        ));
        h.ipmi.set_fail_enable_auto(false);
        h.controller.tick().unwrap();
        assert_eq!(
            mode_changes(&h.sink).last(),
            Some(&Event::ModeChange {
                from: OperatingMode::Manual,
                to: OperatingMode::Auto,
                reason: Reason::ReadFailures,
                temp_c: None,
                failures: Some(5),
            })
        );
    }

    #[test]
    fn set_manual_timeout_during_manual_enter_keeps_state_and_retries() {
        let mut h = default_harness(temps(&[60, 60]));
        h.ipmi.set_fail_set_manual(true);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual"]);
        assert!(mode_changes(&h.sink).is_empty());
        assert_eq!(
            ipmi_errors(&h.sink),
            [Event::IpmiError {
                operation: Some(Operation::SetManualPct),
                reason: Some(Reason::TempUnderCurve),
                failures: 1,
                error: "set_manual_pct 8 failed".to_owned(),
            }]
        );
        h.ipmi.set_fail_set_manual(false);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "manual", "set:8"]);
        assert_eq!(
            mode_changes(&h.sink),
            [Event::ModeChange {
                from: OperatingMode::Unknown,
                to: OperatingMode::Manual,
                reason: Reason::TempUnderCurve,
                temp_c: None,
                failures: None,
            }]
        );
    }

    #[test]
    fn disable_auto_timeout_keeps_state_and_retries() {
        let mut h = default_harness(temps(&[60, 60]));
        h.ipmi.set_fail_disable_auto(true);
        h.controller.tick().unwrap();
        assert!(h.ipmi.commands().is_empty());
        assert_eq!(
            ipmi_errors(&h.sink),
            [Event::IpmiError {
                operation: Some(Operation::DisableAuto),
                reason: Some(Reason::TempUnderCurve),
                failures: 1,
                error: "disable_auto failed".to_owned(),
            }]
        );
        h.ipmi.set_fail_disable_auto(false);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:8"]);
        assert_eq!(
            mode_changes(&h.sink).last(),
            Some(&Event::ModeChange {
                from: OperatingMode::Unknown,
                to: OperatingMode::Manual,
                reason: Reason::TempUnderCurve,
                temp_c: None,
                failures: None,
            })
        );
    }

    #[test]
    fn temp_rise_write_timeout_keeps_last_pct_and_retries() {
        let mut h = default_harness(temps(&[60, 68, 68]));
        h.controller.tick().unwrap();
        h.ipmi.set_fail_set_manual(true);
        h.controller.tick().unwrap();
        assert_eq!(
            ipmi_errors(&h.sink).last(),
            Some(&Event::IpmiError {
                operation: Some(Operation::SetManualPct),
                reason: Some(Reason::TempRise),
                failures: 1,
                error: "set_manual_pct 12 failed".to_owned(),
            })
        );
        h.ipmi.set_fail_set_manual(false);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:8", "set:12"]);
        assert_eq!(
            speed_changes(&h.sink).last(),
            Some(&Event::SpeedChange {
                fan_pct: FanPercent::from_validated(12),
                target_pct: 12,
                reason: Reason::TempRise,
            })
        );
    }

    #[test]
    fn hysteresis_write_timeout_keeps_pending_step_and_retries() {
        let clock = FakeClock::new();
        let mut h = harness(
            temps(&[68, 60, 60, 60]),
            settings(),
            Arc::new(CurvePolicy),
            clock.clone(),
        );
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        clock.advance(30.0);
        h.ipmi.set_fail_set_manual(true);
        h.controller.tick().unwrap();
        assert_eq!(
            ipmi_errors(&h.sink).last(),
            Some(&Event::IpmiError {
                operation: Some(Operation::SetManualPct),
                reason: Some(Reason::HysteresisElapsed),
                failures: 1,
                error: "set_manual_pct 8 failed".to_owned(),
            })
        );
        h.ipmi.set_fail_set_manual(false);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12", "set:8"]);
        assert_eq!(
            speed_changes(&h.sink).last(),
            Some(&Event::SpeedChange {
                fan_pct: FanPercent::from_validated(8),
                target_pct: 8,
                reason: Reason::HysteresisElapsed,
            })
        );
    }

    #[test]
    fn curve_target_drives_controller_to_manual() {
        let settings = Settings::for_tests(curve(&[(0, 42), (100, 42)]));
        let mut h = harness(
            temps(&[30, 85]),
            settings,
            Arc::new(CurvePolicy),
            FakeClock::new(),
        );
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:42"]);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:42"]);
    }

    #[test]
    fn poll_event_carries_full_field_set_in_auto() {
        let mut h = default_harness(temps(&[80]));
        h.controller.tick().unwrap();
        assert_eq!(
            mode_changes(&h.sink),
            [Event::ModeChange {
                from: OperatingMode::Unknown,
                to: OperatingMode::Auto,
                reason: Reason::TempAboveCurve,
                temp_c: Some(TempC(80)),
                failures: None,
            }]
        );
        assert_eq!(
            polls(&h.sink),
            [Event::Poll {
                temp_c: TempC(80),
                fan_pct: None,
                mode: OperatingMode::Auto,
                target_pct: None,
            }]
        );
    }

    #[test]
    fn drifted_stored_pct_hysteresis_applies_pending_value_at_expiry() {
        let clock = FakeClock::new();
        let mut h = harness(
            temps(&[68, 60, 52, 52]),
            settings(),
            Arc::new(CurvePolicy),
            clock.clone(),
        );
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        clock.advance(15.0);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12"]);
        clock.advance(20.0);
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:12", "set:8"]);
        assert_eq!(
            speed_changes(&h.sink).last(),
            Some(&Event::SpeedChange {
                fan_pct: FanPercent::from_validated(8),
                target_pct: 7,
                reason: Reason::HysteresisElapsed,
            })
        );
    }

    #[test]
    fn replace_settings_clears_pending_down_step() {
        let clock = FakeClock::new();
        let mut h = harness(
            temps(&[68, 60, 60]),
            settings(),
            Arc::new(CurvePolicy),
            clock.clone(),
        );
        h.controller.tick().unwrap();
        h.controller.tick().unwrap();
        assert_eq!(hysteresis_waits(&h.sink).len(), 1);
        h.controller.replace_settings(settings());
        clock.advance(100.0);
        h.controller.tick().unwrap();
        assert!(!h.ipmi.commands().contains(&"set:8".to_owned()));
        assert_eq!(hysteresis_waits(&h.sink).len(), 2);
    }

    #[test]
    fn uses_injected_speed_policy_instead_of_curve_interpolation() {
        let policy = RecordingPolicy::new(42);
        let mut h = harness(temps(&[50]), settings(), policy.clone(), FakeClock::new());
        h.controller.tick().unwrap();
        assert_eq!(h.ipmi.commands(), ["manual", "set:42"]);
        assert_eq!(
            policy.calls.lock().unwrap().as_slice(),
            [(settings().curve().points().to_vec(), TempC(50))]
        );
    }

    #[test]
    fn out_of_range_policy_target_emits_config_error_and_skips_command() {
        let policy = RecordingPolicy::new(101);
        let mut h = harness(temps(&[40]), settings(), policy, FakeClock::new());
        h.controller.tick().unwrap();
        assert!(h.ipmi.commands().is_empty());
        assert_eq!(
            h.sink.events(EventType::ConfigError),
            [Event::ConfigError {
                error: "fan_pct must be in 1..100, got 101".to_owned(),
            }]
        );
        assert_eq!(polls(&h.sink).len(), 1);
        assert!(mode_changes(&h.sink).is_empty());
    }

    #[test]
    fn all_recorded_events_have_a_closed_kind() {
        let mut h = default_harness(temps(&[40]));
        h.controller.tick().unwrap();
        for event in h.sink.all() {
            assert!(EventType::ALL.contains(&event.kind()));
        }
    }
}
