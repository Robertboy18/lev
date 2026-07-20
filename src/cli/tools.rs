//! Arguments for tool, inline-script, and self-management commands.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct ToolArgs {
    #[command(subcommand)]
    pub command: ToolCommand,
}

#[derive(Debug, Args)]
pub struct ScriptArgs {
    #[command(subcommand)]
    pub command: ScriptCommand,
}

#[derive(Debug, Args)]
pub struct SelfArgs {
    #[command(subcommand)]
    pub command: SelfCommand,
}

#[derive(Debug, Subcommand)]
pub enum SelfCommand {
    /// Check for or install a released lev binary.
    Update(SelfUpdateArgs),

    /// Remove the running lev executable while retaining cache and data.
    Uninstall(SelfUninstallArgs),
}

#[derive(Debug, Args)]
pub struct SelfUpdateArgs {
    /// GitHub repository in `owner/name` form. Normally embedded in release builds.
    #[arg(long, value_name = "OWNER/REPOSITORY")]
    pub repository: Option<String>,

    /// Install a specific release instead of the latest release.
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,

    /// Report the selected release without downloading it.
    #[arg(long)]
    pub check: bool,

    /// Reinstall even when the selected release matches this executable.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct SelfUninstallArgs {
    /// Show the executable and removal method without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum ScriptCommand {
    /// Execute a Lean `main` in its cached inline environment.
    Run(ScriptRunArgs),

    /// Elaborate a Lean file in its cached inline environment.
    Check(ScriptCheckArgs),
}

#[derive(Debug, Args)]
pub struct ScriptRunArgs {
    #[command(flatten)]
    pub script: ScriptSourceArgs,

    /// Arguments passed to the Lean program.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct ScriptCheckArgs {
    #[command(flatten)]
    pub script: ScriptSourceArgs,
}

#[derive(Debug, Args)]
pub struct ScriptSourceArgs {
    /// Lean source file containing a `-- /// lev` metadata block.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Override the Lean version declared by the script.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub lean: Option<String>,

    /// Reuse only an already materialized script environment.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Subcommand)]
pub enum ToolCommand {
    /// Install a package executable under a stable user-level name.
    Install(ToolInstallArgs),

    /// Run an installed tool or a cached one-shot package executable.
    Run(ToolRunArgs),

    /// List installed user-level tools.
    List {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Remove an installed tool alias while retaining reusable cache data.
    Remove {
        /// Installed tool name.
        #[arg(value_name = "NAME")]
        name: String,
    },

    /// Report or remove old tool environments without installed aliases.
    Gc {
        /// Minimum age of unreferenced environments.
        #[arg(long, default_value_t = 30, value_name = "DAYS")]
        max_age_days: u64,

        /// Delete reported environments.
        #[arg(long)]
        apply: bool,

        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
pub struct ToolInstallArgs {
    #[command(flatten)]
    pub source: ToolSourceArgs,

    /// Stable installed name. Defaults to the executable name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Reuse only an already materialized tool environment.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Args)]
pub struct ToolRunArgs {
    #[command(flatten)]
    pub source: ToolSourceArgs,

    /// Refuse toolchain and dependency network access.
    #[arg(long)]
    pub offline: bool,

    /// Arguments passed to the package executable.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct ToolSourceArgs {
    /// Package name, or an installed tool name for `tool run`.
    #[arg(value_name = "PACKAGE")]
    pub package: String,

    /// Git repository URL instead of a Reservoir package.
    #[arg(long, value_name = "URL", conflicts_with = "scope")]
    pub git: Option<String>,

    /// Reservoir package scope.
    #[arg(long, value_name = "SCOPE", conflicts_with = "git")]
    pub scope: Option<String>,

    /// Requested Git revision, tag, or branch.
    #[arg(long, value_name = "REV")]
    pub rev: Option<String>,

    /// Lean toolchain. Defaults to the nearest project's toolchain.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub lean: Option<String>,

    /// Lake executable name. Defaults to the package name.
    #[arg(long, value_name = "EXE")]
    pub exe: Option<String>,
}
