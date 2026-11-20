//! IPMI adapter over ipmitool — the only module that touches subprocesses.
//!
//! Structurally satisfies [`TempReader`] and [`FanCommander`]. Redaction is by
//! construction: the password is handed to ipmitool through the `IPMI_PASSWORD`
//! environment variable (`-E`), never on argv; error messages reference the
//! ipmitool subcommand args, never the full argv.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::Duration;

use regex::Regex;
use thiserror::Error;
use wait_timeout::ChildExt;

use crate::domain::ports::{FanCommander, TempReader};
use crate::domain::settings::Settings;
use crate::domain::types::{FanPercent, IpmiError, TempC};

const TIMEOUT: Duration = Duration::from_secs(15);

// The SDR text table reports temperature sensors by hex address; only 0Eh/0Fh
// rows are CPU-relevant. Anchored per line so the capture grabs the full 1-3
// digit temp instead of a greedy two-digit tail (108 -> "08").
static SDR_TEMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*.+?\|\s*0[EF]h\s*\|.*\|\s*(\d{1,3}) degrees C\s*$")
        .expect("valid SDR temperature regex")
});

/// Raw Dell iDRAC OEM IPMI byte sequences: netfn 0x30/0x30 toggles dynamic fan
/// control (0x01 auto on/off) or sets a manual PWM duty cycle (0x02 + full-byte
/// selector). ipmitool treats these as opaque bytes; they must stay byte-exact.
const ENABLE_AUTO_CMD: [&str; 5] = ["raw", "0x30", "0x30", "0x01", "0x01"];
const DISABLE_AUTO_CMD: [&str; 5] = ["raw", "0x30", "0x30", "0x01", "0x00"];
const MANUAL_PCT_CMD_PREFIX: [&str; 5] = ["raw", "0x30", "0x30", "0x02", "0xff"];
const REDACTED: &str = "[redacted]";

/// Captured result of one subprocess invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Process exit status code.
    pub status: i32,
    /// Captured stdout (lossy UTF-8).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8).
    pub stderr: String,
}

/// Failure to run the child at all (as opposed to a non-zero exit status).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandError {
    /// The child exceeded the timeout and was killed.
    #[error("timed out")]
    Timeout,
    /// The executable could not be spawned or waited on.
    #[error("{0}")]
    Spawn(String),
}

/// Runs an argv list with an explicit child environment.
pub trait CommandRunner: Send + Sync {
    /// Execute `argv`; `env` is the complete child environment.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] on timeout or spawn failure.
    fn run(
        &self,
        argv: &[String],
        env: &HashMap<String, String>,
    ) -> Result<CommandOutput, CommandError>;
}

/// Production runner: captured output, UTF-8 lossy, 15-second timeout.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        argv: &[String],
        env: &HashMap<String, String>,
    ) -> Result<CommandOutput, CommandError> {
        run_captured(argv, Some(env), TIMEOUT)
    }
}

