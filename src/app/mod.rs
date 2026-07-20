//! Application context and top-level CLI dispatch.
//!
//! Command behavior lives in the focused child modules below.

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::CommandFactory;
use clap_complete::generate;

use crate::cache::{CacheLayout, human_bytes};
use crate::cli::{Cli, Command as CliCommand};
use crate::project::local_workspace as workspace;
use crate::project::{Project, absolute};
use crate::toolchain::store::ToolchainStore;

mod cache_commands;
mod dependency_commands;
mod environment_commands;
mod execution_commands;
mod isolated_environment;
mod matrix_commands;
mod native_resolver;
mod project_commands;
mod script_commands;
mod self_commands;
mod sync_commands;
mod tool_commands;
mod toolchain_commands;
mod transaction;
mod workspace_commands;

/// Process-wide settings shared by commands and workspace members.
#[derive(Clone)]
struct AppContext {
    cache: CacheLayout,
    project_start: PathBuf,
    elan: OsString,
    git: OsString,
    store: ToolchainStore,
    quiet: bool,
    verbose: bool,
    progress: bool,
    local: bool,
}

/// Build the application context and dispatch one parsed CLI command.
pub fn execute(cli: Cli) -> Result<i32> {
    let project_start = match cli.project {
        Some(path) => absolute(&path)?,
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let context = AppContext {
        cache: CacheLayout::resolve(cli.cache_dir)?,
        store: ToolchainStore::resolve(cli.data_dir)?,
        project_start,
        elan: std::env::var_os("LEV_ELAN").unwrap_or_else(|| OsString::from("elan")),
        git: std::env::var_os("LEV_GIT").unwrap_or_else(|| OsString::from("git")),
        quiet: cli.quiet,
        verbose: cli.verbose,
        progress: !cli.no_progress && !cli.quiet && io::stderr().is_terminal(),
        local: cli.local,
    };

    match cli.command {
        CliCommand::Init(args) => project_commands::init(&context, args),
        CliCommand::Sync(args) => {
            let source = context.project()?;
            let project = context.execution_project(&source, &source.toolchain, context.local)?;
            sync_commands::sync_execution(&context, &source, &project, args)?;
            Ok(0)
        }
        CliCommand::Lock(args) => project_commands::lock(&context, args),
        CliCommand::Add(args) => dependency_commands::add(&context, args),
        CliCommand::Remove(args) => dependency_commands::remove(&context, args),
        CliCommand::Update(args) => dependency_commands::update(&context, args),
        CliCommand::Outdated(args) => dependency_commands::outdated(&context, args),
        CliCommand::Upgrade(args) => dependency_commands::upgrade(&context, args),
        CliCommand::Pin(args) => dependency_commands::pin(&context, args),
        CliCommand::Use(args) => project_commands::use_environment(&context, args),
        CliCommand::Run(args) => execution_commands::run_command(&context, args),
        CliCommand::Build(args) => execution_commands::build(&context, args),
        CliCommand::Export(args) => project_commands::export_project(&context, args),
        CliCommand::Audit(args) => project_commands::audit_project(&context, args),
        CliCommand::Publish(args) => project_commands::publish(&context, args),
        CliCommand::Test(args) => execution_commands::workflow(&context, "test", args),
        CliCommand::Lint(args) => execution_commands::workflow(&context, "lint", args),
        CliCommand::Clean(args) => execution_commands::lake_command(&context, "clean", args),
        CliCommand::Check(args) => execution_commands::check(&context, args),
        CliCommand::Shake(args) => execution_commands::workflow(&context, "shake", args),
        CliCommand::Deps(args) => project_commands::dependencies(&context, args),
        CliCommand::Tree(args) => project_commands::dependency_tree(&context, args),
        CliCommand::Why(args) => project_commands::why_dependency(&context, args),
        CliCommand::Matrix(args) => matrix_commands::matrix(&context, args),
        CliCommand::Task(args) => execution_commands::task(&context, args),
        CliCommand::Workspace(args) => {
            workspace_commands::workspace_command(&context, args.command)
        }
        CliCommand::Cache(args) => cache_commands::cache_command(&context, args.command),
        CliCommand::Toolchain(args) => {
            toolchain_commands::toolchain_command(&context, args.command)
        }
        CliCommand::Tool(args) => tool_commands::tool_command(&context, args.command),
        CliCommand::Script(args) => script_commands::script_command(&context, args.command),
        CliCommand::SelfManagement(args) => self_commands::self_command(&context, args.command),
        CliCommand::Doctor => project_commands::doctor(&context),
        CliCommand::Completions { shell } => {
            generate(shell, &mut Cli::command(), "lev", &mut io::stdout());
            Ok(0)
        }
    }
}

impl AppContext {
    /// Discover the nearest Lean project from this context's starting path.
    fn project(&self) -> Result<Project> {
        Project::discover(&self.project_start)
    }

    /// Apply lev's stable cache and project environment to one child process.
    ///
    /// Lake's native artifact cache remains isolated by selected toolchain.
    fn command_env(&self, command: &mut Command, project: &Project) -> Result<()> {
        self.cache.ensure()?;
        let lake_cache = self.cache.ensure_lake_dir(&project.toolchain)?;
        command
            .current_dir(&project.root)
            .env("LEV_PROJECT_ROOT", &project.root)
            .env("LEV_CACHE_DIR", &self.cache.root)
            .env("LAKE_CACHE_DIR", lake_cache)
            .env("LAKE_ARTIFACT_CACHE", "true");
        Ok(())
    }

    /// Select the source tree or materialize its persistent local workspace.
    fn execution_project(&self, source: &Project, toolchain: &str, local: bool) -> Result<Project> {
        if !local {
            return Ok(Project {
                root: source.root.clone(),
                toolchain: toolchain.to_owned(),
            });
        }
        let workspace = workspace::materialize(&self.cache, source, toolchain, &self.git)?;
        self.detail(format!(
            "local workspace {}: {} copied ({}), {} reused, {} removed",
            workspace.project.root.display(),
            workspace.stats.copied,
            human_bytes(workspace.stats.copied_bytes),
            workspace.stats.reused,
            workspace.stats.removed
        ));
        Ok(workspace.project)
    }

    /// Materialize a versioned environment under its lock key.
    fn environment_project(
        &self,
        source: &Project,
        toolchain: &str,
        workspace_key: &str,
    ) -> Result<Project> {
        let workspace =
            workspace::materialize_keyed(&self.cache, source, toolchain, workspace_key, &self.git)?;
        self.detail(format!(
            "versioned workspace {}: {} copied ({}), {} reused, {} removed",
            workspace.project.root.display(),
            workspace.stats.copied,
            human_bytes(workspace.stats.copied_bytes),
            workspace.stats.reused,
            workspace.stats.removed
        ));
        Ok(workspace.project)
    }

    /// Clone the context with project discovery rooted at one member.
    fn for_project(&self, root: &Path) -> Self {
        let mut context = self.clone();
        context.project_start = root.to_owned();
        context
    }

    /// Construct a command from lev's store or through elan.
    fn runtime_command(&self, toolchain: &str, program: &OsStr, install: bool) -> Result<Command> {
        if let Some(stored) = self.store.find(toolchain)? {
            let bin = stored.view.join("bin");
            let requested = Path::new(program);
            let executable = if requested.components().count() == 1 {
                let candidate = bin.join(requested);
                if candidate.is_file() {
                    candidate
                } else {
                    #[cfg(windows)]
                    {
                        let candidate = candidate.with_extension("exe");
                        if candidate.is_file() {
                            candidate
                        } else {
                            require_stored_core_program(
                                &stored.source_toolchain,
                                &stored.view,
                                program,
                            )?;
                            requested.to_owned()
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        require_stored_core_program(
                            &stored.source_toolchain,
                            &stored.view,
                            program,
                        )?;
                        requested.to_owned()
                    }
                }
            } else {
                requested.to_owned()
            };

            let mut paths = vec![bin];
            if let Some(path) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&path));
            }
            let path = std::env::join_paths(paths)
                .context("toolchain path cannot be represented in PATH")?;
            let mut command = Command::new(executable);
            command
                .env("PATH", path)
                .env_remove("ELAN_TOOLCHAIN")
                .env("LEV_TOOLCHAIN", &stored.source_toolchain)
                .env("LEV_TOOLCHAIN_ROOT", &stored.view);
            return Ok(command);
        }

        let mut command = Command::new(&self.elan);
        command.arg("run");
        if install {
            command.arg("--install");
        }
        command.arg(toolchain).arg(program);
        Ok(command)
    }

    /// Emit ordinary user-facing progress unless quiet mode is active.
    fn info(&self, message: impl AsRef<str>) {
        if !self.quiet {
            eprintln!("lev: {}", message.as_ref());
        }
    }

    /// Emit detailed diagnostics only in verbose mode.
    fn detail(&self, message: impl AsRef<str>) {
        if self.verbose && !self.quiet {
            eprintln!("lev: {}", message.as_ref());
        }
    }
}

/// Reject a stored toolchain that is missing a core Lean executable.
///
/// Other commands keep normal `PATH` lookup for workflows such as `lev run git`.
fn require_stored_core_program(toolchain: &str, view: &Path, program: &OsStr) -> Result<()> {
    if ["lean", "lake", "leanc", "lean.exe", "lake.exe", "leanc.exe"]
        .iter()
        .any(|required| program == OsStr::new(required))
    {
        bail!(
            "stored toolchain {toolchain} is incomplete: required executable {} is missing",
            view.join("bin").join(program).display()
        );
    }
    Ok(())
}
