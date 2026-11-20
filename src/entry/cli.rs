//! Clap CLI surface: run/set/auto/status/validate plus the daemon sub-app.
//!
//! Machine output goes to stdout as JSON; errors and human daemon text go to
//! stderr. Exit codes: `0` ok, `1` runtime error, `2` usage/config error.

use std::path::PathBuf;
use std::process;

#[cfg(test)]
use clap::CommandFactory;
use clap::{Args, Parser, Subcommand};

use crate::entry::common::CliError;
use crate::entry::daemon_cli::{
    ApplyOptions, InstallOptions, daemon_apply, daemon_install, daemon_remove,
    daemon_status_command, daemon_uninstall,
};
use crate::entry::runtime::{
    auto_command, run_command, set_command, status_command, validate_command,
};

/// Curve-based fan controller for Dell PowerEdge servers via iDRAC/IPMI.
#[allow(clippy::doc_markdown)]
#[derive(Debug, Parser)]
#[command(
    name = "breezed",
    version,
    subcommand_required = true,
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the control loop until SIGINT/SIGTERM.
    Run(RunArgs),
    /// Set a fixed manual fan percentage (1-100) until the next mode change.
    Set(SetArgs),
    /// Hand fan control back to the iDRAC's automatic policy.
    Auto(AutoArgs),
    /// Print one-shot sensor status as JSON (human-readable with --verbose).
    Status(StatusArgs),
    /// Validate a config file and print a JSON summary; --probe adds a live read.
    Validate(ValidateArgs),
    #[allow(
        missing_docs,
        reason = "clap uses doc comments for help; keep the description blank"
    )]
    Daemon {
        /// Daemon subcommand.
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

/// Arguments for `run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Config file path.
    #[arg(short = 'c', long, value_name = "path", default_value = "breezed.toml")]
    pub config: PathBuf,
    /// Loopback port for the metrics endpoint.
    #[arg(long, value_name = "int")]
    pub metrics_port: Option<u16>,
    /// Plain-text debug logging instead of JSON.
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

/// Arguments for `set`.
#[derive(Debug, Args)]
pub struct SetArgs {
    /// Fixed fan percentage (1-100).
    #[arg(value_name = "pct")]
    pub pct: i64,
    /// Config file path.
    #[arg(short = 'c', long, value_name = "path", default_value = "breezed.toml")]
    pub config: PathBuf,
}

/// Arguments for `auto`.
#[derive(Debug, Args)]
pub struct AutoArgs {
    /// Config file path.
    #[arg(short = 'c', long, value_name = "path", default_value = "breezed.toml")]
    pub config: PathBuf,
}

/// Arguments for `status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Config file path.
    #[arg(long, value_name = "path", default_value = "breezed.toml")]
    pub config: PathBuf,
    /// Human-readable output instead of JSON.
    #[arg(long)]
    pub verbose: bool,
}

/// Arguments for `validate`.
#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Config file path.
    #[arg(value_name = "path")]
    pub path: PathBuf,
    /// Also read a live temperature from the iDRAC.
    #[arg(long)]
    pub probe: bool,
}

#[derive(Debug, Subcommand)]
#[command(disable_help_subcommand = true)]
#[allow(
    missing_docs,
    reason = "the nested command's help text is intentionally blank"
)]
pub enum DaemonCommand {
    /// Stage the unit/env/config, then install the system service via one sudo prompt.
    Install(InstallArgs),
    /// Internal privileged install step, invoked via sudo by `daemon install`.
    #[command(hide = true)]
    Apply(ApplyArgs),
    /// Status subcommand (blank description).
    #[command(about = None)]
    Status,
    /// Stop and remove the service and runtime via one sudo prompt; keeps /etc/breezed.env/.
    Uninstall(UninstallArgs),
    /// Internal privileged uninstall step, invoked via sudo by `daemon uninstall`.
    #[command(hide = true)]
    Remove,
}

/// Arguments for `daemon install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Print the privileged steps without escalating.
    #[arg(long)]
    pub dry_run: bool,
    /// Print a machine-readable result to stdout.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for the hidden `daemon apply`.
#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Invoking user/group for the config file.
    #[arg(long, value_name = "str")]
    pub owner: String,
}

/// Arguments for `daemon uninstall`.
#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Print the privileged steps without escalating.
    #[arg(long)]
    pub dry_run: bool,
    /// Print a machine-readable result to stdout.
    #[arg(long)]
    pub json: bool,
}

/// Parse arguments and exit with the contract's status code.
pub fn main() -> ! {
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(()) => process::exit(0),
        Err(error) => error.exit(),
    }
}

/// Execute one parsed command.
///
/// # Errors
///
/// Propagates the command's [`CliError`] (exit code plus optional message).
pub fn dispatch(command: Command) -> Result<(), CliError> {
    match command {
        Command::Run(args) => run_command(&args.config, args.metrics_port, args.verbose),
        Command::Set(args) => set_command(args.pct, &args.config),
        Command::Auto(args) => auto_command(&args.config),
        Command::Status(args) => status_command(&args.config, args.verbose),
        Command::Validate(args) => validate_command(&args.path, args.probe),
        Command::Daemon { command } => match command {
            DaemonCommand::Install(args) => daemon_install(&InstallOptions {
                dry_run: args.dry_run,
                json: args.json,
            }),
            DaemonCommand::Apply(args) => daemon_apply(&ApplyOptions { owner: args.owner }),
            DaemonCommand::Status => daemon_status_command(),
            DaemonCommand::Uninstall(args) => daemon_uninstall(args.dry_run, args.json),
            DaemonCommand::Remove => daemon_remove(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn help(path: &[&str]) -> String {
        let mut command = Cli::command();
        for name in path {
            command = command
                .find_subcommand(name)
                .expect("known subcommand")
                .clone();
        }
        command.render_help().to_string()
    }

    #[test]
    fn help_lists_all_five_commands() {
        let combined = [
            help(&[]),
            help(&["run"]),
            help(&["set"]),
            help(&["auto"]),
            help(&["status"]),
            help(&["validate"]),
        ]
        .join("\n");
        for name in ["run", "set", "auto", "status", "validate"] {
            assert!(combined.contains(name), "missing {name} in help");
        }
        for flag in ["--config", "-c", "--metrics-port", "--verbose", "--probe"] {
            assert!(combined.contains(flag), "missing {flag} in help");
        }
    }

    #[test]
    fn daemon_help_hides_internal_commands() {
        let output = help(&["daemon"]);
        for name in ["install", "status", "uninstall"] {
            assert!(output.contains(name));
        }
        let daemon = Cli::command()
            .find_subcommand("daemon")
            .expect("daemon subcommand")
            .clone();
        assert!(daemon.find_subcommand("apply").unwrap().is_hide_set());
        assert!(daemon.find_subcommand("remove").unwrap().is_hide_set());
    }
}
