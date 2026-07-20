//! Transactional dependency and project-version commands.
//!
//! Lake resolves general graphs; lev handles edits, rollback, metadata lookup,
//! and post-resolution checks.

use anyhow::{Context, Result, bail};

use crate::cache::registry;
use crate::cli::{AddArgs, OutdatedArgs, PinArgs, RemoveArgs, SyncArgs, UpdateArgs, UpgradeArgs};
use crate::core::json_output::{self, schema};
use crate::core::process::checked_status;
use crate::dependency::reservoir::{
    ReservoirClient, ReservoirPackage, ReservoirVersion, compatible_release, latest_release,
};
use crate::project::Project;
use crate::project::config::LevConfig;
use crate::project::lakefile::{self, Dependency, DependencySource};
use crate::project::lockfile;
use crate::project::manifest::LakeManifest;
use crate::toolchain;

use super::AppContext;
use super::sync_commands::sync;
use super::toolchain_commands::{ensure_toolchain, ensure_toolchain_name};
use crate::core::atomic_file::replace as atomic_write;

use super::transaction::FileTransaction;

/// Add one dependency declaration and, by default, resolve it immediately.
///
/// Configuration, Lake's manifest, and lev's integrity lock form one
/// transaction. A parser, resolver, network, or checkout failure therefore
/// restores the exact project state that existed before the command.
pub(super) fn add(context: &AppContext, args: AddArgs) -> Result<i32> {
    let project = context.project()?;
    ensure_toolchain(context, &project, true)?;
    let config = lakefile::config_path(&project.root)?;
    let lev_config = project.root.join("lev.toml");
    let manifest = project.manifest_path();
    let lock = project.lock_path();
    let mut transaction = FileTransaction::capture([&config, &lev_config, &manifest, &lock])?;

    let source = if let Some(url) = args.git {
        DependencySource::Git { url }
    } else if let Some(path) = args.path {
        DependencySource::Path { path }
    } else {
        DependencySource::Registry { scope: args.scope }
    };
    let dependency = Dependency {
        name: args.name.clone(),
        source,
        rev: args.rev,
    };
    lakefile::add(&config, &dependency, args.replace)?;
    if let Some(group) = args.group.as_deref() {
        LevConfig::add_to_dependency_group(&project.root, group, &args.name)?;
    }

    if !args.no_sync {
        run_lake_update(context, &project, std::slice::from_ref(&args.name))?;
        sync_after_resolution(context, &project)?;
    }
    transaction.commit();
    if let Some(revision) = &dependency.rev {
        context.info(format!("added dependency {} at {revision}", args.name));
    } else {
        context.info(format!("added dependency {}", args.name));
    }
    Ok(0)
}

/// Remove one dependency declaration and resolve the resulting project.
///
/// Lake decides whether the package remains transitively required. lev only
/// edits the direct declaration and commits the files after synchronization
/// has produced a self-consistent manifest and lock.
pub(super) fn remove(context: &AppContext, args: RemoveArgs) -> Result<i32> {
    let project = context.project()?;
    ensure_toolchain(context, &project, true)?;
    let config = lakefile::config_path(&project.root)?;
    let lev_config = project.root.join("lev.toml");
    let manifest = project.manifest_path();
    let lock = project.lock_path();
    let mut transaction = FileTransaction::capture([&config, &lev_config, &manifest, &lock])?;
    lakefile::remove(&config, &args.name)?;
    LevConfig::remove_from_dependency_groups(&project.root, &args.name)?;

    if !args.no_sync {
        run_lake_update(context, &project, std::slice::from_ref(&args.name))?;
        sync_after_resolution(context, &project)?;
    }
    transaction.commit();
    context.info(format!("removed dependency {}", args.name));
    Ok(0)
}

/// Ask Lake to refresh selected moving revisions without changing declarations.
pub(super) fn update(context: &AppContext, args: UpdateArgs) -> Result<i32> {
    let project = context.project()?;
    ensure_toolchain(context, &project, true)?;
    let packages = selected_packages(&project, &args.packages, args.group.as_deref())?;
    let manifest = project.manifest_path();
    let lock = project.lock_path();
    let mut transaction = FileTransaction::capture([&manifest, &lock])?;
    run_lake_update(context, &project, &packages)?;
    sync_after_resolution(context, &project)?;
    transaction.commit();
    Ok(0)
}

