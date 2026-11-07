//! Clap definitions for lev's command-line interface.
//!
//! These types parse arguments and have no side effects.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

mod cache;
mod check;
mod inspect;

pub use cache::*;
pub use check::*;
pub use inspect::*;

#[derive(Debug, Parser)]
#[command(
    name = "lev",
    version,
    about = "Explain Lake rebuilds, check modules with external kernels, prune caches, \
             and report on a Lean setup",
    long_about = None,
    disable_help_subcommand = true,
    after_help = "Run `lev <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Start project discovery from this path.
    #[arg(long, global = true, value_name = "PATH")]
    pub project: Option<PathBuf>,

    /// Override LEV_CACHE_DIR.
    #[arg(long, global = true, value_name = "PATH", hide_short_help = true)]
    pub cache_dir: Option<PathBuf>,

    /// Suppress lev's informational output.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Show the commands lev runs and the caches it reads.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a Lake build and explain what it rebuilt.
    Inspect(InspectArgs),

    /// Check every module with external kernels, reusing the verdicts of
    /// modules whose slices did not change.
    Check(CheckArgs),

    /// Inventory and prune lev's caches and Lake's artifact cache.
    Cache(CacheArgs),

    /// Report the tools, toolchain, and caches a build or check depends on.
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Print a machine-readable report.
    #[arg(long)]
    pub json: bool,
}

/// Parse process arguments.
pub(crate) fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{CacheCommand, CheckKernel, Cli, Command, InspectCommand};

    #[test]
    fn root_help_lists_exactly_the_kept_commands() {
        let mut command = Cli::command();
        let names = command
            .get_subcommands()
            .map(|subcommand| subcommand.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["inspect", "check", "cache", "doctor"]);

        let help = command.render_long_help().to_string();
        for name in ["inspect", "check", "cache", "doctor"] {
            assert!(help.contains(&format!("  {name} ")), "{help}");
        }
        assert!(!help.contains("  help "), "{help}");
    }

    #[test]
    fn inspect_and_cache_have_one_spelling_per_command() {
        let mut command = Cli::command();
        let inspect = command
            .find_subcommand_mut("inspect")
            .expect("inspect command");
        let topics = inspect
            .get_subcommands()
            .map(|subcommand| subcommand.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(topics, ["build"]);

        let cache = command.find_subcommand_mut("cache").expect("cache command");
        let topics = cache
            .get_subcommands()
            .map(|subcommand| subcommand.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(topics, ["status", "gc"]);
    }

    #[test]
    fn inspect_build_passes_lake_arguments_through() {
        let parsed = Cli::try_parse_from([
            "lev", "inspect", "build", "--json", "--why", "Root.B", "--", "--old", "Root",
        ])
        .unwrap();
        let Command::Inspect(inspect) = parsed.command else {
            panic!("expected inspect");
        };
        let InspectCommand::Build(build) = inspect.command;
        assert!(build.json);
        assert_eq!(build.why.as_deref(), Some("Root.B"));
        assert_eq!(build.args, ["--old", "Root"]);

        let parsed = Cli::try_parse_from(["lev", "inspect", "build", "Root"]).unwrap();
        let Command::Inspect(inspect) = parsed.command else {
            panic!("expected inspect");
        };
        let InspectCommand::Build(build) = inspect.command;
        assert_eq!(build.args, ["Root"]);
    }

    #[test]
    fn check_takes_kernels_jobs_and_modules() {
        let parsed = Cli::try_parse_from(["lev", "check"]).unwrap();
        let Command::Check(check) = parsed.command else {
            panic!("expected check");
        };
        assert!(check.kernels.is_empty());
        assert_eq!(check.jobs, None);
        assert_eq!(check.timeout, 3600);
        assert!(!check.no_build && !check.no_sandbox && !check.json);

        let parsed = Cli::try_parse_from([
            "lev",
            "check",
            "--kernel",
            "official",
            "--kernel",
            "nanoda",
            "--jobs",
            "4",
            "--no-build",
            "--no-sandbox",
            "--json",
            "--only",
            "A.B",
            "--only",
            "C",
            "--timeout",
            "60",
        ])
        .unwrap();
        let Command::Check(check) = parsed.command else {
            panic!("expected check");
        };
        assert_eq!(check.kernels, [CheckKernel::Official, CheckKernel::Nanoda]);
        assert_eq!(check.jobs, Some(4));
        assert_eq!(check.only, ["A.B", "C"]);
        assert_eq!(check.timeout, 60);
        assert!(check.no_build && check.no_sandbox && check.json);

        for args in [
            &["lev", "check", "--kernel", "lean4lean"][..],
            &["lev", "check", "--jobs", "0"][..],
            &["lev", "check", "Mathlib"][..],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn cache_gc_defaults_to_a_dry_run_with_a_thirty_day_gate() {
        let parsed = Cli::try_parse_from(["lev", "cache", "gc"]).unwrap();
        let Command::Cache(cache) = parsed.command else {
            panic!("expected cache");
        };
        let CacheCommand::Gc(gc) = cache.command else {
            panic!("expected gc");
        };
        assert_eq!(gc.max_age_days, 30);
        assert!(!gc.apply);
        assert!(!gc.json);
        // Lake's caches are collected only when asked for.
        assert!(!gc.lake);
        let parsed = Cli::try_parse_from(["lev", "cache", "gc", "--lake"]).unwrap();
        let Command::Cache(cache) = parsed.command else {
            panic!("expected cache");
        };
        let CacheCommand::Gc(gc) = cache.command else {
            panic!("expected gc");
        };
        assert!(gc.lake);
    }

    #[test]
    fn global_options_work_around_subcommands() {
        for args in [
            &["lev", "--project", "/tmp", "doctor", "--json"][..],
            &["lev", "doctor", "--project", "/tmp", "-v"][..],
            &["lev", "cache", "--cache-dir", "/tmp/cache", "status"][..],
            &["lev", "-q", "inspect", "build"][..],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
        assert!(Cli::try_parse_from(["lev", "-q", "-v", "doctor"]).is_err());
    }

    #[test]
    fn deleted_commands_and_flags_no_longer_parse() {
        for args in [
            &["lev", "sync"][..],
            &["lev", "build"][..],
            &["lev", "shake"][..],
            &["lev", "inspect", "graph"][..],
            &["lev", "inspect", "environment"][..],
            &["lev", "cache", "dir"][..],
            &["lev", "--local", "doctor"][..],
            &["lev", "--timings", "doctor"][..],
            &["lev", "doctor", "--output", "report.json"][..],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }
}
