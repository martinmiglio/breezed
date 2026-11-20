//! Systemd deployment planning and execution.
//!
//! `daemon install` plans a fixed, code-reviewed sequence and re-executes itself
//! once via sudo. The privileged path (`Deployment::install` / `remove`) runs
//! only argv-list commands — no shell — and stages its own copies of the unit,
//! env skeleton, and example config in a root-private temp directory, so no
//! unprivileged staging input is trusted.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use regex::Regex;
use thiserror::Error;

use crate::VERSION;
use crate::adapters::ipmi::{CommandError, run_captured};

/// systemd unit name.
pub const SERVICE_NAME: &str = "breezed";
/// Non-login system user that owns the service.
pub const SERVICE_USER: &str = "breezed";
/// Installed binary path referenced by the unit.
pub const EXEC_PATH: &str = "/usr/local/bin/breezed";

static UNIT_STAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^# Installed by breezed (\S+) on ").expect("valid unit stamp regex")
});

const ENV_SKELETON: &str = "\
# breezed environment file — holds the iDRAC secrets.
# The installer never overwrites this file; edit values in place.
IDRAC_HOST=
IDRAC_USER=
IDRAC_PASSWORD=
";

const UNIT_TEMPLATE: &str = include_str!("../../resources/breezed.service.template");
const CONFIG_TEMPLATE: &str = include_str!("../../resources/breezed.toml.example");

/// Deployment failures with actionable messages; never raw `OSError` text.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct DaemonError {
    message: String,
}

impl DaemonError {
    /// Build a deployment error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Result of a single privileged step; `Done` means the step changed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step ran and changed state.
    Done,
    /// The step was unnecessary because state already matched.
    Skipped,
}

/// Filesystem paths touched by the installer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerPaths {
    /// systemd unit path.
    pub unit_path: PathBuf,
    /// environment file path.
    pub env_path: PathBuf,
    /// directory holding the config file.
    pub config_dir: PathBuf,
}

impl Default for InstallerPaths {
    fn default() -> Self {
        Self {
            unit_path: PathBuf::from("/etc/systemd/system/breezed.service"),
            env_path: PathBuf::from("/etc/breezed.env"),
            config_dir: PathBuf::from("/etc/breezed"),
        }
    }
}

impl InstallerPaths {
    /// The installed config file path.
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("breezed.toml")
    }
}

/// One fixed install step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAction {
    /// Create the `breezed` system group.
    EnsureGroup,
    /// Create the non-login `breezed` user.
    EnsureUser,
    /// Copy the running binary to `/usr/local/bin/breezed`.
    InstallBinary,
    /// Install the systemd unit.
    InstallUnit,
    /// Install the example config, first run only.
    InstallConfig,
    /// Install the env skeleton, first run only.
    InstallEnv,
    /// `systemctl daemon-reload`.
    DaemonReload,
    /// `systemctl enable breezed`.
    Enable,
    /// `systemctl restart breezed`.
    Restart,
}

impl InstallAction {
    /// Human label for plans and results.
    pub fn label(self, paths: &InstallerPaths) -> String {
        match self {
            Self::EnsureGroup => "ensure system group 'breezed'".to_owned(),
            Self::EnsureUser => "ensure system user 'breezed'".to_owned(),
            Self::InstallBinary => format!("install {EXEC_PATH}"),
            Self::InstallUnit => format!("install {}", paths.unit_path.display()),
            Self::InstallConfig => {
                format!("install {} (first run only)", paths.config_path().display())
            }
            Self::InstallEnv => {
                format!("install {} (first run only)", paths.env_path.display())
            }
            Self::DaemonReload => "systemctl daemon-reload".to_owned(),
            Self::Enable => "systemctl enable breezed".to_owned(),
            Self::Restart => "systemctl restart breezed".to_owned(),
        }
    }
}

/// One fixed uninstall step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveAction {
    /// Stop and disable the service.
    DisableNow,
    /// Remove the unit file.
    RemoveUnit,
    /// Remove the installed binary.
    RemoveBinary,
    /// `systemctl daemon-reload`.
    DaemonReload,
}

impl RemoveAction {
    /// Human label for plans and results.
    pub fn label(self, paths: &InstallerPaths) -> String {
        match self {
            Self::DisableNow => "systemctl disable --now breezed".to_owned(),
            Self::RemoveUnit => format!("remove {}", paths.unit_path.display()),
            Self::RemoveBinary => format!("remove {EXEC_PATH}"),
            Self::DaemonReload => "systemctl daemon-reload".to_owned(),
        }
    }
}

