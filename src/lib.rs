#![forbid(unsafe_code)]

//! Library entry point for the `lev` CLI.
//!
//! The public surface is the parsed CLI, [`run`], and [`check`], the
//! incremental external checking engine.

mod app;
mod cache;
pub mod check;
mod cli;
mod core;
mod project;

use anyhow::Result;

pub use cli::Cli;

/// Parse process arguments, execute one lev command, and return its exit code.
pub fn run() -> Result<i32> {
    app::execute(cli::parse())
}
