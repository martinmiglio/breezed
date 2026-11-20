//! Daemon deployment subcommands: install/apply/status/uninstall/remove.
//!
//! Thin CLI shell over `entry::daemon`; all privileged logic lives there.
//! Re-executes itself once via sudo for the fixed, code-reviewed install steps.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::entry::common::{
    CliError, print_done, print_json, print_note, print_skipped, print_step,
};
use crate::entry::daemon::{
    CommandRunner, FileOps, InstallerPaths, RealFileOps, StepOutcome, SystemRunner, SystemUserDb,
    UserDb, apply, daemon_status, euid, getuser, install_labels, remove, remove_labels,
    resolve_self, which,
};
use serde_json::{Map, Value, json};

const KEEP: [&str; 2] = ["/etc/breezed.env", "/etc/breezed"];

fn print_step_plan(labels: &[String], note: &str) {
    print_note(note);
    for label in labels {
        print_step(label);
    }
}

fn print_results(results: &[(String, StepOutcome)]) {
    for (label, outcome) in results {
        match outcome {
            StepOutcome::Done => print_done(label),
            StepOutcome::Skipped => print_skipped(label),
        }
    }
}

fn resolve_sudo() -> Result<PathBuf, CliError> {
    which("sudo").ok_or_else(|| {
        CliError::runtime("sudo not found; `daemon install` needs a single sudo prompt")
    })
}

fn result_json(event: &str, steps: Option<&[String]>, keeps: bool) -> Value {
    let mut object = Map::with_capacity(2 + usize::from(steps.is_some()) + usize::from(keeps));
    object.insert("event".to_owned(), json!(event));
    if keeps {
        object.insert("keeps".to_owned(), json!(KEEP));
    }
    if let Some(steps) = steps {
        object.insert("steps".to_owned(), json!(steps));
    }
    Value::Object(object)
}

/// Build the fixed sudo argv used for the privileged install step.
pub fn install_sudo_argv(self_path: &Path, owner: &str) -> Vec<String> {
    vec![
        self_path.to_string_lossy().into_owned(),
        "daemon".to_owned(),
        "apply".to_owned(),
        "--owner".to_owned(),
        owner.to_owned(),
    ]
}

/// Build the fixed sudo argv used for the privileged uninstall step.
pub fn uninstall_sudo_argv(self_path: &Path) -> Vec<String> {
    vec![
        self_path.to_string_lossy().into_owned(),
        "daemon".to_owned(),
        "remove".to_owned(),
    ]
}

fn run_sudo(program: &Path, args: &[String]) -> Result<(), CliError> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| CliError::runtime(format!("sudo failed to run: {error}")))?;
    if status.success() {
        return Ok(());
    }
    Err(CliError::silent(status.code().unwrap_or(1).max(1)))
}

fn runner() -> Arc<dyn CommandRunner> {
    Arc::new(SystemRunner)
}

fn fs_ops() -> Arc<dyn FileOps> {
    Arc::new(RealFileOps)
}

fn users() -> Arc<dyn UserDb> {
    Arc::new(SystemUserDb)
}

/// Install the running binary and service via one sudo prompt.
///
/// # Errors
///
/// Returns a runtime error (`1`) on staging, escalation, or step failure.
pub fn daemon_install(dry_run: bool, json: bool) -> Result<(), CliError> {
    let paths = InstallerPaths::default();
    let labels = install_labels(&paths);
    let owner = getuser();
    let self_path = resolve_self();

    if euid() == 0 {
        let results = apply(&self_path, &owner, &paths, runner(), fs_ops(), users())
            .map_err(|error| CliError::runtime(error.to_string()))?;
        print_results(&results);
        if json {
            print_json(&result_json("install_complete", None, false))?;
        }
        return Ok(());
    }

    print_step_plan(&labels, "The following privileged steps will run:");
    if dry_run {
        if json {
            print_json(&result_json("install_planned", Some(&labels), false))?;
        }
        return Ok(());
    }

    let sudo = resolve_sudo()?;
    let argv = install_sudo_argv(&self_path, &owner);
    run_sudo(&sudo, &argv)?;
    if json {
        print_json(&result_json("install_complete", None, false))?;
    }
    Ok(())
}

