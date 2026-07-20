//! Project creation, inspection, export, audit, and release commands.
//!
//! Lake and elan still perform initialization, builds, and uploads.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cache::registry;
use crate::cli::{
    AuditArgs, DepsArgs, ExportArgs, InitArgs, LockArgs, ProjectExportFormat, PublishArgs,
    SyncArgs, TreeArgs, UseArgs, WhyArgs,
};
use crate::core::json_output::{self, schema};
use crate::core::process::{checked_output, checked_status, output_text};
use crate::dependency::graph::DependencyGraph;
use crate::project::audit::{self as project_audit, AuditLevel};
use crate::project::config::LevConfig;
use crate::project::export::{self as project_export, ExportFormat};
use crate::project::lockfile;
use crate::project::manifest::LakeManifest;
use crate::project::{Project, absolute};
use crate::toolchain;

use super::AppContext;
use super::environment_commands;
use super::execution_commands::run_in_project;
use super::sync_commands::sync_execution;
use super::toolchain_commands::{
    ensure_toolchain, ensure_toolchain_name, verify_toolchain_runnable,
};
use crate::core::atomic_file::{create as atomic_create, replace as atomic_write};

use super::transaction::FileTransaction;

/// Initialize a standard Lake project under one normalized Lean toolchain.
pub(super) fn init(context: &AppContext, args: InitArgs) -> Result<i32> {
    let target = absolute(&args.path)?;
    if target.join("lean-toolchain").exists()
        || target.join("lakefile.toml").exists()
        || target.join("lakefile.lean").exists()
    {
        bail!(
            "{} already contains a Lean project; refusing to overwrite it",
            target.display()
        );
    }
    fs::create_dir_all(&target)
        .with_context(|| format!("failed to create {}", target.display()))?;

    let toolchain = toolchain::normalize(&args.lean)?;
    ensure_toolchain_name(context, &toolchain, true)?;
    let package_name = args.name.unwrap_or_else(|| ".".to_owned());
    let mut command = context.runtime_command(&toolchain, OsStr::new("lake"), true)?;
    command
        .arg("init")
        .arg(package_name)
        .arg(args.template)
        .current_dir(&target);
    checked_status(&mut command)?;
    atomic_write(
        &target.join("lean-toolchain"),
        format!("{toolchain}\n").as_bytes(),
    )?;
    context.info(format!(
        "initialized {} with {}",
        target.display(),
        toolchain
    ));
    Ok(0)
}

/// Create or verify lev's digest lock for the current Lake project state.
pub(super) fn lock(context: &AppContext, args: LockArgs) -> Result<i32> {
    let project = context.project()?;
    let toolchains = normalize_toolchains(&args.toolchains)?;
    if toolchains.is_empty() {
        if args.offline || args.refresh {
            bail!("`lev lock --offline/--refresh` requires at least one --lean toolchain");
        }
        if args.check {
            lockfile::verify(&project)?;
            context.info(format!(
                "lockfile is current: {}",
                project.lock_path().display()
            ));
        } else {
            lockfile::refresh(&project)?;
            context.info(format!("wrote {}", project.lock_path().display()));
        }
        return Ok(0);
    }

    if args.check {
        lockfile::verify(&project)?;
        for toolchain in &toolchains {
            environment_commands::verify_locked(&project, toolchain)?;
        }
        context.info(format!(
            "{} and {} version environment{} are current",
            project.lock_path().display(),
            toolchains.len(),
            if toolchains.len() == 1 { "" } else { "s" }
        ));
        return Ok(0);
    }

    // Publish all requested environments together or restore the old lock.
    let lock_path = project.lock_path();
    let mut transaction = FileTransaction::capture([&lock_path])?;
    for toolchain in &toolchains {
        environment_commands::ensure_locked(
            context,
            &project,
            toolchain,
            args.offline,
            args.refresh,
            true,
        )?;
    }
    transaction.commit();
    context.info(format!("wrote {}", project.lock_path().display()));
    Ok(0)
}

fn normalize_toolchains(selectors: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut toolchains = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let toolchain = toolchain::normalize(selector)?;
        if seen.insert(toolchain.clone()) {
            toolchains.push(toolchain);
        }
    }
    Ok(toolchains)
}

