//! Child-process execution and exit-code handling.
//!
//! Checked calls become errors; passthrough calls preserve the child's status.

use std::ffi::OsStr;
use std::process::{Command, ExitStatus, Output};

use anyhow::{Context, Result, bail};

pub fn checked_output(command: &mut Command) -> Result<Output> {
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

pub fn checked_status(command: &mut Command) -> Result<()> {
    let description = describe(command);
    let status = command
        .status()
        .with_context(|| format!("failed to start {description}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{description} failed with {}", display_status(status))
    }
}

pub fn passthrough_status(command: &mut Command) -> Result<ExitStatus> {
    let description = describe(command);
    command
        .status()
        .with_context(|| format!("failed to start {description}"))
}

pub fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

pub fn output_text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn describe(command: &Command) -> String {
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
