#![forbid(unsafe_code)]

//! Library entry point for the `lev` CLI.
//!
//! The public surface is the parsed CLI and [`run`].

mod app;
mod cache;
mod cli;
mod core;
mod dependency;
mod project;
mod toolchain;

use anyhow::Result;

pub use cli::Cli;

/// Parse process arguments, execute one lev command, and return its exit code.
pub fn run() -> Result<i32> {
    app::execute(cli::parse())
}
