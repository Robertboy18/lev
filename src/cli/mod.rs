//! Clap definitions for lev's command-line interface.
//!
//! These types parse arguments and have no side effects.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

mod cache;
mod project;
mod toolchain;
mod tools;

pub use cache::*;
pub use project::*;
pub use toolchain::*;
pub use tools::*;

#[derive(Debug, Parser)]
#[command(
    name = "lev",
    version,
    about = "Fast Lean toolchains, dependencies, and project environments",
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

    /// Override LEV_DATA_DIR for persistent toolchain objects.
    #[arg(long, global = true, value_name = "PATH", hide_short_help = true)]
    pub data_dir: Option<PathBuf>,

    /// Suppress lev's informational output.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Show cache reuse and orchestration details.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Hide progress updates for long operations.
    #[arg(long, global = true, hide_short_help = true)]
    pub no_progress: bool,

    /// Run eligible commands in a persistent local workspace.
    #[arg(long, global = true)]
    pub local: bool,

    /// Print elapsed time for the command and recorded execution phases.
    #[arg(long, global = true)]
    pub timings: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a standard Lake project pinned to a Lean toolchain.
    Init(InitArgs),

    /// Create or update the project environment from its locks.
    Sync(SyncArgs),

    /// Generate or verify base and version-specific dependency locks.
    Lock(LockArgs),

    /// Inspect and change project dependencies.
    Deps(DepsArgs),

    /// Analyze project state and explain recorded evidence.
    Inspect(InspectArgs),

    /// Operate on project pins, exports, archives, and releases.
    #[command(name = "project", hide = true)]
    ProjectState(ProjectArgs),

    /// Add or replace a dependency in lakefile.toml.
    #[command(hide = true)]
    Add(AddArgs),

    /// Remove a dependency from lakefile.toml.
    #[command(hide = true)]
    Remove(RemoveArgs),

    /// Update locked dependencies through Lake.
    #[command(hide = true)]
    Update(UpdateArgs),

    /// Synchronize, then run a command in the project's Lean environment.
    Run(RunArgs),

    /// Synchronize, then run `lake build`.
    Build(BuildArgs),

    /// Synchronize, then run the Lake test driver.
    #[command(hide = true)]
    Test(WorkflowArgs),

    /// Install, list, and remove Lean toolchains.
    Toolchain(ToolchainArgs),

    /// Install and run Lean package executables outside a project.
    #[command(hide = true)]
    Tool(ToolArgs),

    /// Inspect lev's shared caches.
    Cache(CacheArgs),

    /// Check the local Lean, Lake, Git, and cache configuration.
    Doctor(InspectEnvironmentArgs),

    /// Report newer Reservoir releases compatible with this Lean toolchain.
    #[command(hide = true)]
    Outdated(OutdatedArgs),

    /// Upgrade direct dependencies using Reservoir compatibility metadata.
    #[command(hide = true)]
    Upgrade(UpgradeArgs),

    /// Pin the project to a different Lean toolchain.
    #[command(hide = true)]
    Pin(PinArgs),

    /// Make a resolved Lean environment the project's default.
    #[command(hide = true)]
    Use(UseArgs),

    /// Synchronize, then run the Lake lint driver.
    #[command(hide = true)]
    Lint(WorkflowArgs),

    /// Remove project build outputs through Lake.
    #[command(hide = true)]
    Clean(LakeArgs),

    /// Verify a project or elaborate one Lean file.
    Check(CheckArgs),

    /// Infer and optionally apply minimal Lean imports.
    #[command(name = "imports", hide = true)]
    Imports(ShakeArgs),

    /// Infer and optionally apply minimal Lean imports.
    Shake(ShakeArgs),

    /// Measure repeated execution of a command in the Lean environment.
    Profile(LegacyProfileArgs),

    /// Inspect the project's Lean module import graph.
    #[command(hide = true)]
    Graph(GraphArgs),

    /// Verify locks, dependency integrity, builds, and optional policy gates.
    Verify(VerifyArgs),

    /// Create a deterministic archive of the locked project sources.
    #[command(hide = true)]
    Bundle(BundleArgs),

    /// Rerun a command when project files change.
    #[command(hide = true)]
    Watch(WatchArgs),

    /// Print the resolved dependency tree.
    #[command(hide = true)]
    Tree(TreeArgs),

    /// Explain why a package is in the dependency graph.
    #[command(hide = true)]
    Why(WhyArgs),

    /// Run or list commands configured in lev.toml.
    #[command(hide = true)]
    Task(TaskArgs),

    /// Operate on all Lean projects declared in [workspace].
    #[command(hide = true)]
    Workspace(WorkspaceArgs),

    /// Run or check one Lean file with inline dependency metadata.
    #[command(hide = true)]
    Script(ScriptArgs),

    /// Optional CI: run one command against multiple Lean toolchains.
    #[command(hide = true)]
    Matrix(MatrixArgs),

    /// Export the locked dependency inventory or a CycloneDX SBOM.
    #[command(hide = true)]
    Export(ExportArgs),

    /// Audit local lock, toolchain, and dependency checkout integrity.
    #[command(hide = true)]
    Audit(AuditArgs),

    /// Verify and upload Lake build artifacts to an existing GitHub release.
    #[command(hide = true)]
    Publish(PublishArgs),

    /// Inspect, update, or uninstall the lev executable.
    #[command(name = "self", hide = true)]
    SelfManagement(SelfArgs),

    /// Generate shell completion definitions.
    #[command(hide = true)]
    Completions {
        /// Shell whose completion format should be generated.
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// Parse process arguments.
pub(crate) fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command, DepsCommand, InspectCommand, ProjectCommand};

    #[test]
    fn root_help_exposes_command_families_and_familiar_direct_commands() {
        let mut command = Cli::command();
        let visible = command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            [
                "init",
                "sync",
                "lock",
                "deps",
                "inspect",
                "run",
                "build",
                "toolchain",
                "cache",
                "doctor",
                "check",
                "shake",
                "profile",
                "verify",
            ]
        );

        let help = command.render_long_help().to_string();
        for visible in ["doctor", "shake", "profile", "verify"] {
            assert!(help.contains(&format!("  {visible} ")), "{help}");
        }
        for hidden in ["add", "graph", "imports", "workspace"] {
            assert!(!help.contains(&format!("  {hidden} ")), "{help}");
        }
        assert!(!help.contains("  help "), "{help}");
    }

    #[test]
    fn grouped_commands_and_direct_spellings_both_parse() {
        let grouped = Cli::try_parse_from(["lev", "inspect", "performance"]).unwrap();
        assert!(matches!(
            grouped.command,
            Command::Inspect(super::InspectArgs {
                command: Some(InspectCommand::Performance(_)),
                ..
            })
        ));
        let legacy = Cli::try_parse_from(["lev", "profile"]).unwrap();
        assert!(matches!(legacy.command, Command::Profile(_)));

        let grouped =
            Cli::try_parse_from(["lev", "inspect", "trust", "--deny", "sorryAx"]).unwrap();
        assert!(matches!(
            grouped.command,
            Command::Inspect(super::InspectArgs {
                command: Some(InspectCommand::Trust(super::InspectTrustArgs {
                    denied_axioms,
                    ..
                })),
                ..
            }) if denied_axioms == ["sorryAx"]
        ));

        let grouped = Cli::try_parse_from(["lev", "deps", "add", "example"]).unwrap();
        assert!(matches!(
            grouped.command,
            Command::Deps(super::DepsArgs {
                command: Some(DepsCommand::Add(_)),
                ..
            })
        ));
        let legacy = Cli::try_parse_from(["lev", "add", "example"]).unwrap();
        assert!(matches!(legacy.command, Command::Add(_)));

        let grouped = Cli::try_parse_from(["lev", "project", "clean"]).unwrap();
        assert!(matches!(
            grouped.command,
            Command::ProjectState(super::ProjectArgs {
                command: ProjectCommand::Clean(_)
            })
        ));
        let legacy = Cli::try_parse_from(["lev", "clean"]).unwrap();
        assert!(matches!(legacy.command, Command::Clean(_)));

        for args in [
            &["lev", "tool", "list"][..],
            &["lev", "self", "update", "--check"][..],
            &["lev", "completions", "bash"][..],
            &[
                "lev",
                "cache",
                "remote",
                "keygen",
                "--private-key",
                "private.key",
                "--public-key",
                "public.key",
            ][..],
            &[
                "lev",
                "toolchain",
                "chunks",
                "publish",
                "leanprover/lean4:v4.test",
                "remote",
                "--signing-key",
                "private.key",
            ][..],
            &[
                "lev",
                "toolchain",
                "index",
                "list",
                "remote",
                "--public-key",
                "public.key",
            ][..],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn inspect_help_exposes_direct_topics_without_an_extra_code_layer() {
        let mut command = Cli::command();
        let inspect = command
            .find_subcommand_mut("inspect")
            .expect("inspect command");
        let visible = inspect
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            [
                "build",
                "dependencies",
                "imports",
                "graph",
                "declaration",
                "trust",
                "performance",
                "environment"
            ]
        );

        assert!(Cli::try_parse_from(["lev", "inspect", "imports", "--fix"]).is_ok());
        assert!(Cli::try_parse_from(["lev", "inspect", "imports", "--fix", "--isolate"]).is_ok());
        assert!(Cli::try_parse_from(["lev", "shake", "--fix"]).is_ok());
    }

    #[test]
    fn family_and_global_options_remain_usable_around_subcommands() {
        for args in [
            &["lev", "inspect", "--project", "/tmp", "environment"][..],
            &["lev", "deps", "--timings", "list"][..],
            &["lev", "deps", "--json", "list"][..],
            &["lev", "inspect", "--json", "build"][..],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn every_grouped_flat_compatibility_spelling_still_parses() {
        let pairs: &[(&[&str], &[&str])] = &[
            (
                &["lev", "deps", "add", "example"],
                &["lev", "add", "example"],
            ),
            (
                &["lev", "deps", "remove", "example"],
                &["lev", "remove", "example"],
            ),
            (&["lev", "deps", "update"], &["lev", "update"]),
            (&["lev", "deps", "outdated"], &["lev", "outdated"]),
            (&["lev", "deps", "upgrade"], &["lev", "upgrade"]),
            (&["lev", "deps", "tree"], &["lev", "tree"]),
            (
                &["lev", "deps", "why", "example"],
                &["lev", "why", "example"],
            ),
            (&["lev", "inspect", "environment"], &["lev", "doctor"]),
            (&["lev", "inspect", "performance"], &["lev", "profile"]),
            (&["lev", "inspect", "imports"], &["lev", "shake"]),
            (&["lev", "inspect", "imports"], &["lev", "imports"]),
            (&["lev", "inspect", "graph"], &["lev", "graph"]),
            (
                &["lev", "project", "pin", "stable"],
                &["lev", "pin", "stable"],
            ),
            (
                &["lev", "project", "use", "stable"],
                &["lev", "use", "stable"],
            ),
            (&["lev", "project", "clean"], &["lev", "clean"]),
            (&["lev", "project", "export"], &["lev", "export"]),
            (&["lev", "project", "audit"], &["lev", "audit"]),
            (&["lev", "project", "bundle"], &["lev", "bundle"]),
            (
                &["lev", "project", "publish", "v1.0.0"],
                &["lev", "publish", "v1.0.0"],
            ),
        ];

        for (grouped, flat) in pairs {
            assert!(Cli::try_parse_from(*grouped).is_ok(), "{grouped:?}");
            assert!(Cli::try_parse_from(*flat).is_ok(), "{flat:?}");
        }

        for published in [
            &["lev", "verify"][..],
            &["lev", "test"][..],
            &["lev", "lint"][..],
            &["lev", "watch"][..],
            &["lev", "workspace", "list"][..],
        ] {
            assert!(Cli::try_parse_from(published).is_ok(), "{published:?}");
        }
    }

    #[test]
    fn check_parses_explicit_and_legacy_file_modes_and_positional_targets() {
        let project = Cli::try_parse_from(["lev", "check", "--json"]).unwrap();
        assert!(matches!(
            project.command,
            Command::Check(super::CheckArgs { file: None, .. })
        ));

        let file =
            Cli::try_parse_from(["lev", "check", "--file", "Root.lean", "--no-sync"]).unwrap();
        assert!(matches!(
            file.command,
            Command::Check(super::CheckArgs {
                file: Some(_),
                no_sync: true,
                ..
            })
        ));

        let target = Cli::try_parse_from(["lev", "check", "MyLibrary", "--task", "test"]).unwrap();
        assert!(matches!(
            target.command,
            Command::Check(super::CheckArgs {
                file: None,
                first_arg: Some(first_arg),
                tasks,
                ..
            }) if first_arg == "MyLibrary" && tasks == ["test"]
        ));

        let legacy =
            Cli::try_parse_from(["lev", "check", "Root.lean", "--no-sync", "--offline"]).unwrap();
        assert!(matches!(
            legacy.command,
            Command::Check(super::CheckArgs {
                file: None,
                no_sync: true,
                ..
            })
        ));

        let legacy_with_lean_flag =
            Cli::try_parse_from(["lev", "check", "Root.lean", "-Dpp.universes=true"]).unwrap();
        assert!(matches!(
            legacy_with_lean_flag.command,
            Command::Check(super::CheckArgs {
                first_arg: Some(first_arg),
                args,
                ..
            }) if first_arg == "Root.lean" && args == ["-Dpp.universes=true"]
        ));

        let explicit_with_lean_flag =
            Cli::try_parse_from(["lev", "check", "--file", "Root.lean", "-Dpp.universes=true"])
                .unwrap();
        assert!(matches!(
            explicit_with_lean_flag.command,
            Command::Check(super::CheckArgs {
                file: Some(_),
                first_arg: Some(first_arg),
                ..
            }) if first_arg == "-Dpp.universes=true"
        ));
    }
}
