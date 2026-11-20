//! Shared CLI helpers: exit-code contract and stderr formatting.

use std::io::{self, ErrorKind, IsTerminal, Write};
use std::process;

/// A CLI failure carrying the process exit code and an optional message.
///
/// `code` follows the contract: `0` ok, `1` runtime error, `2` usage/config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    code: i32,
    message: Option<String>,
}

impl CliError {
    /// Usage / configuration failure (`2`), with a message for stderr.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: Some(message.into()),
        }
    }

    /// Runtime failure (`1`), with a message for stderr.
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: Some(message.into()),
        }
    }

    /// Exit with `code` and no extra output.
    pub const fn silent(code: i32) -> Self {
        Self {
            code,
            message: None,
        }
    }

    /// Exit code this failure maps to.
    pub const fn code(&self) -> i32 {
        self.code
    }

    /// Print the `breezed: …` line to stderr and terminate.
    pub fn exit(self) -> ! {
        if let Some(message) = self.message {
            let stderr = io::stderr();
            let mut stream = stderr.lock();
            if stderr.is_terminal() {
                let _ = writeln!(stream, "\u{1b}[31mbreezed: {message}\u{1b}[0m");
            } else {
                let _ = writeln!(stream, "breezed: {message}");
            }
        }
        process::exit(self.code);
    }
}

/// Write one machine-output line to stdout.
///
/// Treats a closed downstream pipe as a normal, successful end: `breezed status
/// | head -1` must not panic or report failure. Other I/O errors are runtime
/// failures.
///
/// # Errors
///
/// Returns a silent success (`0`) for `BrokenPipe`, or a runtime error (`1`) for
/// other write failures.
pub fn write_stdout_line(line: &str) -> Result<(), CliError> {
    write_line(&mut io::stdout().lock(), line).map_err(|error| {
        if error.kind() == ErrorKind::BrokenPipe {
            CliError::silent(0)
        } else {
            CliError::runtime(format!("cannot write stdout: {error}"))
        }
    })
}

fn write_line(out: &mut impl Write, line: &str) -> io::Result<()> {
    writeln!(out, "{line}")
}

/// Write one machine-readable JSON value to stdout.
///
/// # Errors
///
/// Propagates the [`write_stdout_line`] exit-code contract.
pub fn print_json(value: &serde_json::Value) -> Result<(), CliError> {
    write_stdout_line(&value.to_string())
}

/// Print a daemon step bullet to stderr.
pub fn print_step(label: &str) {
    print_stderr(&format!("  • {label}"), "\u{1b}[1;36m");
}

/// Print a daemon plan note to stderr.
pub fn print_note(note: &str) {
    print_stderr(note, "\u{1b}[33m");
}

/// Print a completed daemon step to stderr.
pub fn print_done(label: &str) {
    print_stderr(&format!("  ✔ {label}"), "\u{1b}[32m");
}

/// Print a skipped daemon step to stderr.
pub fn print_skipped(label: &str) {
    print_stderr(&format!("  · {label}"), "\u{1b}[2m");
}

fn print_stderr(text: &str, color: &str) {
    let stderr = io::stderr();
    let mut stream = stderr.lock();
    if stderr.is_terminal() {
        let _ = writeln!(stream, "{color}{text}\u{1b}[0m");
    } else {
        let _ = writeln!(stream, "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_pipe_is_a_normal_end_for_line_writes() {
        let error = write_line(&mut BrokenPipe, "line").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    }

    #[test]
    fn line_writes_append_a_single_newline() {
        let mut buffer = Vec::new();
        write_line(&mut buffer, "{\"ok\": true}").unwrap();
        assert_eq!(buffer, b"{\"ok\": true}\n");
    }
}
