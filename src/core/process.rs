//! Child-process helpers.
//!
//! A checked call turns a failed child into an error that quotes the command.

use std::ffi::OsStr;
use std::process::{Command, ExitStatus, Output};

use anyhow::{Context, Result, bail};

/// Run `command` to completion and fail unless it exits successfully.
pub(crate) fn checked_output(command: &mut Command) -> Result<Output> {
    let description = describe(command);
    let output = command
        .output()
        .with_context(|| format!("failed to start {description}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{description} failed with {}{}{}",
            display_status(output.status),
            format_stream("stdout", &stdout),
            format_stream("stderr", &stderr)
        )
    }
}

/// The child's exit code, or 1 when a signal ended it.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Trimmed standard output as text.
pub(crate) fn output_text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Render a command the way a user would type it.
pub(crate) fn describe(command: &Command) -> String {
    let mut text = quote(command.get_program());
    for arg in command.get_args() {
        text.push(' ');
        text.push_str(&quote(arg));
    }
    text
}

fn quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_-./:+@=".contains(character))
    {
        value.into_owned()
    } else {
        format!("{value:?}")
    }
}

fn display_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "termination by signal".to_owned(),
    }
}

fn format_stream(name: &str, value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        String::new()
    } else {
        format!("\n{name}:\n{value}")
    }
}
