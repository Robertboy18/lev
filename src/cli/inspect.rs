//! Arguments for `lev inspect`.

use std::ffi::OsString;

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub struct InspectArgs {
    #[command(subcommand)]
    pub command: InspectCommand,
}

#[derive(Debug, Subcommand)]
pub enum InspectCommand {
    /// Run `lake --verbose build` and explain from Lake's traces why each module rebuilt.
    Build(InspectBuildArgs),
}

#[derive(Debug, Args)]
pub struct InspectBuildArgs {
    /// Print a machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Follow the causes of one rebuilt module back to the edit that started them.
    #[arg(long, value_name = "MODULE")]
    pub why: Option<String>,

    /// Arguments passed to `lake build`, usually after `--`.
    #[arg(
        value_name = "LAKE_ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub args: Vec<OsString>,
}