/// Shared subprocess helper used by this adapter and the daemon runner.
pub(crate) fn run_captured(
    argv: &[String],
    env: Option<&HashMap<String, String>>,
    timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(CommandError::Spawn("empty argv".to_owned()));
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = env {
        command.env_clear();
        command.envs(env);
    }
    // Keep the child out of our foreground process group so an interactive
    // Ctrl-C reaches breezed (which shuts down gracefully) without SIGINT'ing
    // the in-flight ipmitool and turning a clean stop into an IPMI error.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| CommandError::Spawn(error.to_string()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_handle = stdout.map(drain);
    let stderr_handle = stderr.map(drain);

    let status = child
        .wait_timeout(timeout)
        .map_err(|error| CommandError::Spawn(error.to_string()))?;
    let timed_out = status.is_none();
    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = stdout_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if timed_out {
        return Err(CommandError::Timeout);
    }
    let status = status.expect("status present when not timed out");
    Ok(CommandOutput {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Read a child pipe to EOF on its own thread so the timeout never blocks on a
/// full pipe buffer.
fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

/// Structurally satisfies `TempReader` + `FanCommander` — never inherits them.
pub struct IpmiClient {
    settings: Settings,
    runner: Arc<dyn CommandRunner>,
}

impl IpmiClient {
    /// Build a client with the production subprocess runner.
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            runner: Arc::new(SystemRunner),
        }
    }

    /// Build a client with an injected runner (tests).
    pub fn with_runner(settings: Settings, runner: Arc<dyn CommandRunner>) -> Self {
        Self { settings, runner }
    }

    fn password(&self) -> &str {
        self.settings.password().expose_secret()
    }

    fn invoke(&self, args: &[String]) -> Result<CommandOutput, IpmiError> {
        let mut argv = vec![
            self.settings.ipmitool_path().to_owned(),
            "-I".to_owned(),
            "lanplus".to_owned(),
            "-H".to_owned(),
            self.settings.host().to_owned(),
            "-U".to_owned(),
            self.settings.user().to_owned(),
            "-E".to_owned(),
        ];
        argv.extend_from_slice(args);
        // Only ipmitool needs the secret. Do not forward IDRAC_PASSWORD, which
        // the daemon inherits from its EnvironmentFile.
        let mut env: HashMap<String, String> = std::env::vars()
            .filter(|(key, _)| key != "IDRAC_PASSWORD")
            .collect();
        env.insert("IPMI_PASSWORD".to_owned(), self.password().to_owned());
        match self.runner.run(&argv, &env) {
            Ok(output) => Ok(output),
            Err(CommandError::Timeout) => Err(IpmiError::new(format!(
                "ipmitool {} timed out after {}s",
                args.join(" "),
                TIMEOUT.as_secs()
            ))),
            Err(CommandError::Spawn(error)) => Err(IpmiError::new(format!(
                "ipmitool {} failed to run: {error}",
                args.join(" ")
            ))),
        }
    }

    fn raise_failed(&self, args: &[String], output: &CommandOutput) -> IpmiError {
        let mut stderr = output.stderr.clone();
        let password = self.password();
        if !password.is_empty() {
            stderr = stderr.replace(password, REDACTED);
        }
        let first_line = stderr.trim().lines().next().unwrap_or_default().to_owned();
        let first_line: String = first_line.chars().take(200).collect();
        IpmiError::new(format!(
            "ipmitool {} failed (rc={}): {first_line}",
            args.join(" "),
            output.status
        ))
    }

    /// Query subcommands must produce parseable output; empty stdout is a failure.
    fn run_read(&self, args: &[String]) -> Result<String, IpmiError> {
        let output = self.invoke(args)?;
        if output.status != 0 {
            return Err(self.raise_failed(args, &output));
        }
        if output.stdout.trim().is_empty() {
            return Err(IpmiError::new(format!(
                "ipmitool {}: empty output",
                args.join(" ")
            )));
        }
        Ok(output.stdout)
    }

    /// Raw write subcommands report via exit code; empty stdout is success.
    fn run_write(&self, args: &[String]) -> Result<(), IpmiError> {
        let output = self.invoke(args)?;
        if output.status != 0 {
            return Err(self.raise_failed(args, &output));
        }
        Ok(())
    }

    /// Parse every fan RPM row from `ipmitool sdr type fan`.
    ///
    /// # Errors
    ///
    /// Returns [`IpmiError`] when the command fails or produces no output.
    pub fn read_fan_rpms(&self) -> Result<Vec<(String, u32)>, IpmiError> {
        let output = self.run_read(&["sdr".to_owned(), "type".to_owned(), "fan".to_owned()])?;
        let mut rpms = Vec::new();
        for line in output.lines() {
            let fields: Vec<&str> = line.split('|').map(str::trim).collect();
            if fields.len() < 2 || fields[0].is_empty() {
                continue;
            }
            let last = fields[fields.len() - 1];
            let Some(digits) = last.strip_suffix(" RPM") else {
                continue;
            };
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            if let Ok(rpm) = digits.parse::<u32>() {
                rpms.push((fields[0].to_owned(), rpm));
            }
        }
        Ok(rpms)
    }
}

impl TempReader for IpmiClient {
    fn read_max_cpu_temp(&self) -> Result<TempC, IpmiError> {
        let output = self.run_read(&[
            "sdr".to_owned(),
            "type".to_owned(),
            "temperature".to_owned(),
        ])?;
        let temps: Vec<i32> = SDR_TEMP_RE
            .captures_iter(&output)
            .filter_map(|captures| captures.get(1))
            .filter_map(|value| value.as_str().parse().ok())
            .collect();
        let Some(max) = temps.into_iter().max() else {
            return Err(IpmiError::new(
                "ipmitool sdr type temperature: no 0Eh/0Fh temperature rows found",
            ));
        };
        Ok(TempC(max))
    }
}

impl FanCommander for IpmiClient {
    fn enable_auto(&self) -> Result<(), IpmiError> {
        self.run_write(&ENABLE_AUTO_CMD.map(str::to_owned))
    }

    fn disable_auto(&self) -> Result<(), IpmiError> {
        self.run_write(&DISABLE_AUTO_CMD.map(str::to_owned))
    }

    fn set_manual_pct(&self, pct: FanPercent) -> Result<(), IpmiError> {
        let mut args = MANUAL_PCT_CMD_PREFIX.map(str::to_owned).to_vec();
        args.push(format!("{:#04x}", pct.get()));
        self.run_write(&args)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::*;
    use crate::domain::settings::SettingsInput;
    use crate::domain::types::make_fan_pct;

    const PASSWORD: &str = "hunter2-secret";

    fn make_settings() -> Settings {
        Settings::from_input(SettingsInput {
            password: Some(PASSWORD.to_owned()),
            ..SettingsInput::with_identity("169.254.0.1", "root")
        })
        .unwrap()
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    enum Response {
        Ok(CommandOutput),
        Timeout,
        Spawn(String),
    }

    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
        envs: Mutex<Vec<HashMap<String, String>>>,
        responses: Mutex<Vec<Response>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Response>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                envs: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        fn envs(&self) -> Vec<HashMap<String, String>> {
            self.envs.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            argv: &[String],
            env: &HashMap<String, String>,
        ) -> Result<CommandOutput, CommandError> {
            self.calls.lock().unwrap().push(argv.to_vec());
            self.envs.lock().unwrap().push(env.clone());
            match self.responses.lock().unwrap().remove(0) {
                Response::Ok(output) => Ok(output),
                Response::Timeout => Err(CommandError::Timeout),
                Response::Spawn(error) => Err(CommandError::Spawn(error)),
            }
        }
    }

    fn ok(stdout: &str) -> Response {
        Response::Ok(CommandOutput {
            status: 0,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    fn failed(status: i32, stderr: &str) -> Response {
        Response::Ok(CommandOutput {
            status,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        })
    }

    fn make_client(runner: Arc<FakeRunner>) -> IpmiClient {
        IpmiClient::with_runner(make_settings(), runner)
    }

    #[test]
    fn parses_sdr_fixture_to_max_cpu_temp() {
        let runner = FakeRunner::new(vec![ok(&std::fs::read_to_string(fixture(
            "sdr_temperature.txt",
        ))
        .unwrap())]);
        assert_eq!(make_client(runner).read_max_cpu_temp(), Ok(TempC(108)));
    }

    #[test]
    fn read_fan_rpms_parses_sdr_fixture() {
        let runner = FakeRunner::new(vec![ok(
            &std::fs::read_to_string(fixture("sdr_fan.txt")).unwrap()
        )]);
        assert_eq!(
            make_client(runner).read_fan_rpms(),
            Ok(vec![
                ("FAN_1".to_owned(), 4320),
                ("FAN_2".to_owned(), 4320),
                ("FAN_3".to_owned(), 4080),
                ("FAN_4".to_owned(), 0),
            ])
        );
    }

    #[test]
    fn unparseable_nonempty_sdr_raises() {
        let runner = FakeRunner::new(vec![ok("sensor gibberish without any addresses\n")]);
        assert!(make_client(runner).read_max_cpu_temp().is_err());
    }

    #[test]
    fn empty_output_with_zero_rc_raises_ipmi_error() {
        let runner = FakeRunner::new(vec![ok("")]);
        let error = make_client(runner).read_max_cpu_temp().unwrap_err();
        assert!(error.to_string().contains("empty output"));
    }

    #[test]
    fn nonzero_exit_code_raises_ipmi_error_with_stderr_snippet() {
        let runner = FakeRunner::new(vec![failed(1, "unable to establish IPMI v2 connection\n")]);
        let error = make_client(runner).read_max_cpu_temp().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("rc=1"));
        assert!(message.contains("unable to establish IPMI v2 connection"));
        assert!(!message.contains(PASSWORD));
    }

    #[test]
    fn raw_write_commands_succeed_on_empty_stdout_with_zero_rc() {
        let runner = FakeRunner::new(vec![ok(""), ok(""), ok("")]);
        let client = make_client(runner);
        client.enable_auto().unwrap();
        client.disable_auto().unwrap();
        client.set_manual_pct(make_fan_pct(50).unwrap()).unwrap();
    }

    #[test]
    fn raw_write_nonzero_rc_with_empty_stdout_still_raises() {
        let runner = FakeRunner::new(vec![failed(1, "")]);
        let error = make_client(runner).disable_auto().unwrap_err();
        assert!(error.to_string().contains("rc=1"));
    }

    #[test]
    fn commands_emit_exact_argv() {
        let runner = FakeRunner::new(vec![ok("ok\n"), ok("ok\n"), ok("ok\n"), ok("ok\n")]);
        let client = make_client(runner.clone());
        client.enable_auto().unwrap();
        client.disable_auto().unwrap();
        client.set_manual_pct(make_fan_pct(100).unwrap()).unwrap();
        client.set_manual_pct(make_fan_pct(10).unwrap()).unwrap();

        let argv = |tail: &[&str]| {
            [
                "/usr/bin/ipmitool",
                "-I",
                "lanplus",
                "-H",
                "169.254.0.1",
                "-U",
                "root",
                "-E",
            ]
            .iter()
            .map(ToString::to_string)
            .chain(tail.iter().map(|part| (*part).to_owned()))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            runner.calls(),
            [
                argv(&["raw", "0x30", "0x30", "0x01", "0x01"]),
                argv(&["raw", "0x30", "0x30", "0x01", "0x00"]),
                argv(&["raw", "0x30", "0x30", "0x02", "0xff", "0x64"]),
                argv(&["raw", "0x30", "0x30", "0x02", "0xff", "0x0a"]),
            ]
        );
    }

    #[test]
    fn password_is_passed_in_child_environment_not_argv() {
        let runner = FakeRunner::new(vec![ok("ok\n")]);
        make_client(runner.clone()).enable_auto().unwrap();
        let argv = &runner.calls()[0];
        let env = &runner.envs()[0];
        assert_eq!(env.get("IPMI_PASSWORD").map(String::as_str), Some(PASSWORD));
        assert!(!env.contains_key("IDRAC_PASSWORD"));
        assert!(!argv.contains(&"-P".to_owned()));
        assert!(argv.iter().all(|part| !part.contains(PASSWORD)));
    }

    #[test]
    fn spawn_failure_is_wrapped_without_secret() {
        let runner = FakeRunner::new(vec![Response::Spawn(
            "No such file or directory (os error 2)".to_owned(),
        )]);
        let error = make_client(runner).read_max_cpu_temp().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("failed to run"));
        assert!(!message.contains(PASSWORD));
    }

    #[test]
    fn timeout_suppresses_argv_bearing_cause() {
        let runner = FakeRunner::new(vec![Response::Timeout]);
        let error = make_client(runner).read_max_cpu_temp().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("timed out after 15s"));
        assert!(!message.contains(PASSWORD));
        assert!(!message.contains("-P"));
    }

    #[test]
    fn password_never_appears_in_raised_messages_even_when_stderr_echoes_it() {
        let runner = FakeRunner::new(vec![failed(
            1,
            &format!("-P {PASSWORD} rejected by host 169.254.0.1\nsecond line {PASSWORD}\n"),
        )]);
        let error = make_client(runner).enable_auto().unwrap_err();
        assert!(!error.to_string().contains(PASSWORD));
        assert!(!format!("{error:?}").contains(PASSWORD));
    }

    #[test]
    fn client_satisfies_ports_structurally() {
        fn assert_ports<T: TempReader + FanCommander>() {}
        assert_ports::<IpmiClient>();
    }
}
