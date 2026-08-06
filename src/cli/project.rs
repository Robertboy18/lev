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
    #[arg(long, conflicts_with = "check")]
    pub dry_run: bool,

    /// Resolve and build the proposed upgrade in an isolated workspace.
    #[arg(long)]
    pub check: bool,

    /// Run the Lake test driver after an isolated upgrade check builds.
    #[arg(long, requires = "check")]
    pub test: bool,

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

#[derive(Debug, Args, Clone)]
pub struct ShakeArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Check for import drift without changing source files.
    #[arg(long, conflicts_with = "fix")]
    pub check: bool,

    /// Apply suggestions, then rebuild and roll back if validation fails.
    #[arg(long, visible_alias = "apply")]
    pub fix: bool,

    /// Keep independently verified edits when part of an applied batch fails.
    #[arg(long, requires = "fix")]
    pub isolate: bool,

    /// Print a stable machine-readable report.
    #[arg(long, conflicts_with = "sarif")]
    pub json: bool,

    /// Write a SARIF 2.1.0 report (`-` for standard output).
    #[arg(long, value_name = "PATH")]
    pub sarif: Option<PathBuf>,

    /// Classify project files by import-analysis coverage.
    #[arg(long)]
    pub coverage: bool,

    /// Do not automatically build stale modules before analysis.
    #[arg(long)]
    pub no_build: bool,

    /// Analyze stale .olean files without Lake's freshness check.
    #[arg(long)]
    pub force: bool,

    /// Preserve imports already implied by another import.
    #[arg(long)]
    pub keep_implied: bool,

    /// Prefer broader parent modules over their specific submodules.
    #[arg(long)]
    pub keep_prefix: bool,

    /// Preserve public imports even when the current module does not use them.
    #[arg(long)]
    pub keep_public: bool,

    /// Keep replacements public when the original closure was public.
    #[arg(long)]
    pub add_public: bool,

    /// Explain which declarations or compile-time dependencies require imports.
    #[arg(long)]
    pub explain: bool,

    /// Emit GitHub problem-matcher diagnostics.
    #[arg(long)]
    pub gh_style: bool,

    /// Limit edits to this module while still analyzing its dependency graph.
    #[arg(long = "only", value_name = "MODULE")]
    pub only_modules: Vec<String>,

    /// Show Lake's detailed import-selection decisions.
    #[arg(long, hide = true)]
    pub trace: bool,

    /// Root modules to analyze. Defaults to the package's default targets.
    #[arg(value_name = "MODULE")]
    pub modules: Vec<String>,
}

#[derive(Debug, Args, Clone)]
pub struct ProfileArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Number of measured command executions.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub repeat: u32,

    /// Unmeasured executions performed before recording samples.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=10_000))]
    pub warmup: u32,

    /// Print a stable machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Write the report to a file instead of standard output.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Measure every project Lean file after prebuilding its dependencies.
    #[arg(long, conflicts_with = "command")]
    pub files: bool,

    /// Compare file timings with a previously saved baseline.
    #[arg(long, value_name = "PATH", requires = "files")]
    pub baseline: Option<PathBuf>,

    /// Save successful file timings as a reusable baseline.
    #[arg(long, value_name = "PATH", requires = "files")]
    pub save_baseline: Option<PathBuf>,

    /// Fail when any file exceeds its baseline by this percentage.
    #[arg(
        long,
        value_name = "PERCENT",
        requires = "baseline",
        value_parser = nonnegative_f64
    )]
    pub max_regression: Option<f64>,

    /// Command to measure. Defaults to `lake build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

/// Arguments retained for the published `lev profile` command contract.
#[derive(Debug, Args, Clone)]
pub struct LegacyProfileArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Number of measured command executions.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub repeat: u32,

    /// Unmeasured executions performed before recording samples.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=10_000))]
    pub warmup: u32,

    /// Print a stable machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Write the report to a file instead of standard output.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Command to measure. Defaults to `lake build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

impl From<LegacyProfileArgs> for ProfileArgs {
    fn from(args: LegacyProfileArgs) -> Self {
        Self {
            execution: args.execution,
            repeat: args.repeat,
            warmup: args.warmup,
            json: args.json,
            output: args.output,
            files: false,
            baseline: None,
            save_baseline: None,
            max_regression: None,
            command: args.command,
        }
    }
}

