//! Repeatable wall-clock profiling for commands in a Lean environment.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cli::ProfileArgs;
use crate::core::atomic_file::replace as atomic_write;
use crate::core::json_output::{self, schema};
use crate::core::process::exit_code;
use crate::dependency::environment as dependency_env;

use super::AppContext;
use super::execution_commands::{display_os_command, prepared_project, project_command};

#[derive(Debug, Serialize)]
struct ProfileReport {
    project: String,
    toolchain: String,
    command: String,
    warmup: u32,
    requested_repeats: u32,
    completed_repeats: u32,
    samples_ms: Vec<f64>,
    summary: Option<ProfileSummary>,
    exit_code: i32,
}

#[derive(Debug, Serialize)]
struct ProfileSummary {
    min_ms: f64,
    median_ms: f64,
    mean_ms: f64,
    max_ms: f64,
}

pub(super) fn profile(context: &AppContext, args: ProfileArgs) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;
    let command = if args.command.is_empty() {
        vec![OsString::from("lake"), OsString::from("build")]
    } else {
        args.command.clone()
    };

    let _dependency_environment = dependency_env::lock_attached(&context.cache, &project)?;
    let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
    registry::record(&context.cache, &project)?;

    for index in 0..args.warmup {
        context.info(format!("profile warmup {}/{}", index + 1, args.warmup));
        let code = run_once(
            context,
            &project,
            &command,
            args.json,
            !args.execution.offline,
        )?;
        if code != 0 {
            return emit_profile(
                &args,
                ProfileReport {
                    project: source.root.display().to_string(),
                    toolchain: project.toolchain.clone(),
                    command: display_os_command(&command),
                    warmup: args.warmup,
                    requested_repeats: args.repeat,
                    completed_repeats: 0,
                    samples_ms: Vec::new(),
                    summary: None,
                    exit_code: code,
                },
            );
        }
    }

    let mut samples = Vec::with_capacity(args.repeat as usize);
    let mut code = 0;
    for index in 0..args.repeat {
        context.info(format!("profile sample {}/{}", index + 1, args.repeat));
        let started = Instant::now();
        code = run_once(
            context,
            &project,
            &command,
            args.json,
            !args.execution.offline,
        )?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        if code != 0 {
            break;
        }
    }
    let report = ProfileReport {
        project: source.root.display().to_string(),
        toolchain: project.toolchain.clone(),
        command: display_os_command(&command),
        warmup: args.warmup,
        requested_repeats: args.repeat,
        completed_repeats: samples.len() as u32,
        summary: summarize(&samples),
        samples_ms: samples,
        exit_code: code,
    };
    emit_profile(&args, report)
}

fn run_once(
    context: &AppContext,
    project: &crate::project::Project,
    args: &[OsString],
    capture_stdout: bool,
    install_toolchain: bool,
) -> Result<i32> {
    let mut command = project_command(context, project, args, install_toolchain)?;
    if capture_stdout {
        command.stdin(Stdio::inherit());
        let output = command
            .output()
            .with_context(|| format!("failed to start {}", display_os_command(args)))?;
        let mut diagnostics = io::stderr().lock();
        diagnostics
            .write_all(&output.stdout)
            .context("failed to relay profiled command stdout")?;
        diagnostics
            .write_all(&output.stderr)
            .context("failed to relay profiled command stderr")?;
        Ok(exit_code(output.status))
    } else {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command
            .status()
            .with_context(|| format!("failed to start {}", display_os_command(args)))?;
        Ok(exit_code(status))
    }
}

fn summarize(samples: &[f64]) -> Option<ProfileSummary> {
    if samples.is_empty() {
        return None;
    }
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    let middle = ordered.len() / 2;
    let median = if ordered.len() % 2 == 0 {
        (ordered[middle - 1] + ordered[middle]) / 2.0
    } else {
        ordered[middle]
    };
    Some(ProfileSummary {
        min_ms: ordered[0],
        median_ms: median,
        mean_ms: ordered.iter().sum::<f64>() / ordered.len() as f64,
        max_ms: ordered[ordered.len() - 1],
    })
}

fn emit_profile(args: &ProfileArgs, report: ProfileReport) -> Result<i32> {
    let exit = report.exit_code;
    let bytes = if args.json {
        json_output::render(schema::PROFILE, &report)?
    } else {
        let mut text = format!(
            "profile: {} sample{}, command `{}`\n",
            report.completed_repeats,
            if report.completed_repeats == 1 {
                ""
            } else {
                "s"
            },
            report.command
        );
        for (index, sample) in report.samples_ms.iter().enumerate() {
            text.push_str(&format!("  {:>4}: {:>10.3} ms\n", index + 1, sample));
        }
        if let Some(summary) = &report.summary {
            text.push_str(&format!(
                "  min {:>10.3} ms  median {:>10.3} ms  mean {:>10.3} ms  max {:>10.3} ms\n",
                summary.min_ms, summary.median_ms, summary.mean_ms, summary.max_ms
            ));
        } else {
            text.push_str("  no measured samples\n");
        }
        text.into_bytes()
    };
    if let Some(path) = &args.output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_write(path, &bytes)?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write profile report")?;
    }
    Ok(exit)
}