/// Promote one isolated version lock into the project's standard Lake files.
///
/// This is the persistent counterpart to `build --lean`: after the
/// transaction, direct `lake` and `lean` commands see the selected toolchain
/// and dependency graph without lev. Both Lakefile variants are captured so a
/// configured executable alternative can safely replace a TOML file or vice
/// versa, with complete rollback on any later failure.
pub(super) fn use_environment(context: &AppContext, args: UseArgs) -> Result<i32> {
    let source = context.project()?;
    let environment =
        environment_commands::select(context, &source, &args.toolchain, args.offline, true)?;
    ensure_toolchain(context, &environment, !args.offline)?;

    let environment_toml = environment.root.join("lakefile.toml");
    let environment_lean = environment.root.join("lakefile.lean");
    let (effective, destination, obsolete) = if environment_toml.is_file() {
        (
            environment_toml,
            source.root.join("lakefile.toml"),
            source.root.join("lakefile.lean"),
        )
    } else if environment_lean.is_file() {
        (
            environment_lean,
            source.root.join("lakefile.lean"),
            source.root.join("lakefile.toml"),
        )
    } else {
        bail!(
            "resolved environment {} contains no Lakefile",
            environment.root.display()
        );
    };
    let toolchain_file = source.root.join("lean-toolchain");
    let manifest = source.manifest_path();
    let lock = source.lock_path();
    let mut transaction =
        FileTransaction::capture([&destination, &obsolete, &toolchain_file, &manifest, &lock])?;

    atomic_write(
        &destination,
        &fs::read(&effective).with_context(|| format!("failed to read {}", effective.display()))?,
    )?;
    if obsolete.exists() {
        fs::remove_file(&obsolete)
            .with_context(|| format!("failed to remove {}", obsolete.display()))?;
    }
    atomic_write(
        &toolchain_file,
        format!("{}\n", environment.toolchain).as_bytes(),
    )?;
    atomic_write(
        &manifest,
        &fs::read(environment.manifest_path())
            .with_context(|| format!("failed to read {}", environment.manifest_path().display()))?,
    )?;

    let selected = Project::load(source.root.clone())?;
    lockfile::refresh(&selected)?;
    registry::record(&context.cache, &selected)?;
    transaction.commit();
    context.info(format!(
        "using {} as the default environment for {}",
        selected.toolchain,
        selected.root.display()
    ));
    Ok(0)
}