fn nonnegative_f64(value: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("{value:?} is not a number"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err("value must be a finite, non-negative number".to_owned());
    }
    Ok(parsed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GraphFormat {
    /// Human-readable module and import summary.
    Text,
    /// Stable JSON envelope.
    Json,
    /// Graphviz DOT.
    Dot,
}

#[derive(Debug, Args)]
pub struct GraphArgs {
    /// Root module or Lean file. Repeat to select several roots.
    #[arg(value_name = "MODULE")]
    pub roots: Vec<String>,

    /// Include imports whose source is outside this project.
    #[arg(long)]
    pub external: bool,

    /// Output representation.
    #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
    pub format: GraphFormat,

    /// Write output to this file instead of standard output.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct VerifyArgs {
    /// Require all dependencies and the selected toolchain to be local.
    #[arg(long)]
    pub offline: bool,

    /// Run `lake clean` before building.
    #[arg(long)]
    pub clean: bool,

    /// Run the Lake test driver after the build.
    #[arg(long)]
    pub test: bool,

    /// Run the Lake lint driver after the build.
    #[arg(long)]
    pub lint: bool,

    /// Fail when Lake's import minimizer reports source drift.
    #[arg(long)]
    pub imports: bool,

    /// Treat audit portability warnings as failures.
    #[arg(long)]
    pub strict: bool,

    /// Print a stable machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Arguments passed to `lake build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub build_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct BundleArgs {
    /// Destination archive. Defaults to `<project>.lev.tar.zst`.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Replace an existing destination archive.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args, Clone)]
pub struct WatchArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Wait this many milliseconds after the last change before rerunning.
    #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(10..=60_000))]
    pub debounce_ms: u64,

    /// Wait for a change before the first execution.
    #[arg(long)]
    pub no_initial: bool,

    /// Stop watching when the command exits unsuccessfully.
    #[arg(long)]
    pub exit_on_failure: bool,

    /// Stop after this many command executions.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
    pub max_runs: Option<u32>,

    /// Command to run. Defaults to `lake build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct LakeArgs {
    /// Arguments passed to the Lake command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Elaborate one Lean source file instead of checking the project.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = [
            "clean",
            "tasks",
            "imports",
            "strict",
            "denied_axioms",
            "deny_unsafe",
            "json"
        ]
    )]
    pub file: Option<PathBuf>,

    /// Select a locked Lean environment without changing lean-toolchain.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub lean: Option<String>,

    /// Elaborate the selected file without synchronizing dependencies first.
    #[arg(long)]
    pub no_sync: bool,

    /// Require all dependencies and the selected toolchain to be local.
    #[arg(long)]
    pub offline: bool,

    /// Run `lake clean` before a project build.
    #[arg(long)]
    pub clean: bool,

    /// Run this named `[tasks]` command after building. May be repeated.
    #[arg(long = "task", value_name = "NAME", conflicts_with = "file")]
    pub tasks: Vec<String>,

    /// Fail when Lake's import minimizer reports source drift.
    #[arg(long, conflicts_with = "file")]
    pub imports: bool,

    /// Treat audit portability warnings as project-check failures.
    #[arg(long, conflicts_with = "file")]
    pub strict: bool,

    /// Fail when a declaration depends on this axiom. May be repeated.
    #[arg(long = "deny", value_name = "AXIOM", conflicts_with = "file")]
    pub denied_axioms: Vec<String>,

    /// Fail when the project contains an unsafe declaration.
    #[arg(long, conflicts_with = "file")]
    pub deny_unsafe: bool,

    /// Print a stable machine-readable project report.
    #[arg(long, conflicts_with = "file")]
    pub json: bool,

    /// Lake build targets, or a legacy positional Lean file and its arguments.
    #[arg(value_name = "TARGET_OR_FILE")]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct DepsListArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Dependency inventory and mutation commands.
#[derive(Debug, Args)]
pub struct DepsArgs {
    #[command(subcommand)]
    pub command: Option<DepsCommand>,

    /// Print the default dependency listing as machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum DepsCommand {
    /// List dependencies recorded in lake-manifest.json.
    List(DepsListArgs),

    /// Add or replace a dependency in lakefile.toml.
    Add(AddArgs),

    /// Remove a dependency from lakefile.toml.
    Remove(RemoveArgs),

