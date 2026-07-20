//! Clap definitions for lev's command-line interface.
//!
//! These types parse arguments and have no side effects.

use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::Shell;

mod cache;
mod project;
mod toolchain;
mod tools;

pub use cache::*;
pub use project::*;
pub use toolchain::*;
pub use tools::*;

#[derive(Debug, Parser)]
#[command(
    name = "lev",
    version,
    about = "Fast Lean toolchains, dependencies, and project environments",
    long_about = None,
    after_help = "Use `lev --help` to list advanced commands. Run `lev <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Start project discovery from this path.
    #[arg(long, global = true, value_name = "PATH")]
    pub project: Option<PathBuf>,

    /// Override LEV_CACHE_DIR.
    #[arg(long, global = true, value_name = "PATH", hide_short_help = true)]
    pub cache_dir: Option<PathBuf>,

    /// Override LEV_DATA_DIR for persistent toolchain objects.
    #[arg(long, global = true, value_name = "PATH", hide_short_help = true)]
    pub data_dir: Option<PathBuf>,

    /// Suppress lev's informational output.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Show cache reuse and orchestration details.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Hide progress updates for long operations.
    #[arg(long, global = true, hide_short_help = true)]
    pub no_progress: bool,

    /// Run eligible commands in a persistent local workspace.
    #[arg(long, global = true)]
    pub local: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a standard Lake project pinned to a Lean toolchain.
    Init(InitArgs),

    /// Create or update the project environment from its locks.
    Sync(SyncArgs),

    /// Generate or verify base and version-specific dependency locks.
    Lock(LockArgs),

    /// Add or replace a dependency in lakefile.toml.
    Add(AddArgs),

    /// Remove a dependency from lakefile.toml.
    Remove(RemoveArgs),

    /// Update locked dependencies through Lake.
    Update(UpdateArgs),

    /// Synchronize, then run a command in the project's Lean environment.
    Run(RunArgs),

    /// Synchronize, then run `lake build`.
    Build(BuildArgs),

    /// Synchronize, then run the Lake test driver.
    Test(WorkflowArgs),

    /// Install, list, and remove Lean toolchains.
    Toolchain(ToolchainArgs),

    /// Install and run Lean package executables outside a project.
    Tool(ToolArgs),

    /// Inspect lev's shared caches.
    Cache(CacheArgs),

    /// Check the local Lean, Lake, Git, and cache configuration.
    Doctor,

    /// Report newer Reservoir releases compatible with this Lean toolchain.
    Outdated(OutdatedArgs),

    /// Upgrade direct dependencies using Reservoir compatibility metadata.
    Upgrade(UpgradeArgs),

    /// Pin the project to a different Lean toolchain.
    Pin(PinArgs),

    /// Make a resolved Lean environment the project's default.
    Use(UseArgs),

    /// Synchronize, then run the Lake lint driver.
    Lint(WorkflowArgs),

    /// Remove project build outputs through Lake.
    Clean(LakeArgs),

    /// Elaborate a Lean file in the Lake environment.
    Check(CheckArgs),

    /// Analyze or minimize unused Lean imports.
    Shake(WorkflowArgs),

    /// Inspect dependencies recorded in lake-manifest.json.
    Deps(DepsArgs),

    /// Print the resolved dependency tree.
    Tree(TreeArgs),

    /// Explain why a package is in the dependency graph.
    Why(WhyArgs),

    /// Run or list commands configured in lev.toml.
    Task(TaskArgs),

    /// Operate on all Lean projects declared in [workspace].
    Workspace(WorkspaceArgs),

    /// Run or check one Lean file with inline dependency metadata.
    Script(ScriptArgs),

    /// Optional CI: run one command against multiple Lean toolchains.
    Matrix(MatrixArgs),

    /// Export the locked dependency inventory or a CycloneDX SBOM.
    Export(ExportArgs),

    /// Audit local lock, toolchain, and dependency checkout integrity.
    Audit(AuditArgs),

    /// Verify and upload Lake build artifacts to an existing GitHub release.
    Publish(PublishArgs),

    /// Inspect, update, or uninstall the lev executable.
    #[command(name = "self")]
    SelfManagement(SelfArgs),

    /// Generate shell completion definitions.
    Completions {
        /// Shell whose completion format should be generated.
        #[arg(value_enum)]
        shell: Shell,
    },
}

const QUICK_COMMANDS: &[&str] = &[
    "init",
    "sync",
    "lock",
    "add",
    "remove",
    "update",
    "run",
    "build",
    "test",
    "toolchain",
    "tool",
    "cache",
    "doctor",
];

/// Parse process arguments while keeping short and long root help distinct.
pub(crate) fn parse() -> Cli {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let use_quick_help = arguments.iter().skip(1).any(|argument| argument == "-h")
        && !arguments
            .iter()
            .skip(1)
            .any(|argument| argument == "--help");

    if use_quick_help {
        let matches = quick_command().get_matches_from(arguments);
        return Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    }
    Cli::parse_from(arguments)
}

fn quick_command() -> clap::Command {
    Cli::command().mut_subcommands(|subcommand| {
        if QUICK_COMMANDS.contains(&subcommand.get_name()) {
            subcommand
        } else {
            subcommand.hide(true)
        }
    })
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::{Cli, quick_command};

    #[test]
    fn short_help_keeps_the_first_run_surface_small() {
        let short = quick_command().render_help().to_string();
        let long = Cli::command().render_long_help().to_string();

        for command in ["init", "sync", "lock", "add", "build", "toolchain"] {
            assert!(short.contains(&format!("  {command} ")), "{short}");
        }
        for command in ["matrix", "publish", "workspace", "completions"] {
            assert!(!short.contains(&format!("  {command} ")), "{short}");
            assert!(long.contains(&format!("  {command} ")), "{long}");
        }
        assert!(short.contains("lev --help"), "{short}");
    }
}