/// The fixed install sequence.
pub const INSTALL_ACTIONS: [InstallAction; 9] = [
    InstallAction::EnsureGroup,
    InstallAction::EnsureUser,
    InstallAction::InstallBinary,
    InstallAction::InstallUnit,
    InstallAction::InstallConfig,
    InstallAction::InstallEnv,
    InstallAction::DaemonReload,
    InstallAction::Enable,
    InstallAction::Restart,
];

/// The fixed uninstall sequence; `/etc/breezed.env` and `/etc/breezed/` are kept.
pub const REMOVE_ACTIONS: [RemoveAction; 4] = [
    RemoveAction::DisableNow,
    RemoveAction::RemoveUnit,
    RemoveAction::RemoveBinary,
    RemoveAction::DaemonReload,
];

/// Read-only filesystem seam used by `status` and idempotence checks.
pub trait FileOps: Send + Sync {
    /// Read a file as UTF-8.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error.
    fn read_text(&self, path: &Path) -> io::Result<String>;

    /// Presence check; `Some(())` when the path exists.
    fn stat(&self, path: &Path) -> Option<()>;
}

/// Production filesystem operations.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFileOps;

impl FileOps for RealFileOps {
    fn read_text(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn stat(&self, path: &Path) -> Option<()> {
        fs::metadata(path).ok().map(|_| ())
    }
}

/// Runs an argv list and returns stdout.
pub trait CommandRunner: Send + Sync {
    /// Execute `argv` with the inherited environment.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] on spawn failure, timeout, or non-zero status.
    fn run(&self, argv: &[String]) -> Result<String, DaemonError>;
}

/// Production command runner with a 600-second timeout.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, argv: &[String]) -> Result<String, DaemonError> {
        let command = argv.join(" ");
        let output =
            run_captured(argv, None, Duration::from_secs(600)).map_err(|error| match error {
                CommandError::Timeout => {
                    DaemonError::new(format!("{command} failed to run: timed out after 600s"))
                }
                CommandError::Spawn(message) => {
                    DaemonError::new(format!("{command} failed to run: {message}"))
                }
            })?;
        if output.status != 0 {
            return Err(DaemonError::new(format!(
                "{command} failed (rc={}): {}",
                output.status,
                first_line(&output.stderr)
            )));
        }
        Ok(output.stdout)
    }
}

/// Service-user lookup seam.
pub trait UserDb: Send + Sync {
    /// Whether the named system user exists.
    fn user_exists(&self, name: &str) -> bool;

    /// Whether the named system group exists.
    fn group_exists(&self, name: &str) -> bool;

    /// The user's primary group name, when it can be resolved.
    fn primary_group(&self, name: &str) -> Option<String>;
}

/// Production lookup: local files first, then NSS (`getent`) for LDAP/SSSD users.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemUserDb;

impl UserDb for SystemUserDb {
    fn user_exists(&self, name: &str) -> bool {
        passwd_entry(name).is_some()
    }

    fn group_exists(&self, name: &str) -> bool {
        group_entry(name).is_some()
    }

    fn primary_group(&self, name: &str) -> Option<String> {
        let gid = passwd_entry(name)?.split(':').nth(3)?.to_owned();
        group_name_for_gid(&gid)
    }
}

fn passwd_entry(name: &str) -> Option<String> {
    local_entry("/etc/passwd", name).or_else(|| nss_entry("passwd", name))
}

fn group_entry(name: &str) -> Option<String> {
    local_entry("/etc/group", name).or_else(|| nss_entry("group", name))
}

fn group_name_for_gid(gid: &str) -> Option<String> {
    let local = fs::read_to_string("/etc/group").ok().and_then(|groups| {
        groups
            .lines()
            .find(|line| line.split(':').nth(2) == Some(gid))
            .and_then(|line| line.split(':').next())
            .map(str::to_owned)
    });
    local.or_else(|| {
        nss_entry("group", gid).and_then(|entry| entry.split(':').next().map(str::to_owned))
    })
}

fn local_entry(path: &str, name: &str) -> Option<String> {
    let data = fs::read_to_string(path).ok()?;
    data.lines()
        .find(|line| line.split(':').next() == Some(name))
        .map(str::to_owned)
}

