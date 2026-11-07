//! Arguments for `lev cache`.

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Inventory lev's cache and the Lake caches of registered projects.
    ///
    /// Exits with status 1 when a local Lake mapping names a missing artifact.
    Status(CacheStatusArgs),

    /// Report or remove lev's unused cache entries, and with --lake the Lake
    /// cache entries that no registered project references.
    Gc(CacheGcArgs),
}

#[derive(Debug, Args)]
pub struct CacheStatusArgs {
    /// Print a machine-readable report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct CacheGcArgs {
    /// Keep unreferenced entries younger than this many days.
    #[arg(long, default_value_t = 30, value_name = "N")]
    pub max_age_days: u64,

    /// Also collect the Lake artifact caches of registered projects: the
    /// mappings and artifacts that no registered project's traces reference.
    #[arg(long)]
    pub lake: bool,

    /// Delete the reported entries instead of only listing them.
    #[arg(long)]
    pub apply: bool,

    /// Print a machine-readable report.
    #[arg(long)]
    pub json: bool,
}