/// Export verified lock state.
pub(super) fn export_project(context: &AppContext, args: ExportArgs) -> Result<i32> {
    let project = context.project()?;
    let format = match args.format {
        ProjectExportFormat::Lev => ExportFormat::LevJson,
        ProjectExportFormat::Cyclonedx => ExportFormat::CycloneDxJson,
    };
    let contents = project_export::render(&project, format)?;
    match args.output.as_deref() {
        None => io::stdout()
            .write_all(&contents)
            .context("failed to write project export")?,
        Some(path) if path == Path::new("-") => io::stdout()
            .write_all(&contents)
            .context("failed to write project export")?,
        Some(path) => {
            let destination = absolute(path)?;
            if destination.exists() && !args.force {
                bail!(
                    "{} already exists; pass --force to replace it",
                    destination.display()
                );
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            if args.force {
                atomic_write(&destination, &contents)?;
            } else {
                atomic_create(&destination, &contents)?;
            }
            context.info(format!("wrote {}", destination.display()));
        }
    }
    Ok(0)
}

/// Inspect locks, checkouts, and the selected toolchain without mutation.
pub(super) fn audit_project(context: &AppContext, args: AuditArgs) -> Result<i32> {
    let project = context.project()?;
    let mut report =
        project_audit::AuditReport::inspect(&project, &context.git, !args.no_checkouts);
    match verify_toolchain_runnable(context, &project.toolchain) {
        Ok(()) => report.pass(
            "toolchain",
            &project.toolchain,
            "selected Lean toolchain is installed and runnable",
        ),
        Err(error) => report.error(
            "toolchain",
            &project.toolchain,
            format!("selected Lean toolchain is unavailable: {error:#}"),
        ),
    }

    if args.json {
        json_output::print(schema::AUDIT, &report)?;
    } else {
        for finding in &report.findings {
            let level = match finding.level {
                AuditLevel::Pass => "PASS",
                AuditLevel::Warning => "WARN",
                AuditLevel::Error => "ERROR",
            };
            println!(
                "{level}\t{}\t{}\t{}",
                finding.check, finding.subject, finding.message
            );
        }
        println!(
            "audit: {} passed, {} warnings, {} errors",
            report.summary.passed, report.summary.warnings, report.summary.errors
        );
    }
    Ok(report.exit_code(args.strict))
}

/// Check release state before delegating artifact upload to Lake.
pub(super) fn publish(context: &AppContext, args: PublishArgs) -> Result<i32> {
    if context.local {
        bail!(
            "`lev publish` requires the real Git worktree; remove --local for release publication"
        );
    }
    let source = context.project()?;
    if args.no_sync {
        lockfile::verify(&source)?;
        ensure_toolchain(context, &source, !args.offline)?;
    } else {
        sync_execution(
            context,
            &source,
            &source,
            SyncArgs {
                offline: args.offline,
                update: false,
                locked: true,
                frozen: true,
            },
        )?;
    }

    let audit = project_audit::AuditReport::inspect(&source, &context.git, true);
    if audit.exit_code(!args.allow_nonportable) != 0 {
        bail!(
            "release audit failed ({} error{}, {} warning{}); run `lev audit --strict` for details{}",
            audit.summary.errors,
            if audit.summary.errors == 1 { "" } else { "s" },
            audit.summary.warnings,
            if audit.summary.warnings == 1 { "" } else { "s" },
            if args.allow_nonportable {
                ""
            } else {
                " or pass --allow-nonportable to accept warnings"
            }
        );
    }

    if !args.no_build {
        let mut command = vec![OsString::from("lake"), OsString::from("build")];
        command.extend(args.build_args);
        let code = run_in_project(context, &source, &command, !args.offline)?;
        if code != 0 {
            return Ok(code);
        }
    }

    let release =
        project_audit::verify_release_source(&source, &context.git, &args.tag, args.allow_dirty)?;
    if args.dry_run {
        println!(
            "would upload Lake build artifacts for {} at {}",
            args.tag, release.commit
        );
        return Ok(0);
    }

    context.info(format!(
        "publishing Lake build artifacts for {} at {}",
        args.tag, release.commit
    ));
    let command = [
        OsString::from("lake"),
        OsString::from("upload"),
        OsString::from(args.tag),
    ];
    run_in_project(context, &source, &command, false)
}

/// Display Lake's locked package inventory in stable text or raw JSON.
pub(super) fn dependencies(context: &AppContext, args: DepsArgs) -> Result<i32> {
    let project = context.project()?;
    let manifest = LakeManifest::read(&project.manifest_path())?;
    if args.json {
        json_output::print(schema::DEPS, &manifest)?;
        return Ok(0);
    }

    println!("{} ({})", project.root.display(), project.toolchain);
    if manifest.packages.is_empty() {
        println!("No locked dependencies");
        return Ok(0);
    }
    for package in &manifest.packages {
        let relation = if package.inherited {
            "transitive"
        } else {
            "direct"
        };
        let requested = package
            .input_rev
            .as_deref()
            .map(|revision| format!(" requested={revision}"))
            .unwrap_or_default();
        let locked = package
            .rev
            .as_deref()
            .map(|revision| format!(" locked={}", &revision[..revision.len().min(12)]))
            .unwrap_or_default();
        let scope = package
            .scope
            .as_deref()
            .filter(|scope| !scope.is_empty())
            .map(|scope| format!("{scope}/"))
            .unwrap_or_default();
        println!(
            "- {scope}{} [{relation}, {}]{requested}{locked}",
            package.name, package.kind
        );
    }
    Ok(0)
}

/// Render the checkout-derived dependency graph.
pub(super) fn dependency_tree(context: &AppContext, args: TreeArgs) -> Result<i32> {
    let graph = DependencyGraph::load(&context.project()?)?;
    if args.json {
        json_output::print(schema::TREE, &graph)?;
    } else {
        println!("{}", graph.render_tree());
    }
    Ok(0)
}

/// Explain one shortest root-to-package path in the dependency graph.
pub(super) fn why_dependency(context: &AppContext, args: WhyArgs) -> Result<i32> {
    let graph = DependencyGraph::load(&context.project()?)?;
    let path = graph
        .why(&args.package)
        .with_context(|| format!("package {:?} is not in the dependency graph", args.package))?;
    if args.json {
        json_output::print(schema::WHY, &path)?;
    } else {
        println!("{}", path.join(" -> "));
    }
    Ok(0)
}

/// Print the executable, project, toolchain, cache, and configuration context.
pub(super) fn doctor(context: &AppContext) -> Result<i32> {
    println!("lev cache: {}", context.cache.root.display());
    print_version(&context.git, &["--version"], "Git")?;
    match print_version(&context.elan, &["--version"], "elan") {
        Ok(()) => {}
        Err(_) => println!("elan: not installed (optional for lev-managed toolchains)"),
    }

    match context.project() {
        Ok(project) => {
            println!("project: {}", project.root.display());
            println!("toolchain: {}", project.toolchain);
            if let Some(stored) = context.store.find(&project.toolchain)? {
                println!("runtime: lev store ({})", stored.view.display());
            } else {
                println!("runtime: elan");
            }

            let mut lean =
                context.runtime_command(&project.toolchain, OsStr::new("lean"), false)?;
            lean.arg("--version");
            print_command_version(&mut lean, "Lean")?;

            let mut lake =
                context.runtime_command(&project.toolchain, OsStr::new("lake"), false)?;
            lake.arg("--version");
            print_command_version(&mut lake, "Lake")?;

            println!(
                "Lake artifact cache: {}",
                context.cache.lake_dir(&project.toolchain).display()
            );
            let config = LevConfig::read(&project.root)?;
            println!(
                "lev config: {}",
                config.path.as_deref().map_or_else(
                    || "not configured".to_owned(),
                    |path| path.display().to_string()
                )
            );
            println!("configured tasks: {}", config.tasks.len());
            println!(
                "configured matrix toolchains: {}",
                config.matrix_toolchains.len()
            );
        }
        Err(_) => {
            println!(
                "project: none found from {}",
                context.project_start.display()
            );
        }
    }
    Ok(0)
}

fn print_version(program: &OsStr, args: &[&str], label: &str) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    print_command_version(&mut command, label)
}

fn print_command_version(command: &mut Command, label: &str) -> Result<()> {
    let output = output_text(checked_output(command)?);
    println!("{label}: {output}");
    Ok(())
}
