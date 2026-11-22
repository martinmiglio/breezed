//! Native IPMI adapter over RMCP+.
//!
//! This is the experimental replacement for the `ipmitool` subprocess adapter:
//! one long-lived session, the SDR repository loaded once, and no fork per poll.
//!
//! Enabled with the `native-ipmi` feature and selected at runtime with
//! `BREEZED_NATIVE_IPMI=1`. The `ipmitool` adapter remains the default until
//! this path has seen production use.

use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ipmi_rs::app::auth::{PrivilegeLevel, SetSessionPrivilegeLevel};
use ipmi_rs::connection::{IpmiConnection, LogicalUnit, Message, Request, RequestTargetAddress};
use ipmi_rs::rmcp::Rmcp;
use ipmi_rs::sensor_event::{GetSensorReading, ThresholdReading};
use ipmi_rs::storage::sdr::record::{FullSensorRecord, InstancedSensor, SensorKey};
use ipmi_rs::storage::sdr::{GetSdrRepositoryInfo, SensorType, Unit};

use crate::domain::ports::{FanCommander, TempReader};
use crate::domain::settings::Settings;
use crate::domain::types::{FanPercent, IpmiError, TempC};

/// Per-request timeout for the RMCP+ socket.
const TIMEOUT: Duration = Duration::from_secs(3);
/// Connection attempts before a poll is reported as failed.
const CONNECT_ATTEMPTS: u32 = 2;
/// Delay between connection attempts.
const CONNECT_BACKOFF: Duration = Duration::from_millis(250);
/// Wall-clock budget for reading the whole SDR repository.
const SDR_LOAD_BUDGET: Duration = Duration::from_secs(15);
/// Upper bound on the Debug-formatted ipmi-rs error text copied into a message.
const DEBUG_EXCERPT_CHARS: usize = 200;
/// Plausible CPU temperature range; readings outside it are rejected.
const TEMP_RANGE_C: std::ops::RangeInclusive<f64> = -100.0..=200.0;

/// IPMI netfn and command for Dell's OEM fan control.
const OEM_NETFN: u8 = 0x30;
const OEM_FAN_CMD: u8 = 0x30;

/// The SDR sensor numbers Dell uses for the CPU temperatures.
const CPU_SENSOR_NUMBERS: [u8; 2] = [0x0e, 0x0f];

/// A CPU temperature sensor and the record needed to convert its reading.
#[derive(Debug, Clone)]
struct Sensor {
    key: SensorKey,
    record: FullSensorRecord,
}

/// One live RMCP+ session with the sensor map loaded.
///
/// The session owns the connection so that every construction error after
/// activation (privilege rejection, SDR load failure) and every replaced
/// session releases its BMC slot through [`Drop`].
struct Session {
    ipmi: ipmi_rs::Ipmi<Rmcp>,
    sensors: Vec<Sensor>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // `Rmcp` deliberately does not close sessions in its own `Drop`.
        // Best effort: a failed close still leaves the session to time out.
        let _ = self.ipmi.inner_mut().deactivate();
    }
}

/// An adapter failure plus whether opening a new session could fix it.
#[derive(Debug)]
struct OpError {
    error: IpmiError,
    retryable: bool,
}

impl OpError {
    /// A failure that must be reported as-is; reconnecting would repeat it.
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            error: IpmiError::new(message),
            retryable: false,
        }
    }

    /// Wrap an ipmi-rs error. Only a transport/connection failure is retryable;
    /// completion codes, parse errors, and mismatched responses are not.
    fn from_ipmi<C: fmt::Debug, P: fmt::Debug>(
        context: &str,
        error: &ipmi_rs::IpmiError<C, P>,
    ) -> Self {
        let retryable = matches!(error, ipmi_rs::IpmiError::Connection(_));
        Self {
            error: IpmiError::new(format!("{context}: {}", debug_excerpt(error))),
            retryable,
        }
    }

    /// Wrap a transport-level error reported directly by the connection.
    fn from_connection(context: &str, error: impl fmt::Debug) -> Self {
        Self {
            error: IpmiError::new(format!("{context}: {}", debug_excerpt(&error))),
            retryable: true,
        }
    }

    fn into_ipmi(self) -> IpmiError {
        self.error
    }
}