/// Reservoir metadata plus the locked Lake package used to classify one row.
#[derive(Debug)]
struct DependencyInspection {
    package: crate::project::manifest::ManifestPackage,
    reservoir: Option<ReservoirPackage>,
    compatible: Option<ReservoirVersion>,
    latest: Option<ReservoirVersion>,
    from_cache: bool,
    error: Option<String>,
}

/// Stable serialized form emitted by `lev outdated --json`.
#[derive(Debug, serde::Serialize)]
struct DependencyStatus {
    package: String,
    direct: bool,
    reservoir: Option<String>,
    requested: Option<String>,
    locked: Option<String>,
    compatible: Option<VersionStatus>,
    latest: Option<VersionStatus>,
    status: &'static str,
    metadata_cache_hit: bool,
    error: Option<String>,
}

/// The release fields useful to humans and automation inspecting an upgrade.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct VersionStatus {
    tag: Option<String>,
    revision: String,
    toolchain: String,
}

impl From<&ReservoirVersion> for VersionStatus {
    fn from(version: &ReservoirVersion) -> Self {
        Self {
            tag: version.tag.clone(),
            revision: version.revision.clone(),
            toolchain: version.toolchain.clone(),
        }
    }
}

impl DependencyInspection {
    /// Reduce lookup details to the mutually exclusive public status labels.
    fn status(&self) -> DependencyStatus {
        let status = if self.error.is_some() {
            "unavailable"
        } else if self.reservoir.is_none() {
            "unmanaged"
        } else if self.compatible.is_none() {
            "incompatible"
        } else if self.package.rev.as_deref()
            == self
                .compatible
                .as_ref()
                .map(|version| version.revision.as_str())
        {
            "current"
        } else {
            "outdated"
        };
        DependencyStatus {
            package: self.package.name.clone(),
            direct: !self.package.inherited,
            reservoir: self.reservoir.as_ref().map(ReservoirPackage::full_name),
            requested: self.package.input_rev.clone(),
            locked: self.package.rev.clone(),
            compatible: self.compatible.as_ref().map(VersionStatus::from),
            latest: self.latest.as_ref().map(VersionStatus::from),
            status,
            metadata_cache_hit: self.from_cache,
            error: self.error.clone(),
        }
    }
}