    /// Update locked dependencies through Lake.
    Update(UpdateArgs),

    /// Report newer Reservoir releases compatible with this Lean toolchain.
    Outdated(OutdatedArgs),

    /// Upgrade direct dependencies using Reservoir compatibility metadata.
    Upgrade(UpgradeArgs),

    /// Print the resolved dependency tree.
    Tree(TreeArgs),

    /// Explain why a package is in the dependency graph.
    Why(WhyArgs),
}

/// Project analysis, explanations, and explicit repair modes.
#[derive(Debug, Args)]
pub struct InspectArgs {
    #[command(subcommand)]
    pub command: Option<InspectCommand>,

    /// Print the project diagnosis as machine-readable JSON.
    #[arg(long)]
    pub json: bool,

    /// Write the project diagnosis to a file.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum InspectCommand {
    /// Explain build work, failures, traces, and cache decisions.
    Build(InspectBuildArgs),

    /// Explain selected revisions and transitive dependency pin disagreements.
    #[command(visible_alias = "deps")]
    Dependencies(InspectDependenciesArgs),

    /// Infer, check, or apply minimal Lean imports.
    Imports(ShakeArgs),

    /// Inspect the project's Lean module import graph.
    Graph(GraphArgs),

    /// Find the module, source, import, and type of a Lean declaration.
    #[command(visible_alias = "decl")]
    Declaration(InspectDeclarationArgs),

    /// Report axioms and unsafe declarations across the project.
    Trust(InspectTrustArgs),

    /// Measure commands or project Lean files.
    Performance(ProfileArgs),

    /// Check local tools, paths, project files, caches, and workspace state.
    #[command(alias = "doctor")]
    Environment(InspectEnvironmentArgs),
}

#[derive(Debug, Args)]
pub struct InspectEnvironmentArgs {
    /// Print the environment diagnosis as machine-readable JSON.
    #[arg(long)]
    pub json: bool,

    /// Write the diagnosis to a file instead of standard output.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct InspectDependenciesArgs {
    /// Print a stable machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Write the report to a file instead of standard output.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct InspectBuildArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Print a stable machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Write the report to a file instead of standard output.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Arguments passed to `lake build`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct InspectDeclarationArgs {
    /// Fully qualified declaration name.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Import this module instead of discovering it from built .ilean files.
    #[arg(long, value_name = "MODULE")]
    pub from: Option<String>,

    /// Build the project before searching declaration metadata.
    #[arg(long)]
    pub build: bool,

    /// Require all dependencies and the selected toolchain to be local.
    #[arg(long)]
    pub offline: bool,

    /// Print a stable machine-readable report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InspectTrustArgs {
    #[command(flatten)]
    pub execution: ExecutionArgs,

    /// Inspect existing .ilean metadata without building first.
    #[arg(long)]
    pub no_build: bool,

    /// Fail when a declaration depends on this axiom. May be repeated.
    #[arg(long = "deny", value_name = "AXIOM")]
    pub denied_axioms: Vec<String>,

    /// Fail when the project contains an unsafe declaration.
    #[arg(long)]
    pub deny_unsafe: bool,

    /// Print a stable machine-readable report.
    #[arg(long)]
    pub json: bool,
}

/// Less frequent project state and release operations.
#[derive(Debug, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Pin the project to a different Lean toolchain.
    Pin(PinArgs),

    /// Make a resolved Lean environment the project's default.
    Use(UseArgs),

    /// Remove project build outputs through Lake.
    Clean(LakeArgs),

    /// Export the locked dependency inventory or a CycloneDX SBOM.
    Export(ExportArgs),

    /// Audit local lock, toolchain, and dependency checkout integrity.
    Audit(AuditArgs),

    /// Create a deterministic archive of the locked project sources.
    Bundle(BundleArgs),

    /// Verify and upload Lake build artifacts to an existing GitHub release.
    Publish(PublishArgs),
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

#[derive(Debug, Args)]
pub struct WorkspaceShakeArgs {
    #[command(flatten)]
    pub shake: ShakeArgs,

    /// Continue minimizing remaining members after a failure.
    #[arg(long)]
    pub keep_going: bool,
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

    /// Analyze or minimize imports in every workspace project.
    Shake(WorkspaceShakeArgs),
}
