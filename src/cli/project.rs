//! Arguments for project, dependency, execution, and workspace commands.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Directory to initialize.
    #[arg(default_value = ".", value_name = "PATH")]
    pub path: PathBuf,

    /// Lean release, channel, nightly, or full elan toolchain name.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub lean: String,

    /// Lake package name. Defaults to the directory name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Lake template, such as std, lib, exe, math, or math-lax.
    #[arg(long, default_value = "std", value_name = "TEMPLATE")]
    pub template: String,
}

#[derive(Debug, Args, Clone, Copy)]
pub struct SyncArgs {
    /// Refuse all network access.
    #[arg(long)]
    pub offline: bool,

    /// Ask Lake to update the manifest before synchronizing.
    #[arg(long, conflicts_with = "offline")]
    pub update: bool,

    /// Fail when lake-manifest.json is absent instead of creating it.
    #[arg(long)]
    pub locked: bool,

    /// Require lev.lock, lake-manifest.json, and project configuration to match.
    #[arg(long, conflicts_with = "update")]
    pub frozen: bool,
}

#[derive(Debug, Args, Clone)]
pub struct LockArgs {
    /// Verify lev.lock without changing it.
    #[arg(long, conflicts_with = "refresh")]
    pub check: bool,

    /// Lean environment to resolve or verify. Repeat to lock several versions.
    #[arg(long = "lean", value_name = "TOOLCHAIN")]
    pub toolchains: Vec<String>,

    /// Require requested version environments to exist in the current lock.
    #[arg(long, conflicts_with = "refresh")]
    pub offline: bool,

    /// Re-resolve requested environments and refresh compatibility metadata.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Lake package name.
    #[arg(value_name = "PACKAGE")]
    pub name: String,

    /// Git repository URL.
    #[arg(long, value_name = "URL", conflicts_with_all = ["path", "scope"])]
    pub git: Option<String>,

    /// Local dependency path.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["git", "scope", "rev"])]
    pub path: Option<PathBuf>,

    /// Reservoir package owner or scope.
    #[arg(long, value_name = "SCOPE", conflicts_with_all = ["git", "path"])]
    pub scope: Option<String>,

    /// Requested Git revision, tag, or branch.
    #[arg(long, value_name = "REV")]
    pub rev: Option<String>,

    /// Record the dependency in this named project group.
    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,

    /// Replace an existing dependency declaration.
    #[arg(long)]
    pub replace: bool,

    /// Edit lakefile.toml without updating the manifest.
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Lake package name.
    #[arg(value_name = "PACKAGE")]
    pub name: String,

    /// Edit lakefile.toml without updating the manifest.
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Direct dependencies to update. Omit to update all dependencies.
    #[arg(conflicts_with = "group")]
    #[arg(value_name = "PACKAGE")]
    pub packages: Vec<String>,

    /// Update direct dependencies in this configured group.
    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,
}

#[derive(Debug, Args)]
pub struct OutdatedArgs {
    /// Packages to inspect. Omit to inspect direct dependencies.
    #[arg(conflicts_with = "group")]
    #[arg(value_name = "PACKAGE")]
    pub packages: Vec<String>,

    /// Inspect direct dependencies in this configured group.
    #[arg(long, value_name = "GROUP", conflicts_with = "all")]
    pub group: Option<String>,

    /// Include transitive dependencies.
    #[arg(long)]
    pub all: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,

    /// Fail with exit code 1 when a compatible update exists.
    #[arg(long)]
    pub check: bool,

    /// Use only cached Reservoir metadata.
    #[arg(long, conflicts_with = "refresh")]
    pub offline: bool,

    /// Ignore fresh metadata cached by earlier queries.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct UpgradeArgs {
    /// Direct dependencies to upgrade. Omit to upgrade all indexed direct dependencies.
    #[arg(value_name = "PACKAGE", conflicts_with_all = ["lean", "group"])]
    pub packages: Vec<String>,

    /// Upgrade direct dependencies in this configured group.
    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,

    /// Upgrade the project and compatible direct dependencies to this Lean toolchain.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub lean: Option<String>,

    /// Print the transaction without changing project files.
    #[arg(long)]
    pub dry_run: bool,

    /// Ignore fresh Reservoir metadata cached by earlier queries.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct PinArgs {
    /// Lean release, channel, nightly, or full elan toolchain name.
    #[arg(value_name = "TOOLCHAIN")]
    pub toolchain: String,

    /// Update the Lake manifest after changing toolchains.
    #[arg(long, conflicts_with = "offline")]
    pub update: bool,

    /// Require the toolchain to already be installed.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Args)]
pub struct UseArgs {
    /// Lean toolchain to resolve and make the project default.
    #[arg(value_name = "TOOLCHAIN")]
    pub toolchain: String,

    /// Require the environment to exist in lev.lock and the toolchain locally.
    #[arg(long)]
    pub offline: bool,
}

/// Environment selection shared by commands that prepare a project to run.
#[derive(Debug, Args, Clone)]
pub struct ExecutionArgs {
    /// Select a locked Lean environment without changing lean-toolchain.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub lean: Option<String>,

    /// Run without synchronizing dependencies first.
    #[arg(long)]
    pub no_sync: bool,

    /// Refuse toolchain and dependency network access.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Args, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Command and arguments to execute.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

#[derive(Debug, Args, Clone)]
pub struct BuildArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Recompute file hashes instead of trusting Lake .hash sidecars.
    #[arg(long)]
    pub rehash: bool,