/// NSS fallback for LDAP/SSSD users; `getent` is part of glibc.
fn nss_entry(database: &str, key: &str) -> Option<String> {
    let output = std::process::Command::new("getent")
        .args([database, key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .next()
        .map(str::to_owned)
}

/// Status reported by `daemon status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    /// Whether the unit file exists.
    pub unit_present: bool,
    /// `systemctl is-active` reported active.
    pub active: bool,
    /// `systemctl is-enabled` reported enabled.
    pub enabled: bool,
    /// Version stamped in the unit, when present.
    pub unit_version: Option<String>,
    /// Version of the running binary.
    pub binary_version: String,
}

fn first_line(text: &str) -> String {
    text.trim()
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

fn probe_flag(runner: &dyn CommandRunner, argv: &[String], ok_values: &[&str]) -> bool {
    match runner.run(argv) {
        Ok(output) => ok_values.contains(&output.trim()),
        Err(_) => false,
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// Render the unit with the installer stamp so `daemon status` can detect drift.
pub fn render_unit(version: &str, timestamp: &str) -> String {
    format!("# Installed by breezed {version} on {timestamp}\n{UNIT_TEMPLATE}")
}

fn unit_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Collect the daemon deployment report without requiring privileges.
pub fn daemon_status(
    paths: &InstallerPaths,
    runner: &dyn CommandRunner,
    fs: &dyn FileOps,
) -> DaemonStatus {
    let unit_present = fs.stat(&paths.unit_path).is_some();
    let mut unit_version = None;
    if unit_present {
        let unit_text = fs.read_text(&paths.unit_path).unwrap_or_default();
        if let Some(captures) = UNIT_STAMP_RE.captures(&unit_text) {
            unit_version = captures.get(1).map(|value| value.as_str().to_owned());
        }
    }
    DaemonStatus {
        unit_present,
        active: probe_flag(
            runner,
            &strings(&["systemctl", "is-active", SERVICE_NAME]),
            &["active"],
        ),
        enabled: probe_flag(
            runner,
            &strings(&["systemctl", "is-enabled", SERVICE_NAME]),
            &["enabled", "enabled-runtime"],
        ),
        unit_version,
        binary_version: VERSION.to_owned(),
    }
}

/// Fixed install/remove executor. Holds the injected seams and target paths.
#[derive(Clone)]
pub struct Deployment {
    paths: InstallerPaths,
    runner: Arc<dyn CommandRunner>,
    fs: Arc<dyn FileOps>,
    users: Arc<dyn UserDb>,
}

impl Deployment {
    /// Build a deployment context.
    pub fn new(
        paths: InstallerPaths,
        runner: Arc<dyn CommandRunner>,
        fs: Arc<dyn FileOps>,
        users: Arc<dyn UserDb>,
    ) -> Self {
        Self {
            paths,
            runner,
            fs,
            users,
        }
    }

    /// Execute the fixed install sequence from `source_exe`.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] for an invalid source binary, staging failure, or
    /// failing privileged step.
    pub fn install(
        &self,
        source_exe: &Path,
        owner: &str,
    ) -> Result<Vec<(String, StepOutcome)>, DaemonError> {
        ensure_runnable_binary(source_exe)?;
        if !self.users.user_exists(owner) {
            return Err(DaemonError::new(format!(
                "config owner '{owner}' does not exist"
            )));
        }
        let unit_group = self.users.primary_group(owner).ok_or_else(|| {
            DaemonError::new(format!(
                "cannot resolve the primary group for config owner '{owner}'"
            ))
        })?;
        let staging = Self::stage()?;
        let mut results = Vec::with_capacity(INSTALL_ACTIONS.len());
        for action in INSTALL_ACTIONS {
            let outcome = self.execute_install(action, source_exe, owner, &unit_group, &staging)?;
            results.push((action.label(&self.paths), outcome));
        }
        Ok(results)
    }

    /// Execute the fixed uninstall sequence.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] when a privileged step fails.
    pub fn remove(&self) -> Result<Vec<(String, StepOutcome)>, DaemonError> {
        let mut results = Vec::with_capacity(REMOVE_ACTIONS.len());
        for action in REMOVE_ACTIONS {
            let outcome = self.execute_remove(action)?;
            results.push((action.label(&self.paths), outcome));
        }
        Ok(results)
    }

    fn stage() -> Result<tempfile::TempDir, DaemonError> {
        let staging = tempfile::Builder::new()
            .prefix("breezed-install-")
            .tempdir()
            .map_err(|error| {
                DaemonError::new(format!("cannot create staging directory: {error}"))
            })?;
        let files = [
            ("breezed.service", render_unit(VERSION, &unit_timestamp())),
            ("breezed.toml", CONFIG_TEMPLATE.to_owned()),
            ("breezed.env", ENV_SKELETON.to_owned()),
        ];
        for (name, content) in files {
            let path = staging.path().join(name);
            fs::write(&path, content).map_err(|error| {
                DaemonError::new(format!(
                    "cannot write staged files into {}: {error}",
                    staging.path().display()
                ))
            })?;
        }
        Ok(staging)
    }

    fn execute_install(
        &self,
        action: InstallAction,
        source_exe: &Path,
        owner: &str,
        unit_group: &str,
        staging: &tempfile::TempDir,
    ) -> Result<StepOutcome, DaemonError> {
        match action {
            InstallAction::EnsureGroup => {
                if self.users.group_exists(SERVICE_USER) {
                    return Ok(StepOutcome::Skipped);
                }
                self.run(&["groupadd", "--system", SERVICE_USER])?;
                Ok(StepOutcome::Done)
            }
            InstallAction::EnsureUser => {
                if self.users.user_exists(SERVICE_USER) {
                    return Ok(StepOutcome::Skipped);
                }
                self.run(&[
                    "useradd",
                    "--system",
                    "--gid",
                    SERVICE_USER,
                    "--no-create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    SERVICE_USER,
                ])?;
                Ok(StepOutcome::Done)
            }
            InstallAction::InstallBinary => {
                let destination = Path::new(EXEC_PATH);
                if same_file(source_exe, destination) {
                    return Ok(StepOutcome::Skipped);
                }
                self.run(&[
                    "install",
                    "-D",
                    "-o",
                    "root",
                    "-g",
                    "root",
                    "-m",
                    "0755",
                    &source_exe.to_string_lossy(),
                    EXEC_PATH,
                ])?;
                Ok(StepOutcome::Done)
            }
            InstallAction::InstallUnit => {
                self.install_file(
                    &staging.path().join("breezed.service"),
                    &self.paths.unit_path,
                    "root",
                    "root",
                    "0644",
                )?;
                Ok(StepOutcome::Done)
            }
            InstallAction::InstallConfig => {
                let config_path = self.paths.config_path();
                if self.fs.stat(&config_path).is_some() {
                    return Ok(StepOutcome::Skipped);
                }
                self.install_file(
                    &staging.path().join("breezed.toml"),
                    &config_path,
                    owner,
                    unit_group,
                    "0664",
                )?;
                Ok(StepOutcome::Done)
            }
            InstallAction::InstallEnv => {
                if self.fs.stat(&self.paths.env_path).is_some() {
                    return Ok(StepOutcome::Skipped);
                }
                self.install_file(
                    &staging.path().join("breezed.env"),
                    &self.paths.env_path,
                    "root",
                    SERVICE_USER,
                    "0640",
                )?;
                Ok(StepOutcome::Done)
            }
            InstallAction::DaemonReload => {
                self.run(&["systemctl", "daemon-reload"])?;
                Ok(StepOutcome::Done)
            }
            InstallAction::Enable => {
                self.run(&["systemctl", "enable", SERVICE_NAME])?;
                Ok(StepOutcome::Done)
            }
            InstallAction::Restart => {
                // Unconditional restart: an upgrade must load the new binary.
                self.run(&["systemctl", "restart", SERVICE_NAME])?;
                Ok(StepOutcome::Done)
            }
        }
    }

    fn execute_remove(&self, action: RemoveAction) -> Result<StepOutcome, DaemonError> {
        match action {
            RemoveAction::DisableNow => {
                if self.fs.stat(&self.paths.unit_path).is_none() {
                    return Ok(StepOutcome::Skipped);
                }
                self.run(&["systemctl", "disable", "--now", SERVICE_NAME])?;
                Ok(StepOutcome::Done)
            }
            RemoveAction::RemoveUnit => {
                self.run(&["rm", "-f", &self.paths.unit_path.to_string_lossy()])?;
                Ok(StepOutcome::Done)
            }
            RemoveAction::RemoveBinary => {
                self.run(&["rm", "-f", EXEC_PATH])?;
                Ok(StepOutcome::Done)
            }
            RemoveAction::DaemonReload => {
                self.run(&["systemctl", "daemon-reload"])?;
                Ok(StepOutcome::Done)
            }
        }
    }

    fn install_file(
        &self,
        source: &Path,
        destination: &Path,
        owner: &str,
        group: &str,
        mode: &str,
    ) -> Result<(), DaemonError> {
        self.run(&[
            "install",
            "-D",
            "-o",
            owner,
            "-g",
            group,
            "-m",
            mode,
            &source.to_string_lossy(),
            &destination.to_string_lossy(),
        ])?;
        Ok(())
    }

    fn run(&self, argv: &[&str]) -> Result<String, DaemonError> {
        self.runner.run(&strings(argv))
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .ok()
        .zip(fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
}

fn ensure_runnable_binary(path: &Path) -> Result<(), DaemonError> {
    let metadata = fs::metadata(path).map_err(|error| {
        DaemonError::new(format!("cannot read binary {}: {error}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(DaemonError::new(format!(
            "binary {} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(DaemonError::new(format!(
                "binary {} is not executable",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Execute the install sequence as root.
///
/// # Errors
///
/// Returns [`DaemonError`] when not root or when a step fails.
pub fn apply(
    source_exe: &Path,
    owner: &str,
    paths: &InstallerPaths,
    runner: Arc<dyn CommandRunner>,
    fs: Arc<dyn FileOps>,
    users: Arc<dyn UserDb>,
) -> Result<Vec<(String, StepOutcome)>, DaemonError> {
    require_root()?;
    Deployment::new(paths.clone(), runner, fs, users).install(source_exe, owner)
}

/// Execute the uninstall sequence as root.
///
/// # Errors
///
/// Returns [`DaemonError`] when not root or when a step fails.
pub fn remove(
    paths: &InstallerPaths,
    runner: Arc<dyn CommandRunner>,
    fs: Arc<dyn FileOps>,
    users: Arc<dyn UserDb>,
) -> Result<Vec<(String, StepOutcome)>, DaemonError> {
    require_root()?;
    Deployment::new(paths.clone(), runner, fs, users).remove()
}

fn require_root() -> Result<(), DaemonError> {
    if euid() != 0 {
        return Err(DaemonError::new(
            "apply/remove must run as root; they are invoked via sudo by `breezed daemon install`",
        ));
    }
    Ok(())
}

/// Effective UID, read from `/proc/self/status` (Linux-only project).
pub fn euid() -> u32 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return u32::MAX;
    };
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("Uid:")?
                .split_whitespace()
                .nth(2)?
                .parse()
                .ok()
        })
        .unwrap_or(u32::MAX)
}

/// Login name of the invoking user. Prefers `SUDO_USER` so a direct
/// `sudo breezed daemon install` still assigns the config to the real caller.
pub fn getuser() -> String {
    let non_empty = |key: &str| env::var(key).ok().filter(|value| !value.is_empty());
    if let Some(value) = non_empty("SUDO_USER") {
        return value;
    }
    if let Some(value) = ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .into_iter()
        .find_map(non_empty)
    {
        return value;
    }
    let uid = euid();
    if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() > 2 && fields[2].parse::<u32>() == Ok(uid) {
                return fields[0].to_owned();
            }
        }
    }
    "unknown".to_owned()
}

/// Resolve a program on `PATH`, returning its absolute path.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if let Ok(metadata) = fs::metadata(&candidate)
            && metadata.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    return Some(candidate);
                }
            }
            #[cfg(not(unix))]
            {
                return Some(candidate);
            }
        }
    }
    None
}

