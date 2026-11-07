//! Command dispatch and the context every command shares.
//!
//! lev runs Lean and Lake through elan, the way a plain `lake build` in the
//! project would, and never changes Lake's environment.

mod cache;
mod check;
mod doctor;
mod exporter;
mod inspect_build;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::cache::CacheLayout;
use crate::cache::lake::{self, Location};
use crate::cli::{CacheCommand, Cli, Command as CliCommand, InspectCommand};
use crate::core::process::{checked_output, output_text};
use crate::project::{Project, absolute};

/// Settings shared by every command.
struct AppContext {
    cache: CacheLayout,
    project_start: PathBuf,
    elan: OsString,
    git: OsString,
    quiet: bool,
    verbose: bool,
}

/// Build the context and run one parsed command.
pub fn execute(cli: Cli) -> Result<i32> {
    let project_start = match cli.project {
        Some(path) => absolute(&path)?,
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let context = AppContext {
        cache: CacheLayout::resolve(cli.cache_dir)?,
        project_start,
        elan: std::env::var_os("LEV_ELAN").unwrap_or_else(|| OsString::from("elan")),
        git: std::env::var_os("LEV_GIT").unwrap_or_else(|| OsString::from("git")),
        quiet: cli.quiet,
        verbose: cli.verbose,
    };

    match cli.command {
        CliCommand::Inspect(args) => match args.command {
            InspectCommand::Build(args) => inspect_build::run(&context, args),
        },
        CliCommand::Check(args) => check::run(&context, args),
        CliCommand::Cache(args) => match args.command {
            CacheCommand::Status(args) => cache::status(&context, args),
            CacheCommand::Gc(args) => cache::gc(&context, args),
        },
        CliCommand::Doctor(args) => doctor::run(&context, args),
    }
}

impl AppContext {
    /// The nearest Lean project from the starting path.
    fn project(&self) -> Result<Project> {
        Project::discover(&self.project_start)
    }

    /// `elan run [--install] TOOLCHAIN PROGRAM`.
    fn elan_run(&self, toolchain: &str, program: &str, install: bool) -> Command {
        let mut command = Command::new(&self.elan);
        command.arg("run");
        if install {
            command.arg("--install");
        }
        command.arg(toolchain).arg(program);
        command
    }

    /// The directory elan installed `toolchain` in, read from `elan which lean`
    /// without starting Lean.
    fn toolchain_root(&self, toolchain: &str) -> Result<PathBuf> {
        let mut command = Command::new(&self.elan);
        command
            .arg("which")
            .arg("lean")
            .env("ELAN_TOOLCHAIN", toolchain);
        let lean = PathBuf::from(output_text(checked_output(&mut command)?));
        lean.parent()
            .and_then(|bin| bin.parent())
            .map(PathBuf::from)
            .with_context(|| {
                format!(
                    "elan returned an unexpected Lean path for {toolchain}: {}",
                    lean.display()
                )
            })
    }

    /// Where Lake keeps the artifact cache for builds of `project`.
    fn lake_cache(&self, project: &Project) -> Result<Location> {
        lake::locate(std::env::var_os("LAKE_CACHE_DIR"), &project.root, || {
            self.toolchain_root(&project.toolchain)
        })
    }

    /// A note for the user, unless `--quiet` is set.
    fn info(&self, message: impl AsRef<str>) {
        if !self.quiet {
            eprintln!("lev: {}", message.as_ref());
        }
    }

    /// A detail shown only with `--verbose`.
    fn detail(&self, message: impl AsRef<str>) {
        if self.verbose {
            eprintln!("lev: {}", message.as_ref());
        }
    }
}