    /// Arguments passed to `lake build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

/// Arguments controlling deterministic project metadata export.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Output representation.
    #[arg(long, value_enum, default_value_t = ProjectExportFormat::Lev)]
    pub format: ProjectExportFormat,

    /// Destination file. Omit or use `-` for standard output.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Replace an existing output file.
    #[arg(long)]
    pub force: bool,
}

/// Supported deterministic project export representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProjectExportFormat {
    /// lev's normalized dependency and integrity inventory.
    Lev,
    /// CycloneDX 1.6 JSON software bill of materials.
    Cyclonedx,
}

/// Arguments controlling local reproducibility and integrity checks.
#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Print the complete machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Treat portability warnings as failures.
    #[arg(long)]
    pub strict: bool,

    /// Validate metadata without requiring materialized Git checkouts.
    #[arg(long)]
    pub no_checkouts: bool,
}

/// Arguments for guarded Lake build-artifact publication.
#[derive(Debug, Args)]
pub struct PublishArgs {
    /// Existing local Git tag and GitHub release tag.
    #[arg(value_name = "TAG")]
    pub tag: String,

    /// Use existing dependencies after verifying lev.lock.
    #[arg(long)]
    pub no_sync: bool,

    /// Upload existing build outputs without running `lake build`.
    #[arg(long)]
    pub no_build: bool,

    /// Refuse dependency or toolchain downloads before publication.
    #[arg(long)]
    pub offline: bool,

    /// Permit tracked or untracked changes in the Git worktree.
    #[arg(long)]
    pub allow_dirty: bool,

    /// Permit portability warnings such as local path dependencies.
    #[arg(long)]
    pub allow_nonportable: bool,

    /// Run synchronization, build, and release preflights without uploading.
    #[arg(long)]
    pub dry_run: bool,

    /// Arguments passed to the release build.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub build_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct WorkflowArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Arguments passed to the Lake command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct LakeArgs {
    /// Arguments passed to the Lake command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Lean source file to elaborate.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Arguments passed to `lake lean`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct DepsArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TreeArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct WhyArgs {
    /// Package whose dependency path should be shown.
    #[arg(value_name = "PACKAGE")]
    pub package: String,

    /// Print the path as machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct MatrixArgs {
    /// Write a starter [matrix] table to lev.toml and exit.
    #[arg(
        long,
        conflicts_with_all = ["keep_going", "offline", "in_place"]
    )]
    pub init: bool,

    /// Lean toolchain to include. Repeat, or configure matrix.toolchains in lev.toml.
    #[arg(long = "lean", value_name = "TOOLCHAIN")]
    pub toolchains: Vec<String>,

    /// Continue after a command fails.
    #[arg(long)]
    pub keep_going: bool,

    /// Require every toolchain to already be installed.
    #[arg(long)]
    pub offline: bool,

    /// Reuse the source project instead of isolated local workspaces.
    #[arg(long)]
    pub in_place: bool,

    /// Command to run. Defaults to `lake build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct TaskArgs {
    /// Select a locked Lean environment without changing lean-toolchain.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub lean: Option<String>,

    /// Configured task name. Omit to list tasks.
    #[arg(value_name = "TASK")]
    pub name: Option<String>,

    /// Run without synchronizing dependencies first.
    #[arg(long)]
    pub no_sync: bool,

    /// Refuse toolchain and dependency network access.
    #[arg(long)]
    pub offline: bool,

    /// Additional arguments appended to the configured command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
/// Arguments for commands spanning every configured project member.
pub struct WorkspaceArgs {
    #[command(subcommand)]
    pub command: WorkspaceCommand,
}

#[derive(Debug, Subcommand)]
/// Operations over the stable member set declared by `[workspace]`.
pub enum WorkspaceCommand {
    /// List expanded workspace projects and their toolchains.
    List {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Synchronize dependencies and locks for every workspace project.
    Sync {
        /// Refuse all network access.
        #[arg(long)]
        offline: bool,

        /// Ask Lake to update every member manifest.
        #[arg(long, conflicts_with = "offline")]
        update: bool,

        /// Require every member lake-manifest.json to exist.
        #[arg(long)]
        locked: bool,

        /// Verify every member lock before synchronization.
        #[arg(long, conflicts_with = "update")]
        frozen: bool,

        /// Continue synchronizing remaining members after a failure.
        #[arg(long)]
        keep_going: bool,
    },

    /// Generate or verify lev-workspace.lock and all member locks.
    Lock {
        /// Verify without changing lockfiles.
        #[arg(long)]
        check: bool,
    },

    /// Synchronize and build every workspace project.
    Build {
        /// Run without synchronizing members first.
        #[arg(long)]
        no_sync: bool,

        /// Refuse toolchain and dependency network access.
        #[arg(long)]
        offline: bool,

        /// Continue building remaining members after a failure.
        #[arg(long)]
        keep_going: bool,

        /// Recompute file hashes instead of trusting Lake .hash sidecars.
        #[arg(long)]
        rehash: bool,

        /// Arguments passed to each `lake build`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },

    /// Run one command in every workspace project.
    Run {
        /// Run without synchronizing members first.
        #[arg(long)]
        no_sync: bool,

        /// Refuse toolchain and dependency network access.
        #[arg(long)]
        offline: bool,

        /// Continue running remaining members after a failure.
        #[arg(long)]
        keep_going: bool,

        /// Command and arguments executed in each project.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
}