/// Compare locked dependencies with releases for the exact selected Lean.
pub(super) fn outdated(context: &AppContext, args: OutdatedArgs) -> Result<i32> {
    let project = context.project()?;
    let packages = selected_packages(&project, &args.packages, args.group.as_deref())?;
    let inspections = inspect_dependencies(
        context,
        &project,
        &packages,
        args.all,
        args.offline,
        args.refresh,
        &project.toolchain,
    )?;
    let statuses = inspections
        .iter()
        .map(DependencyInspection::status)
        .collect::<Vec<_>>();
    if args.json {
        json_output::print(schema::OUTDATED, &statuses)?;
    } else if statuses.is_empty() {
        println!("No matching dependencies");
    } else {
        println!(
            "{:<24} {:<16} {:<16} {:<23} STATUS",
            "PACKAGE", "REQUESTED", "COMPATIBLE", "LATEST"
        );
        for status in &statuses {
            println!(
                "{:<24} {:<16} {:<16} {:<23} {}",
                status.package,
                status.requested.as_deref().unwrap_or("-"),
                status
                    .compatible
                    .as_ref()
                    .map(display_version_status)
                    .unwrap_or_else(|| "-".to_owned()),
                status
                    .latest
                    .as_ref()
                    .map(|version| {
                        format!(
                            "{} ({})",
                            display_version_status(version),
                            toolchain::short_name(&version.toolchain)
                        )
                    })
                    .unwrap_or_else(|| "-".to_owned()),
                status.status
            );
            if let Some(error) = &status.error {
                context.detail(format!("{}: {error}", status.package));
            }
        }
    }
    let unavailable = statuses.iter().any(|status| status.status == "unavailable");
    let outdated = statuses.iter().any(|status| status.status == "outdated");
    if unavailable {
        Ok(2)
    } else if args.check && outdated {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Upgrade compatible direct releases, optionally together with Lean itself.
///
/// Reservoir proposes a tagged revision, but Lake performs the real
/// resolution. Before committing, lev reads Lake's new manifest and requires
/// every resolved commit to equal Reservoir's advertised commit. Any mismatch
/// drops the transaction and restores all four project files.
pub(super) fn upgrade(context: &AppContext, args: UpgradeArgs) -> Result<i32> {
    let project = context.project()?;
    let packages = selected_packages(&project, &args.packages, args.group.as_deref())?;
    let target_toolchain = args
        .lean
        .as_deref()
        .map(toolchain::normalize)
        .transpose()?
        .unwrap_or_else(|| project.toolchain.clone());
    let inspections = inspect_dependencies(
        context,
        &project,
        &packages,
        false,
        false,
        args.refresh,
        &target_toolchain,
    )?;
    let explicitly_selected = !packages.is_empty();
    let changing_toolchain = target_toolchain != project.toolchain;
    let mut upgrades = Vec::new();

    for inspection in inspections {
        let package = &inspection.package;
        let Some(reservoir) = &inspection.reservoir else {
            if explicitly_selected {
                bail!(
                    "dependency {} has no Reservoir identity; use `lev update {}` to preserve its declared revision",
                    package.name,
                    package.name
                );
            }
            context.detail(format!("skipping unindexed dependency {}", package.name));
            continue;
        };
        if let Some(error) = inspection.error {
            bail!(
                "failed to inspect {} for dependency {}: {error}",
                reservoir.full_name(),
                package.name
            );
        }
        let compatible = inspection.compatible.with_context(|| {
            format!(
                "{} has no Reservoir release for {}",
                reservoir.full_name(),
                target_toolchain
            )
        })?;
        let Some(tag) = compatible.tag.clone() else {
            if explicitly_selected || changing_toolchain {
                bail!(
                    "{} has no tagged release for {}",
                    reservoir.full_name(),
                    target_toolchain
                );
            }
            continue;
        };
        if crate::core::hex::is_git_object_id(package.input_rev.as_deref().unwrap_or(""))
            && !changing_toolchain
        {
            if explicitly_selected {
                bail!(
                    "dependency {} is pinned to an exact commit; replace that pin explicitly before upgrading",
                    package.name
                );
            }
            context.detail(format!(
                "skipping exact commit pin for dependency {}",
                package.name
            ));
            continue;
        }
        if package.input_rev.as_deref() == Some(tag.as_str())
            && package.rev.as_deref() == Some(compatible.revision.as_str())
        {
            continue;
        }
        upgrades.push(DependencyUpgrade {
            package: package.name.clone(),
            from: package
                .input_rev
                .clone()
                .or_else(|| package.rev.as_deref().map(short_revision)),
            tag,
            revision: compatible.revision,
        });
    }

    if !changing_toolchain && upgrades.is_empty() {
        context.info("all selected direct dependencies are current");
        return Ok(0);
    }
    if changing_toolchain {
        println!("Lean\t{} -> {}", project.toolchain, target_toolchain);
    }
    for upgrade in &upgrades {
        println!(
            "{}\t{} -> {} ({})",
            upgrade.package,
            upgrade.from.as_deref().unwrap_or("-"),
            upgrade.tag,
            short_revision(&upgrade.revision)
        );
    }
    if args.dry_run {
        println!("Dry run; no project files changed");
        return Ok(0);
    }

    ensure_toolchain_name(context, &target_toolchain, true)?;
    let config = lakefile::config_path(&project.root)?;
    let toolchain_file = project.root.join("lean-toolchain");
    let manifest = project.manifest_path();
    let lock = project.lock_path();
    let mut transaction = FileTransaction::capture([&config, &toolchain_file, &manifest, &lock])?;
    for upgrade in &upgrades {
        lakefile::set_revision(&config, &upgrade.package, &upgrade.tag)?;
    }
    if changing_toolchain {
        atomic_write(&toolchain_file, format!("{target_toolchain}\n").as_bytes())?;
    }
    let upgraded_project = Project::load(project.root.clone())?;
    let package_names = upgrades
        .iter()
        .map(|upgrade| upgrade.package.clone())
        .collect::<Vec<_>>();
    if changing_toolchain {
        run_lake_update(context, &upgraded_project, &[])?;
    } else {
        run_lake_update(context, &upgraded_project, &package_names)?;
    }
    sync_after_resolution(context, &upgraded_project)?;
    if let Err(error) = verify_upgraded_revisions(&upgraded_project, &upgrades) {
        // Restore project files before repairing the registry entry; otherwise
        // the registry could briefly retain the rejected toolchain state.
        drop(transaction);
        if let Ok(restored) = Project::load(project.root.clone()) {
            let _ = registry::record(&context.cache, &restored);
        }
        return Err(error);
    }
    transaction.commit();
    context.info(format!(
        "upgraded {} direct dependencies for {}",
        upgrades.len(),
        target_toolchain
    ));
    Ok(0)
}

fn selected_packages(
    project: &Project,
    packages: &[String],
    group: Option<&str>,
) -> Result<Vec<String>> {
    if let Some(group) = group {
        return LevConfig::read(&project.root)?.group_packages(group);
    }
    Ok(packages.to_vec())
}

/// One declaration change and the immutable revision it must resolve to.
#[derive(Debug)]
struct DependencyUpgrade {
    package: String,
    from: Option<String>,
    tag: String,
    revision: String,
}

/// Load selected manifest packages and enrich them with Reservoir releases.
///
/// Metadata errors are captured per package so `outdated` can report all rows.
/// `upgrade` subsequently promotes any selected-package error to a hard
/// failure because mutating from incomplete information would be unsafe.
fn inspect_dependencies(
    context: &AppContext,
    project: &Project,
    selected: &[String],
    include_transitive: bool,
    offline: bool,
    refresh: bool,
    toolchain: &str,
) -> Result<Vec<DependencyInspection>> {
    let manifest = LakeManifest::read(&project.manifest_path())?;
    let mut packages = manifest
        .packages
        .into_iter()
        .filter(|package| {
            (include_transitive || !package.inherited)
                && (selected.is_empty() || selected.contains(&package.name))
        })
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    packages.dedup_by(|left, right| left.name == right.name);
    for name in selected {
        if !packages.iter().any(|package| &package.name == name) {
            bail!(
                "package {name:?} is not a {}dependency in {}",
                if include_transitive { "" } else { "direct " },
                project.manifest_path().display()
            );
        }
    }

    let config = LevConfig::read(&project.root)?;
    let mut inspections = Vec::with_capacity(packages.len());
    for package in packages {
        let Some(reservoir) = ReservoirPackage::from_manifest(&package) else {
            inspections.push(DependencyInspection {
                package,
                reservoir: None,
                compatible: None,
                latest: None,
                from_cache: false,
                error: None,
            });
            continue;
        };
        let client = ReservoirClient::new(&context.cache, offline, refresh)
            .with_source(config.reservoir_source(&reservoir)?);
        match client.versions(&reservoir) {
            Ok(set) => inspections.push(DependencyInspection {
                compatible: compatible_release(&set.versions, toolchain).cloned(),
                latest: latest_release(&set.versions).cloned(),
                package,
                reservoir: Some(reservoir),
                from_cache: set.from_cache,
                error: None,
            }),
            Err(error) => inspections.push(DependencyInspection {
                package,
                reservoir: Some(reservoir),
                compatible: None,
                latest: None,
                from_cache: false,
                error: Some(format!("{error:#}")),
            }),
        }
    }
    Ok(inspections)
}

/// Prove that Lake resolved every proposed tag to Reservoir's exact commit.
fn verify_upgraded_revisions(project: &Project, upgrades: &[DependencyUpgrade]) -> Result<()> {
    let manifest = LakeManifest::read(&project.manifest_path())?;
    for upgrade in upgrades {
        let resolved = manifest
            .packages
            .iter()
            .find(|package| package.name == upgrade.package)
            .with_context(|| {
                format!(
                    "Lake did not resolve upgraded dependency {}",
                    upgrade.package
                )
            })?;
        if resolved.rev.as_deref() != Some(upgrade.revision.as_str()) {
            bail!(
                "Lake resolved {} to {}, but Reservoir release {} requires {}; rolling back",
                upgrade.package,
                resolved.rev.as_deref().unwrap_or("<missing>"),
                upgrade.tag,
                upgrade.revision
            );
        }
    }
    Ok(())
}

/// Render a release tag when present, otherwise a compact immutable revision.
fn display_version_status(version: &VersionStatus) -> String {
    version
        .tag
        .clone()
        .unwrap_or_else(|| short_revision(&version.revision))
}

/// Return at most twelve Unicode scalar values without slicing mid-character.
fn short_revision(revision: &str) -> String {
    revision.chars().take(12).collect()
}

/// Change the Lean pin, optionally re-resolving dependencies for that Lean.
///
/// Without `--update`, the existing manifest remains authoritative and only
/// lev's lock envelope is refreshed. With `--update`, toolchain, manifest, and
/// lock are committed together after Lake and cache synchronization succeed.
pub(super) fn pin(context: &AppContext, args: PinArgs) -> Result<i32> {
    let project = context.project()?;
    let toolchain = toolchain::normalize(&args.toolchain)?;
    ensure_toolchain_name(context, &toolchain, !args.offline)?;

    let toolchain_file = project.root.join("lean-toolchain");
    let manifest = project.manifest_path();
    let lock = project.lock_path();
    let mut transaction = FileTransaction::capture([&toolchain_file, &manifest, &lock])?;
    atomic_write(&toolchain_file, format!("{toolchain}\n").as_bytes())?;
    let pinned = Project::load(project.root.clone())?;

    if args.update {
        run_lake_update(context, &pinned, &[])?;
        sync_after_resolution(context, &pinned)?;
    }
    if !args.update && pinned.manifest_path().is_file() {
        lockfile::refresh(&pinned)?;
    }
    registry::record(&context.cache, &pinned)?;
    transaction.commit();
    context.info(format!(
        "pinned {} to {}",
        project.root.display(),
        toolchain
    ));
    Ok(0)
}

/// Run Lake update under lev's normal project environment.
fn run_lake_update(context: &AppContext, project: &Project, packages: &[String]) -> Result<()> {
    let mut command =
        context.runtime_command(&project.toolchain, std::ffi::OsStr::new("lake"), true)?;
    command.arg("update").args(packages);
    context.command_env(&mut command, project)?;
    checked_status(&mut command)
}

/// Finish a Lake resolution by materializing and locking its exact packages.
fn sync_after_resolution(context: &AppContext, project: &Project) -> Result<()> {
    sync(
        context,
        project,
        SyncArgs {
            offline: false,
            update: false,
            locked: true,
            frozen: false,
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::project::manifest::ManifestPackage;

    use super::*;

    fn package(revision: Option<&str>) -> ManifestPackage {
        ManifestPackage {
            name: "example".to_owned(),
            kind: "git".to_owned(),
            url: Some("https://github.com/owner/example".to_owned()),
            rev: revision.map(str::to_owned),
            scope: Some("owner".to_owned()),
            input_rev: Some("v1.0.0".to_owned()),
            inherited: false,
            sub_dir: None,
            dir: None,
            manifest_file: None,
            config_file: None,
        }
    }

    fn release(revision: &str) -> ReservoirVersion {
        ReservoirVersion {
            version: "1.0.0".to_owned(),
            revision: revision.to_owned(),
            date: "2026-07-17".to_owned(),
            tag: Some("v1.0.0".to_owned()),
            toolchain: "leanprover/lean4:v4.fixture-d".to_owned(),
            dependencies: None,
        }
    }

    fn inspection(
        revision: Option<&str>,
        indexed: bool,
        compatible: Option<ReservoirVersion>,
        error: Option<&str>,
    ) -> DependencyInspection {
        DependencyInspection {
            package: package(revision),
            reservoir: indexed.then(|| ReservoirPackage {
                owner: "owner".to_owned(),
                name: "example".to_owned(),
            }),
            compatible,
            latest: None,
            from_cache: false,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn short_revision_is_unicode_safe() {
        assert_eq!(short_revision("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_revision("λεια"), "λεια");
    }

    #[test]
    fn dependency_statuses_cover_every_public_classification() {
        let locked = "a".repeat(40);
        let other = "b".repeat(40);

        assert_eq!(
            inspection(Some(&locked), false, None, None).status().status,
            "unmanaged"
        );
        assert_eq!(
            inspection(Some(&locked), true, None, None).status().status,
            "incompatible"
        );
        assert_eq!(
            inspection(Some(&locked), true, Some(release(&locked)), None)
                .status()
                .status,
            "current"
        );
        assert_eq!(
            inspection(Some(&locked), true, Some(release(&other)), None)
                .status()
                .status,
            "outdated"
        );
        assert_eq!(
            inspection(Some(&locked), true, None, Some("network unavailable"))
                .status()
                .status,
            "unavailable"
        );
    }

    #[test]
    fn version_display_prefers_a_tag_and_shortens_untagged_revisions() {
        let tagged = VersionStatus {
            tag: Some("v1.2.3".to_owned()),
            revision: "0123456789abcdef".to_owned(),
            toolchain: "leanprover/lean4:v4.fixture-d".to_owned(),
        };
        let untagged = VersionStatus {
            tag: None,
            revision: tagged.revision.clone(),
            toolchain: tagged.toolchain.clone(),
        };

        assert_eq!(display_version_status(&tagged), "v1.2.3");
        assert_eq!(display_version_status(&untagged), "0123456789ab");
        assert_eq!(toolchain::short_name(&tagged.toolchain), "v4.fixture-d");
    }
}
