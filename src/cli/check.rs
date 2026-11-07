//! Arguments for `lev check`.

use clap::{Args, ValueEnum};

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Kernel that must accept every module; repeat to require several.
    /// Defaults to the official kernel.
    #[arg(long = "kernel", value_enum, value_name = "KERNEL")]
    pub kernels: Vec<CheckKernel>,

    /// Exporter threads and parallel kernel runs (default: CPUs, at most 16).
    #[arg(long, value_name = "N", value_parser = parse_jobs)]
    pub jobs: Option<usize>,

    /// Check the current build instead of running `lake build` first.
    #[arg(long)]
    pub no_build: bool,

    /// Run the exporter and the kernels without the bwrap sandbox.
    #[arg(long)]
    pub no_sandbox: bool,

    /// Print a machine-readable report.
    #[arg(long)]
    pub json: bool,

    /// Check only this module and what its slice relies on; repeatable.
    #[arg(long, value_name = "MODULE")]
    pub only: Vec<String>,

    /// Time limit of one kernel run, in seconds.
    #[arg(long, value_name = "SECS", default_value_t = 3600)]
    pub timeout: u64,
}

/// The kernels `lev check` runs. lean4lean rejects most slices, so it is
/// not offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CheckKernel {
    /// `leanchecker`, Lean's C++ kernel.
    Official,
    /// `leanchecker-paranoid`, the same kernel linked statically.
    Paranoid,
    /// `nanoda_bin`, an independent kernel in Rust.
    Nanoda,
}

fn parse_jobs(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(jobs) if jobs > 0 => Ok(jobs),
        _ => Err(format!("expected a positive number of jobs, got {value:?}")),
    }
}