/// Absolute path of the running executable (used for the sudo re-exec).
pub fn resolve_self() -> PathBuf {
    env::current_exe().unwrap_or_else(|_| PathBuf::from(EXEC_PATH))
}

/// Labels for the install plan.
pub fn install_labels(paths: &InstallerPaths) -> Vec<String> {
    INSTALL_ACTIONS
        .iter()
        .map(|action| action.label(paths))
        .collect()
}

/// Labels for the uninstall plan.
pub fn remove_labels(paths: &InstallerPaths) -> Vec<String> {
    REMOVE_ACTIONS
        .iter()
        .map(|action| action.label(paths))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    struct FakeFileOps {
        files: HashMap<PathBuf, String>,
    }

    impl FakeFileOps {
        fn new(files: &[(&Path, &str)]) -> Self {
            Self {
                files: files
                    .iter()
                    .map(|(path, body)| (path.to_path_buf(), (*body).to_owned()))
                    .collect(),
            }
        }
    }

    impl FileOps for FakeFileOps {
        fn read_text(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn stat(&self, path: &Path) -> Option<()> {
            self.files.get(path).map(|_| ())
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        argvs: Mutex<Vec<Vec<String>>>,
        outputs: HashMap<String, String>,
        errors: HashMap<String, String>,
    }

    impl FakeRunner {
        fn with(outputs: &[(&str, &str)], errors: &[(&str, &str)]) -> Arc<Self> {
            Arc::new(Self {
                argvs: Mutex::new(Vec::new()),
                outputs: outputs
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
                errors: errors
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.argvs.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, argv: &[String]) -> Result<String, DaemonError> {
            self.argvs.lock().unwrap().push(argv.to_vec());
            let key = argv.join(" ");
            if let Some(error) = self.errors.get(&key) {
                return Err(DaemonError::new(error.clone()));
            }
            Ok(self.outputs.get(&key).cloned().unwrap_or_default())
        }
    }

    struct FakeUsers {
        service_user: bool,
        service_group: bool,
        owner: bool,
        owner_group: Option<&'static str>,
    }

    impl FakeUsers {
        fn new(service_user: bool) -> Self {
            Self {
                service_user,
                service_group: service_user,
                owner: true,
                owner_group: Some("martin"),
            }
        }
    }

    impl UserDb for FakeUsers {
        fn user_exists(&self, name: &str) -> bool {
            if name == SERVICE_USER {
                self.service_user
            } else {
                self.owner
            }
        }

        fn group_exists(&self, name: &str) -> bool {
            name == SERVICE_USER && self.service_group
        }

        fn primary_group(&self, _name: &str) -> Option<String> {
            self.owner_group.map(str::to_owned)
        }
    }

    fn paths(dir: &Path) -> InstallerPaths {
        InstallerPaths {
            unit_path: dir.join("breezed.service"),
            env_path: dir.join("breezed.env"),
            config_dir: dir.join("breezed"),
        }
    }

    fn executable_source(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let source = dir.join("breezed");
        fs::write(&source, b"binary").unwrap();
        let mut permissions = fs::metadata(&source).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&source, permissions).unwrap();
        source
    }

    fn deployment(
        dir: &Path,
        runner: Arc<FakeRunner>,
        fs: FakeFileOps,
        exists: bool,
    ) -> Deployment {
        Deployment::new(
            paths(dir),
            runner,
            Arc::new(fs),
            Arc::new(FakeUsers::new(exists)),
        )
    }

    const STAMPED_UNIT: &str = "\
# Installed by breezed 9.9.9 on 2026-01-01T00:00:00Z
[Service]
ExecStart=/usr/local/bin/breezed run
";

    #[test]
    fn status_reports_version_drift_when_stamped_unit_differs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let fs = FakeFileOps::new(&[(&paths.unit_path, STAMPED_UNIT)]);
        let runner = FakeRunner::with(
            &[
                ("systemctl is-active breezed", "active\n"),
                ("systemctl is-enabled breezed", "enabled\n"),
            ],
            &[],
        );
        let status = daemon_status(&paths, &*runner, &fs);
        assert!(status.unit_present);
        assert!(status.active);
        assert!(status.enabled);
        assert_eq!(status.unit_version.as_deref(), Some("9.9.9"));
        assert_eq!(status.binary_version, VERSION);
    }

    #[test]
    fn status_absent_unit_reports_all_clear() {
        let dir = tempfile::tempdir().unwrap();
        let status = daemon_status(
            &paths(dir.path()),
            &*FakeRunner::with(&[], &[]),
            &FakeFileOps::new(&[]),
        );
        assert_eq!(
            status,
            DaemonStatus {
                unit_present: false,
                active: false,
                enabled: false,
                unit_version: None,
                binary_version: VERSION.to_owned(),
            }
        );
    }

    #[test]
    fn status_probe_failure_reports_inactive() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let fs = FakeFileOps::new(&[(&paths.unit_path, STAMPED_UNIT)]);
        let runner = FakeRunner::with(&[], &[("systemctl is-active breezed", "failed")]);
        assert!(!daemon_status(&paths, &*runner, &fs).active);
    }

    #[test]
    fn install_actions_are_stable_and_complete() {
        let paths = InstallerPaths::default();
        assert_eq!(
            install_labels(&paths),
            [
                "ensure system group 'breezed'",
                "ensure system user 'breezed'",
                "install /usr/local/bin/breezed",
                "install /etc/systemd/system/breezed.service",
                "install /etc/breezed/breezed.toml (first run only)",
                "install /etc/breezed.env (first run only)",
                "systemctl daemon-reload",
                "systemctl enable breezed",
                "systemctl restart breezed",
            ]
        );
        assert_eq!(
            remove_labels(&paths),
            [
                "systemctl disable --now breezed",
                "remove /etc/systemd/system/breezed.service",
                "remove /usr/local/bin/breezed",
                "systemctl daemon-reload",
            ]
        );
    }

    #[test]
    fn rendered_unit_is_stamped_and_hardened() {
        let unit = render_unit(VERSION, "2026-01-01T00:00:00Z");
        assert!(unit.starts_with(&format!(
            "# Installed by breezed {VERSION} on 2026-01-01T00:00:00Z\n"
        )));
        assert!(unit.contains("Type=exec"));
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/breezed run --config /etc/breezed/breezed.toml \
             --metrics-port 9762"
        ));
        assert!(unit.contains("MemoryHigh=16M"));
        assert!(unit.contains("MemoryMax=32M"));
        assert!(unit.contains("TasksMax=16"));
        assert!(unit.contains("SystemCallFilter=@system-service"));
        assert!(unit.contains("MemoryDenyWriteExecute=yes"));
        assert!(unit.contains("UMask=0077"));
        assert!(unit.contains("KillMode=mixed"));
    }

    #[test]
    fn install_runs_the_fixed_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let source = executable_source(dir.path());
        let paths = paths(dir.path());
        let runner = FakeRunner::with(&[], &[]);
        let deployment = deployment(dir.path(), runner.clone(), FakeFileOps::new(&[]), false);

        let results = deployment.install(&source, "martin").unwrap();

        assert_eq!(results.len(), INSTALL_ACTIONS.len());
        assert!(
            results
                .iter()
                .all(|(_, outcome)| *outcome == StepOutcome::Done)
        );
        let calls = runner.calls();
        assert_eq!(calls[0], strings(&["groupadd", "--system", "breezed"]));
        assert_eq!(
            calls[1],
            strings(&[
                "useradd",
                "--system",
                "--gid",
                "breezed",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                "breezed",
            ])
        );
        assert_eq!(
            calls[2],
            strings(&[
                "install",
                "-D",
                "-o",
                "root",
                "-g",
                "root",
                "-m",
                "0755",
                &source.to_string_lossy(),
                EXEC_PATH,
            ])
        );
        assert_eq!(
            &calls[3][..8],
            &strings(&["install", "-D", "-o", "root", "-g", "root", "-m", "0644"])[..]
        );
        assert!(calls[3][8].ends_with("breezed.service"));
        assert_eq!(calls[3][9], paths.unit_path.to_string_lossy());
        assert_eq!(
            &calls[4][..8],
            &strings(&[
                "install", "-D", "-o", "martin", "-g", "martin", "-m", "0664"
            ])[..]
        );
        assert!(calls[4][8].ends_with("breezed.toml"));
        assert_eq!(calls[4][9], paths.config_path().to_string_lossy());
        assert_eq!(
            &calls[5][..8],
            &strings(&["install", "-D", "-o", "root", "-g", "breezed", "-m", "0640"])[..]
        );
        assert!(calls[5][8].ends_with("breezed.env"));
        assert_eq!(calls[5][9], paths.env_path.to_string_lossy());
        assert_eq!(calls[6], strings(&["systemctl", "daemon-reload"]));
        assert_eq!(calls[7], strings(&["systemctl", "enable", "breezed"]));
        assert_eq!(calls[8], strings(&["systemctl", "restart", "breezed"]));
    }

    #[test]
    fn install_skips_existing_user_config_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let source = executable_source(dir.path());
        let paths = paths(dir.path());
        let fs = FakeFileOps::new(&[
            (&paths.config_path(), "tuned"),
            (&paths.env_path, "secrets"),
        ]);
        let runner = FakeRunner::with(&[], &[]);
        let deployment = deployment(dir.path(), runner.clone(), fs, true);

        let results = deployment.install(&source, "martin").unwrap();
        assert_eq!(results[0].1, StepOutcome::Skipped);
        assert_eq!(results[1].1, StepOutcome::Skipped);
        assert_eq!(results[4].1, StepOutcome::Skipped);
        assert_eq!(results[5].1, StepOutcome::Skipped);
        assert_eq!(runner.calls().len(), 5);
    }

    #[test]
    fn same_file_detects_symlinked_paths() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("breezed");
        fs::write(&target, b"binary").unwrap();
        let link = dir.path().join("breezed-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(same_file(&target, &link));
        assert!(!same_file(&target, &dir.path().join("other")));
    }

    #[test]
    fn install_rejects_a_missing_config_owner() {
        let dir = tempfile::tempdir().unwrap();
        let source = executable_source(dir.path());
        let mut users = FakeUsers::new(false);
        users.owner = false;
        let deployment = Deployment::new(
            paths(dir.path()),
            FakeRunner::with(&[], &[]),
            Arc::new(FakeFileOps::new(&[])),
            Arc::new(users),
        );
        let error = deployment.install(&source, "ghost").unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn install_requires_a_primary_group_for_the_config_owner() {
        let dir = tempfile::tempdir().unwrap();
        let source = executable_source(dir.path());
        let mut users = FakeUsers::new(false);
        users.owner_group = None;
        let deployment = Deployment::new(
            paths(dir.path()),
            FakeRunner::with(&[], &[]),
            Arc::new(FakeFileOps::new(&[])),
            Arc::new(users),
        );
        let error = deployment.install(&source, "martin").unwrap_err();
        assert!(error.to_string().contains("primary group"), "{error}");
    }

    #[test]
    fn install_rejects_a_non_executable_binary() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("breezed");
        fs::write(&source, b"binary").unwrap();
        let deployment = deployment(
            dir.path(),
            FakeRunner::with(&[], &[]),
            FakeFileOps::new(&[]),
            false,
        );
        let error = deployment.install(&source, "martin").unwrap_err();
        assert!(error.to_string().contains("not executable"), "{error}");
    }

    #[test]
    fn install_rejects_a_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let deployment = deployment(
            dir.path(),
            FakeRunner::with(&[], &[]),
            FakeFileOps::new(&[]),
            false,
        );
        let error = deployment
            .install(&dir.path().join("missing"), "martin")
            .unwrap_err();
        assert!(error.to_string().contains("cannot read binary"), "{error}");
    }

    #[test]
    fn remove_runs_the_fixed_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let runner = FakeRunner::with(&[], &[]);
        let fs = FakeFileOps::new(&[(&paths.unit_path, "unit")]);
        let deployment = deployment(dir.path(), runner.clone(), fs, true);

        deployment.remove().unwrap();

        assert_eq!(
            runner.calls(),
            vec![
                strings(&["systemctl", "disable", "--now", "breezed"]),
                strings(&["rm", "-f", &paths.unit_path.to_string_lossy()]),
                strings(&["rm", "-f", EXEC_PATH]),
                strings(&["systemctl", "daemon-reload"]),
            ]
        );
    }

    #[test]
    fn remove_skips_disable_when_unit_absent() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner::with(&[], &[]);
        let deployment = deployment(dir.path(), runner.clone(), FakeFileOps::new(&[]), true);

        let results = deployment.remove().unwrap();
        let outcomes: HashMap<String, StepOutcome> = results.into_iter().collect();
        assert_eq!(
            outcomes["systemctl disable --now breezed"],
            StepOutcome::Skipped
        );
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn apply_and_remove_require_root() {
        if euid() == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let source = executable_source(dir.path());
        let error = apply(
            &source,
            "martin",
            &paths(dir.path()),
            FakeRunner::with(&[], &[]),
            Arc::new(FakeFileOps::new(&[])),
            Arc::new(FakeUsers::new(false)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("root"));
        let error = remove(
            &paths(dir.path()),
            FakeRunner::with(&[], &[]),
            Arc::new(FakeFileOps::new(&[])),
            Arc::new(FakeUsers::new(false)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("root"));
    }
}
