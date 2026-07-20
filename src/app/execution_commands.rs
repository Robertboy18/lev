//! Lean and Lake process execution.
//!
//! Build-like commands share one preparation path for toolchains, dependency
//! sync, locks, and local workspaces.

use std::ffi::OsString;

use anyhow::{Context, Result};

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cli::{BuildArgs, CheckArgs, LakeArgs, RunArgs, SyncArgs, TaskArgs, WorkflowArgs};
use crate::core::process::{exit_code, passthrough_status};
use crate::dependency::environment as dependency_env;
use crate::project::Project;
use crate::project::config::LevConfig;

use super::AppContext;
use super::environment_commands;
use super::sync_commands::{sync, sync_execution};
use super::toolchain_commands::ensure_toolchain;

/// Run an arbitrary command in the project's selected Lean environment.
pub(super) fn run_command(context: &AppContext, args: RunArgs) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;
    run_in_project(context, &project, &args.command, !args.execution.offline)
}

/// Build Lake targets after preparing the reproducible project environment.
pub(super) fn build(context: &AppContext, args: BuildArgs) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;

    let mut command = vec![OsString::from("lake")];
    if args.rehash {
        command.push(OsString::from("--rehash"));
    }
    command.push(OsString::from("build"));
    command.extend(args.args);
    let code = run_in_project(context, &project, &command, !args.execution.offline)?;
    if code != 0 {
        context.info(format!(
            "Lake build failed under {} with exit code {code}; retry trusted hash metadata with \
             `lev build --no-sync --rehash TARGET`",
            project.toolchain
        ));
    }
    Ok(code)
}

/// Run one standard Lake workflow such as `test`, `lint`, or `shake`.
pub(super) fn workflow(
    context: &AppContext,
    lake_subcommand: &str,
    args: WorkflowArgs,
) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;
    let mut command = vec![OsString::from("lake"), OsString::from(lake_subcommand)];
    command.extend(args.args);
    run_in_project(context, &project, &command, !args.execution.offline)
}

/// Pass a Lake command through without performing dependency synchronization.
pub(super) fn lake_command(
    context: &AppContext,
    lake_subcommand: &str,
    args: LakeArgs,
) -> Result<i32> {
    let source = context.project()?;
    let project = context.execution_project(&source, &source.toolchain, context.local)?;
    ensure_toolchain(context, &project, true)?;
    let mut command = vec![OsString::from("lake"), OsString::from(lake_subcommand)];
    command.extend(args.args);
    run_in_project(context, &project, &command, true)
}

/// Elaborate one Lean file with Lake's package environment.
pub(super) fn check(context: &AppContext, args: CheckArgs) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;
    let file = if project.root != source.root
        && args.file.is_absolute()
        && args.file.starts_with(&source.root)
    {
        project.root.join(
            args.file
                .strip_prefix(&source.root)
                .context("failed to map Lean file into local workspace")?,
        )
    } else {
        args.file
    };
    let mut command = vec![
        OsString::from("lake"),
        OsString::from("lean"),
        file.into_os_string(),
    ];
    command.extend(args.args);
    run_in_project(context, &project, &command, !args.execution.offline)
}

/// List or run one configured project task.
pub(super) fn task(context: &AppContext, args: TaskArgs) -> Result<i32> {
    let source = context.project()?;
    let config = LevConfig::read(&source.root)?;
    let Some(name) = args.name else {
        if config.tasks.is_empty() {
            println!(
                "No tasks configured in {}",
                source.root.join("lev.toml").display()
            );
        } else {
            for (name, command) in config.tasks {
                println!("{}\t{}", name, display_os_command(&command));
            }
        }
        return Ok(0);
    };
    let mut command = config.tasks.get(&name).cloned().with_context(|| {
        format!(
            "task {name:?} is not configured in {}",
            source.root.join("lev.toml").display()
        )
    })?;
    command.extend(args.args);
    let project = prepared_project(
        context,
        &source,
        args.lean.as_deref(),
        args.no_sync,
        args.offline,
    )?;
    run_in_project(context, &project, &command, !args.offline)
}

/// Select the requested environment and apply the common execution preflight.
fn prepared_project(
    context: &AppContext,
    source: &Project,
    selector: Option<&str>,
    no_sync: bool,
    offline: bool,
) -> Result<Project> {
    let (project, publish_locks) = select_project(context, source, selector, no_sync, offline)?;
    prepare_execution(context, source, &project, no_sync, offline, publish_locks)?;
    Ok(project)
}

/// Apply the common synchronization/toolchain preflight for build-like work.
fn prepare_execution(
    context: &AppContext,
    source: &Project,
    project: &Project,
    no_sync: bool,
    offline: bool,
    publish_locks: bool,
) -> Result<()> {
    if no_sync {
        ensure_toolchain(context, project, !offline)
    } else if publish_locks {
        sync_execution(
            context,
            source,
            project,
            SyncArgs {
                offline,
                update: false,
                locked: false,
                frozen: false,
            },
        )
        .map(|_| ())
    } else {
        sync(
            context,
            project,
            SyncArgs {
                offline,
                update: false,
                locked: false,
                frozen: false,
            },
        )
        .map(|_| ())
    }
}

/// Select the source project, local copy, or locked alternate environment.
fn select_project(
    context: &AppContext,
    source: &Project,
    selector: Option<&str>,
    no_sync: bool,
    offline: bool,
) -> Result<(Project, bool)> {
    if let Some(selector) = selector {
        let project = environment_commands::select(context, source, selector, offline, !no_sync)?;
        Ok((project, false))
    } else {
        Ok((
            context.execution_project(source, &source.toolchain, context.local)?,
            true,
        ))
    }
}

/// Run a command while holding lev's dependency and artifact-cache locks.
///
/// Direct Lake processes do not participate in these locks.
pub(super) fn run_in_project(
    context: &AppContext,
    project: &Project,
    args: &[OsString],
    install_toolchain: bool,
) -> Result<i32> {
    let (program, arguments) = args
        .split_first()
        .context("a command is required after `lev run`")?;
    let _dependency_environment = dependency_env::lock_attached(&context.cache, project)?;
    let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
    let mut command =
        context.runtime_command(&project.toolchain, program.as_os_str(), install_toolchain)?;
    command.args(arguments);
    context.command_env(&mut command, project)?;
    registry::record(&context.cache, project)?;
    Ok(exit_code(passthrough_status(&mut command)?))
}

fn display_os_command(command: &[OsString]) -> String {
    command
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