/// Internal privileged install step, invoked via sudo by `daemon install`.
///
/// # Errors
///
/// Returns a runtime error (`1`) when a privileged step fails.
pub fn daemon_apply(owner: &str) -> Result<(), CliError> {
    validate_sudo_owner(owner, env::var("SUDO_USER").ok().as_deref())?;
    let results = apply(
        &resolve_self(),
        owner,
        &InstallerPaths::default(),
        runner(),
        fs_ops(),
        users(),
    )
    .map_err(|error| CliError::runtime(error.to_string()))?;
    print_results(&results);
    Ok(())
}

fn validate_sudo_owner(owner: &str, sudo_user: Option<&str>) -> Result<(), CliError> {
    if let Some(sudo_user) = sudo_user.filter(|user| !user.is_empty()) {
        if sudo_user != owner {
            return Err(CliError::runtime(format!(
                "refusing config owner '{owner}': sudo was invoked by '{sudo_user}'"
            )));
        }
    }
    Ok(())
}

/// Report unit state and binary version as one JSON object.
///
/// # Errors
///
/// Returns a runtime error (`1`) when the report cannot be produced.
pub fn daemon_status_command() -> Result<(), CliError> {
    let report = daemon_status(&InstallerPaths::default(), &SystemRunner, &RealFileOps);
    print_json(&json!({
        "unit_present": report.unit_present,
        "active": report.active,
        "enabled": report.enabled,
        "unit_version": report.unit_version,
        "binary_version": report.binary_version,
    }))
}

/// Stop and remove the service and binary via one sudo prompt.
///
/// # Errors
///
/// Returns a runtime error (`1`) on escalation or step failure.
pub fn daemon_uninstall(dry_run: bool, json: bool) -> Result<(), CliError> {
    let paths = InstallerPaths::default();
    let labels = remove_labels(&paths);
    let self_path = resolve_self();

    if euid() == 0 {
        let results = remove(&paths, runner(), fs_ops(), users())
            .map_err(|error| CliError::runtime(error.to_string()))?;
        print_results(&results);
        if json {
            print_json(&result_json("uninstall_complete", None, true))?;
        }
        return Ok(());
    }

    print_step_plan(&labels, "The following privileged steps will run:");
    if dry_run {
        if json {
            print_json(&result_json("uninstall_planned", Some(&labels), true))?;
        }
        return Ok(());
    }

    let sudo = resolve_sudo()?;
    let argv = uninstall_sudo_argv(&self_path);
    run_sudo(&sudo, &argv)?;
    if json {
        print_json(&result_json("uninstall_complete", None, true))?;
    }
    Ok(())
}

/// Internal privileged uninstall step, invoked via sudo by `daemon uninstall`.
///
/// # Errors
///
/// Returns a runtime error (`1`) when a privileged step fails.
pub fn daemon_remove() -> Result<(), CliError> {
    let results = remove(&InstallerPaths::default(), runner(), fs_ops(), users())
        .map_err(|error| CliError::runtime(error.to_string()))?;
    print_results(&results);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_owner_must_match_the_invoking_user() {
        assert!(validate_sudo_owner("martin", Some("martin")).is_ok());
        assert!(validate_sudo_owner("root", Some("martin")).is_err());
        assert!(validate_sudo_owner("martin", None).is_ok());
        assert!(validate_sudo_owner("martin", Some("")).is_ok());
    }

    #[test]
    fn sudo_argv_is_minimal() {
        assert_eq!(
            install_sudo_argv(Path::new("/usr/local/bin/breezed"), "martin"),
            [
                "/usr/local/bin/breezed",
                "daemon",
                "apply",
                "--owner",
                "martin"
            ]
        );
        assert_eq!(
            uninstall_sudo_argv(Path::new("/usr/local/bin/breezed")),
            ["/usr/local/bin/breezed", "daemon", "remove"]
        );
    }
}
