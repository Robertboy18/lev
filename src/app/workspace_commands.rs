//! Commands across an ordered set of Lake projects.
//!
//! Members use the normal single-project code paths. Only the aggregate lock
//! is published across the whole workspace.

use anyhow::{Context, Result};

use crate::cli::{BuildArgs, ExecutionArgs, RunArgs, SyncArgs, WorkspaceCommand};
use crate::core::json_output::{self, schema};
use crate::project::lockfile;
use crate::project::workspace::{ProjectWorkspace, WorkspaceMember};

use super::AppContext;
use super::execution_commands::{build, run_command};
use super::sync_commands::sync_execution;

/// Stable JSON representation for `lev workspace list`.
#[derive(Debug, serde::Serialize)]
struct WorkspaceListing {
    root: String,
    lockfile: String,
    members: Vec<WorkspaceListingMember>,
}

/// One independently pinned Lake project in a workspace listing.
#[derive(Debug, serde::Serialize)]
struct WorkspaceListingMember {
    path: String,
    root: String,
    toolchain: String,
}

/// Dispatch a command across the deterministic member set declared by the
/// nearest ancestor `[workspace]` configuration.
///
/// Each member receives a cloned [`AppContext`]. In particular, command
/// options such as `--local`, `--offline`, cache locations, and verbosity
/// retain their ordinary single-project meaning.
pub(super) fn workspace_command(context: &AppContext, command: WorkspaceCommand) -> Result<i32> {
    let workspace = ProjectWorkspace::discover(&context.project_start)?;
    match command {
        WorkspaceCommand::List { json } => {
            if json {
                let listing = WorkspaceListing {
                    root: workspace.root.display().to_string(),
                    lockfile: workspace.lock_path().display().to_string(),
                    members: workspace
                        .members
                        .iter()
                        .map(|member| WorkspaceListingMember {
                            path: member.relative.clone(),
                            root: member.project.root.display().to_string(),
                            toolchain: member.project.toolchain.clone(),
                        })
                        .collect(),
                };
                json_output::print(schema::WORKSPACE_LIST, &listing)?;
            } else {
                for member in &workspace.members {
                    println!("{}\t{}", member.relative, member.project.toolchain);
                }
            }
            Ok(0)
        }
        WorkspaceCommand::Sync {
            offline,
            update,
            locked,
            frozen,
            keep_going,
        } => {
            let sync_args = SyncArgs {
                offline,
                update,
                locked,
                frozen,
            };
            let code = run_workspace_members(
                context,
                &workspace,
                "sync",
                keep_going,
                |member_context, member| {
                    let project = member_context.execution_project(
                        &member.project,
                        &member.project.toolchain,
                        member_context.local,
                    )?;
                    sync_execution(member_context, &member.project, &project, sync_args)?;
                    Ok(0)
                },
            )?;

            // The aggregate lock is an all-members snapshot. Publishing it
            // after a partial run would falsely describe failed members as
            // synchronized, so it changes only after complete success.
            if code == 0 {
                let path = workspace.refresh_lock()?;
                workspace.verify_lock()?;
                context.info(format!("wrote {}", path.display()));
            }
            Ok(code)
        }
        WorkspaceCommand::Lock { check } => {
            if check {
                workspace.verify_lock()?;
                context.info(format!(
                    "workspace lock is current: {}",
                    workspace.lock_path().display()
                ));
            } else {
                for member in &workspace.members {
                    lockfile::refresh(&member.project)
                        .with_context(|| format!("workspace member {}", member.relative))?;
                    context.detail(format!("locked workspace member {}", member.relative));
                }
                let path = workspace.refresh_lock()?;
                workspace.verify_lock()?;
                context.info(format!("wrote {}", path.display()));
            }
            Ok(0)
        }
        WorkspaceCommand::Build {
            no_sync,
            offline,
            keep_going,
            rehash,
            args,
        } => {
            let build_args = BuildArgs {
                execution: ExecutionArgs {
                    lean: None,
                    no_sync,
                    offline,
                },
                rehash,
                args,
            };
            run_workspace_members(
                context,
                &workspace,
                "build",
                keep_going,
                |member_context, _| build(member_context, build_args.clone()),
            )
        }
        WorkspaceCommand::Run {
            no_sync,
            offline,
            keep_going,
            command,
        } => {
            let run_args = RunArgs {
                execution: ExecutionArgs {
                    lean: None,
                    no_sync,
                    offline,
                },
                command,
            };
            run_workspace_members(
                context,
                &workspace,
                "run",
                keep_going,
                |member_context, _| run_command(member_context, run_args.clone()),
            )
        }
    }
}

/// Run members in path order and preserve the first child exit status.
fn run_workspace_members(
    context: &AppContext,
    workspace: &ProjectWorkspace,
    operation: &str,
    keep_going: bool,
    mut run: impl FnMut(&AppContext, &WorkspaceMember) -> Result<i32>,
) -> Result<i32> {
    let mut first_failure = None;
    for member in &workspace.members {
        context.info(format!("workspace {operation}: {}", member.relative));
        let member_context = context.for_project(&member.project.root);
        match run(&member_context, member) {
            Ok(0) => {}
            Ok(code) => {
                eprintln!(
                    "{}: {operation} failed with exit code {code}",
                    member.relative
                );
                first_failure.get_or_insert(code);
                if !keep_going {
                    return Ok(code);
                }
            }
            Err(error) => {
                if !keep_going {
                    return Err(error)
                        .with_context(|| format!("workspace member {}", member.relative));
                }
                eprintln!("{}: {error:#}", member.relative);
                first_failure.get_or_insert(1);
            }
        }
    }
    Ok(first_failure.unwrap_or(0))
}