/// Reads temperatures and commands fans over a native IPMI connection.
pub struct NativeIpmiClient {
    settings: Settings,
    session: Mutex<Option<Session>>,
}

impl NativeIpmiClient {
    /// Build a client; the session is opened on first use.
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            session: Mutex::new(None),
        }
    }

    /// Run `operation` against a live session, reconnecting once on a transport
    /// failure. Command-level failures are returned without touching the
    /// session, because a new session would fail identically.
    fn with_session<T>(
        &self,
        operation: impl Fn(&mut Session) -> Result<T, OpError>,
    ) -> Result<T, IpmiError> {
        let mut guard = lock(&self.session);

        if guard.is_none() {
            *guard = Some(self.connect()?);
        }

        match operation(guard.as_mut().expect("session was just connected")) {
            Ok(value) => Ok(value),
            Err(error) if !error.retryable => Err(error.error),
            Err(first) => {
                // Dropping the failed session releases its BMC slot before the
                // replacement is activated.
                let _ = guard.take();
                let session = match self.connect() {
                    Ok(session) => session,
                    Err(reconnect) => {
                        return Err(IpmiError::new(format!(
                            "{}; reconnect failed: {}",
                            first.error.message(),
                            reconnect.message()
                        )));
                    }
                };
                *guard = Some(session);
                operation(guard.as_mut().expect("session was just reconnected"))
                    .map_err(OpError::into_ipmi)
            }
        }
    }

    /// Connect with a small retry budget; a single lost UDP packet should not
    /// fail a poll.
    fn connect(&self) -> Result<Session, IpmiError> {
        let mut last_error = None;

        for attempt in 0..CONNECT_ATTEMPTS {
            match self.connect_once() {
                Ok(session) => return Ok(session),
                Err(error) => {
                    last_error = Some(error);
                    if attempt + 1 < CONNECT_ATTEMPTS {
                        std::thread::sleep(CONNECT_BACKOFF);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| IpmiError::new("could not connect to the iDRAC")))
    }

    fn connect_once(&self) -> Result<Session, IpmiError> {
        let mut connection = Rmcp::new((self.settings.host(), 623), TIMEOUT).map_err(|error| {
            IpmiError::new(format!("connect failed: {}", debug_excerpt(&error)))
        })?;

        connection
            .activate(
                true,
                Some(self.settings.user()),
                Some(self.settings.password().expose_secret().as_bytes()),
            )
            .map_err(|error| {
                IpmiError::new(format!(
                    "session activation failed: {}",
                    debug_excerpt(&error)
                ))
            })?;

        // From here on the `Session` owns the connection: dropping it on any
        // error path (privilege rejection, SDR load failure) releases the BMC
        // session slot instead of leaking it until the BMC's own timeout.
        let mut session = Session {
            ipmi: ipmi_rs::Ipmi::new(connection),
            sensors: Vec::new(),
        };

        // Activation negotiates the privilege but does not apply it. Fan
        // control is an Administrator-level command.
        session
            .ipmi
            .send_recv(SetSessionPrivilegeLevel {
                privilege_level: PrivilegeLevel::Administrator,
            })
            .map_err(|error| {
                IpmiError::new(format!(
                    "session privilege failed (the iDRAC account needs the Administrator role): {}",
                    debug_excerpt(&error)
                ))
            })?;

        session.sensors = load_sensors(&mut session.ipmi)?;

        Ok(session)
    }

    fn write_raw(&self, data: &[u8]) -> Result<(), IpmiError> {
        self.with_session(|session| {
            let mut request = fan_request(data);
            let response = session
                .ipmi
                .inner_mut()
                .send_recv(&mut request)
                .map_err(|error| OpError::from_connection("raw fan command failed", error))?;

            // This path bypasses `Ipmi::send_recv`, which normally checks the
            // response netfn and command, so verify them here before trusting
            // the completion code.
            if response.netfn().request_value() != OEM_NETFN || response.cmd() != OEM_FAN_CMD {
                return Err(OpError::fatal(format!(
                    "raw fan command got an unexpected response (netfn 0x{:02x}, cmd 0x{:02x})",
                    response.netfn().request_value(),
                    response.cmd()
                )));
            }

            if response.cc() != 0 {
                return Err(OpError::fatal(format!(
                    "raw fan command rejected (completion code 0x{:02x})",
                    response.cc()
                )));
            }

            Ok(())
        })
    }
}

impl Drop for NativeIpmiClient {
    fn drop(&mut self) {
        // Dropping the session runs `Session::drop`, which deactivates it.
        let _ = lock(&self.session).take();
    }
}

impl TempReader for NativeIpmiClient {
    fn read_max_cpu_temp(&self) -> Result<TempC, IpmiError> {
        self.with_session(|session| {
            let Session { ipmi, sensors } = session;
            let mut hottest = None::<i32>;
            let mut skipped = 0usize;

            for sensor in sensors.iter() {
                let reading = ipmi
                    .send_recv(GetSensorReading::for_sensor_key(&sensor.key))
                    .map_err(|error| {
                        OpError::from_ipmi(
                            &format!(
                                "sensor 0x{:02x} read failed",
                                sensor.key.sensor_number.get()
                            ),
                            &error,
                        )
                    })?;
                let threshold: ThresholdReading = (&reading).into();
                if threshold.scanning_disabled {
                    skipped += 1;
                    continue;
                }
                let value = threshold.reading.ok_or_else(|| {
                    OpError::fatal(format!(
                        "sensor 0x{:02x} has no reading",
                        sensor.key.sensor_number.get()
                    ))
                })?;
                let display = sensor.record.display_reading(value).ok_or_else(|| {
                    OpError::fatal(format!(
                        "sensor 0x{:02x} reading could not be converted",
                        sensor.key.sensor_number.get()
                    ))
                })?;
                let celsius = parse_celsius(&display).ok_or_else(|| {
                    OpError::fatal(format!(
                        "sensor 0x{:02x} returned an unexpected reading: {display}",
                        sensor.key.sensor_number.get()
                    ))
                })?;

                hottest = Some(hottest.map_or(celsius, |current| current.max(celsius)));
            }

            if !sensors.is_empty() && skipped == sensors.len() {
                return Err(OpError::fatal(
                    "all 0Eh/0Fh CPU temperature sensors are disabled (BMC scanning disabled)",
                ));
            }

            hottest
                .map(TempC)
                .ok_or_else(|| OpError::fatal("no CPU temperature sensors were loaded"))
        })
    }
}

impl FanCommander for NativeIpmiClient {
    fn enable_auto(&self) -> Result<(), IpmiError> {
        self.write_raw(&auto_payload(true))
    }

    fn disable_auto(&self) -> Result<(), IpmiError> {
        self.write_raw(&auto_payload(false))
    }

    fn set_manual_pct(&self, pct: FanPercent) -> Result<(), IpmiError> {
        let pct =
            u8::try_from(pct.get()).map_err(|_| IpmiError::new("fan percentage out of range"))?;
        self.write_raw(&manual_payload(pct))
    }
}

/// The Dell OEM payload that enables (or disables) iDRAC automatic control.
const fn auto_payload(enabled: bool) -> [u8; 2] {
    [0x01, if enabled { 0x01 } else { 0x00 }]
}

/// The Dell OEM payload that applies a fixed manual duty cycle.
const fn manual_payload(pct: u8) -> [u8; 3] {
    [0x02, 0xff, pct]
}

/// Build the raw OEM request for one fan-control payload.
fn fan_request(data: &[u8]) -> Request {
    Request::new(
        Message::new_raw(OEM_NETFN, OEM_FAN_CMD, data.to_vec()),
        RequestTargetAddress::Bmc(LogicalUnit::Zero),
    )
}

/// Whether an SDR record count is authoritative.
///
/// `0xFFFF` is the spec's "65,535 or more"/unknown sentinel, and `overflow`
/// means the BMC stopped counting.
fn sdr_count_known(record_count: u16, overflow: bool) -> bool {
    record_count != u16::MAX && !overflow
}

/// Whether the repository read is complete enough to use.
///
/// Extra records are allowed: the repository may grow while it is being read.
/// With an unknown count, at least one record must have been read.
fn sdr_is_complete(loaded: usize, expected: usize, count_known: bool) -> bool {
    if count_known {
        loaded >= expected
    } else {
        loaded > 0
    }
}

/// Load every SDR record, keep the CPU temperature sensors, and verify that
/// the repository was read completely.
fn load_sensors<C: IpmiConnection>(ipmi: &mut ipmi_rs::Ipmi<C>) -> Result<Vec<Sensor>, IpmiError> {
    let info = ipmi
        .send_recv(GetSdrRepositoryInfo)
        .map_err(|error| OpError::from_ipmi("SDR repository info failed", &error).into_ipmi())?;
    let expected = usize::from(info.record_count);
    let count_known = sdr_count_known(info.record_count, info.overflow);

    let started = Instant::now();
    let mut loaded = 0usize;
    let mut sensors = Vec::new();

    // `sdrs()` stops on a fatal error and skips a recoverable parse error.
    // Either way the record count comes up short, which is what the check
    // below detects.
    for record in ipmi.sdrs() {
        if started.elapsed() > SDR_LOAD_BUDGET {
            return Err(IpmiError::new(format!(
                "SDR repository load exceeded {}s after {loaded} records",
                SDR_LOAD_BUDGET.as_secs()
            )));
        }
        loaded += 1;

        let Some(common) = record.common_data() else {
            continue;
        };
        if common.ty != SensorType::Temperature
            || common.sensor_units.base_unit != Unit::DegreesCelsius
        {
            continue;
        }

        let Some(record) = record.full_sensor() else {
            continue;
        };
        let key = record.key_data();
        if !CPU_SENSOR_NUMBERS.contains(&key.sensor_number.get()) {
            continue;
        }

        sensors.push(Sensor {
            key: *key,
            record: record.clone(),
        });
    }

    if !sdr_is_complete(loaded, expected, count_known) {
        let message = if count_known {
            format!("SDR repository incomplete: read {loaded} of {expected} records")
        } else {
            "SDR repository returned no records".to_owned()
        };
        return Err(IpmiError::new(message));
    }
    if sensors.is_empty() {
        return Err(IpmiError::new(
            "no 0Eh/0Fh CPU temperature sensors in the SDR repository",
        ));
    }

    Ok(sensors)
}

/// Truncate a Debug-formatted error so one bad packet cannot flood a log line.
fn debug_excerpt(value: &impl fmt::Debug) -> String {
    let text = format!("{value:?}");
    if text.chars().count() <= DEBUG_EXCERPT_CHARS {
        return text;
    }
    let mut truncated: String = text.chars().take(DEBUG_EXCERPT_CHARS).collect();
    truncated.push('…');
    truncated
}

/// Parse a Celsius display reading such as `49.9 °C`, rounding to a whole
/// degree. Readings in other units are rejected.
fn parse_celsius(display: &str) -> Option<i32> {
    let mut parts = display.split_whitespace();
    let token = parts.next()?;
    if parts.next()? != "°C" {
        return None;
    }
    let value: f64 = token.parse().ok()?;
    if !TEMP_RANGE_C.contains(&value) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let rounded = value.round() as i32;
    Some(rounded)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipmi_rs::connection::{CompletionErrorCode, NetFn};

    #[test]
    fn parse_celsius_rounds_to_the_nearest_degree() {
        assert_eq!(parse_celsius("49.9 °C"), Some(50));
        assert_eq!(parse_celsius("49.4 °C"), Some(49));
        assert_eq!(parse_celsius("49.00 °C"), Some(49));
        assert_eq!(parse_celsius("108.00 °C"), Some(108));
        assert_eq!(parse_celsius("-0.5 °C"), Some(-1));
        assert_eq!(parse_celsius("-5.00 °C"), Some(-5));
    }

    #[test]
    fn parse_celsius_rejects_other_units_and_garbage() {
        assert_eq!(parse_celsius("49.00 °F"), None);
        assert_eq!(parse_celsius("49.00"), None);
        assert_eq!(parse_celsius("1000.00 °C"), None);
        assert_eq!(parse_celsius("not a reading"), None);
        assert_eq!(parse_celsius(""), None);
    }

    #[test]
    fn fan_commands_encode_to_the_dell_payloads() {
        assert_eq!(auto_payload(true), [0x01, 0x01]);
        assert_eq!(auto_payload(false), [0x01, 0x00]);
        assert_eq!(manual_payload(42), [0x02, 0xff, 42]);

        let request = fan_request(&manual_payload(100));
        assert_eq!(request.netfn_raw(), OEM_NETFN);
        assert_eq!(request.cmd(), OEM_FAN_CMD);
        assert_eq!(request.data(), [0x02, 0xff, 100]);
    }

    #[test]
    fn only_connection_errors_are_retryable() {
        let transport = ipmi_rs::IpmiError::<&str, ()>::Connection("timed out");
        assert!(OpError::from_ipmi("read", &transport).retryable);

        let rejected = ipmi_rs::IpmiError::<&str, ()>::Failed {
            netfn: NetFn::App,
            cmd: 0x3b,
            completion_code: CompletionErrorCode::try_from(0xd4).expect("known completion code"),
            data: Vec::new(),
        };
        assert!(!OpError::from_ipmi("write", &rejected).retryable);

        let parse = ipmi_rs::IpmiError::<&str, &str>::Command {
            error: "bad record",
            netfn: NetFn::Storage,
            cmd: 0x21,
            completion_code: None,
            data: Vec::new(),
        };
        assert!(!OpError::from_ipmi("sdr", &parse).retryable);
    }

    #[test]
    fn sdr_count_validation_handles_unknown_counts() {
        assert!(sdr_count_known(10, false));
        assert!(!sdr_count_known(u16::MAX, false));
        assert!(!sdr_count_known(10, true));

        assert!(sdr_is_complete(10, 10, true));
        assert!(sdr_is_complete(11, 10, true));
        assert!(!sdr_is_complete(9, 10, true));
        assert!(sdr_is_complete(1, 0, false));
        assert!(!sdr_is_complete(0, 0, false));
    }

    #[test]
    fn debug_excerpt_is_bounded() {
        let message = "x".repeat(DEBUG_EXCERPT_CHARS * 2);
        let excerpt = debug_excerpt(&message);
        assert_eq!(excerpt.chars().count(), DEBUG_EXCERPT_CHARS + 1);
        assert!(excerpt.ends_with('…'));
    }

    #[test]
    fn client_satisfies_ports_and_is_send_sync() {
        fn assert_device<T: TempReader + FanCommander + Send + Sync>() {}
        assert_device::<NativeIpmiClient>();
    }

    use std::collections::VecDeque;
    use std::io;

    use ipmi_rs::Ipmi;
    use ipmi_rs::connection::Response;

    /// A connection that replays scripted responses.
    struct Scripted {
        responses: VecDeque<Result<Response, io::Error>>,
    }

    impl Scripted {
        fn new(responses: Vec<Response>) -> Self {
            Self {
                responses: responses.into_iter().map(Ok).collect(),
            }
        }
    }

    impl IpmiConnection for Scripted {
        type SendError = io::Error;
        type RecvError = io::Error;
        type Error = io::Error;

        fn send(&mut self, _request: &mut Request) -> Result<(), Self::SendError> {
            Ok(())
        }

        fn recv(&mut self) -> Result<Response, Self::RecvError> {
            Err(io::Error::other("not scripted"))
        }

        fn send_recv(&mut self, _request: &mut Request) -> Result<Response, Self::Error> {
            self.responses
                .pop_front()
                .unwrap_or_else(|| Err(io::Error::other("script exhausted")))
        }
    }

    fn response(netfn: NetFn, cmd: u8, payload: &[u8]) -> Response {
        let mut data = vec![0u8]; // completion code
        data.extend_from_slice(payload);
        Response::new(Message::new_response(netfn, cmd, data), 0).expect("non-empty response")
    }

    fn repository_info(count: u16, overflow: bool) -> Response {
        let mut payload = vec![0x51, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        payload[1..3].copy_from_slice(&count.to_le_bytes());
        if overflow {
            payload[13] = 0x80;
        }
        response(NetFn::Storage, 0x20, &payload)
    }

    /// An unknown record type: parses, but contributes no sensor.
    fn unknown_record(id: u16, next: u16) -> Response {
        let mut payload = vec![0u8; 9];
        payload[0..2].copy_from_slice(&next.to_le_bytes());
        payload[2..4].copy_from_slice(&id.to_le_bytes());
        payload[4] = 0x51;
        payload[5] = 0x88;
        payload[6] = 2;
        payload[7] = 0xAA;
        payload[8] = 0xBB;
        response(NetFn::Storage, 0x23, &payload)
    }

    /// A full-sensor record with a zero length, which fails to parse but still
    /// carries the next record ID.
    fn malformed_record(id: u16, next: u16) -> Response {
        let mut payload = vec![0u8; 9];
        payload[0..2].copy_from_slice(&next.to_le_bytes());
        payload[2..4].copy_from_slice(&id.to_le_bytes());
        payload[4] = 0x51;
        payload[5] = 0x01; // full sensor record
        payload[6] = 2; // record length, followed by too little data
        payload[7] = 0xAA;
        payload[8] = 0xBB;
        response(NetFn::Storage, 0x23, &payload)
    }

    fn load_error(responses: Vec<Response>) -> String {
        let mut ipmi = Ipmi::new(Scripted::new(responses));
        load_sensors(&mut ipmi)
            .expect_err("no CPU sensors were scripted")
            .message()
            .to_owned()
    }

    #[test]
    fn load_sensors_accepts_a_complete_repository_without_cpu_sensors() {
        let error = load_error(vec![
            repository_info(2, false),
            unknown_record(1, 2),
            unknown_record(2, 0xffff),
        ]);
        assert!(
            error.contains("no 0Eh/0Fh CPU temperature sensors"),
            "{error}"
        );
        assert!(!error.contains("incomplete"), "{error}");
    }

    #[test]
    fn load_sensors_rejects_a_truncated_repository() {
        let error = load_error(vec![
            repository_info(3, false),
            unknown_record(1, 2),
            unknown_record(2, 2),
        ]);
        assert!(
            error.contains("SDR repository incomplete: read 1 of 3"),
            "{error}"
        );
    }

    #[test]
    fn load_sensors_ignores_the_unknown_count_sentinel() {
        let error = load_error(vec![
            repository_info(u16::MAX, true),
            unknown_record(1, 2),
            unknown_record(2, 0xffff),
        ]);
        assert!(
            error.contains("no 0Eh/0Fh CPU temperature sensors"),
            "{error}"
        );
        assert!(!error.contains("incomplete"), "{error}");
    }

    #[test]
    fn load_sensors_detects_a_malformed_record_as_a_short_read() {
        let error = load_error(vec![
            repository_info(2, false),
            malformed_record(1, 2),
            unknown_record(2, 0xffff),
        ]);
        assert!(
            error.contains("SDR repository incomplete: read 1 of 2"),
            "{error}"
        );
    }
}
