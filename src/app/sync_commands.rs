//! Project synchronization and lock publication.
//!
//! A sync commits Lake and lev state together, after dependencies have been
//! materialized and checked.

use anyhow::{Result, bail};

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cli::SyncArgs;
use crate::core::process::checked_status;
use crate::dependency::environment as dependency_env;
use crate::dependency::git::{GitCache, SyncStats};
use crate::project::Project;
use crate::project::config::LevConfig;
use crate::project::local_workspace as workspace;
use crate::project::lockfile;
use crate::project::manifest::LakeManifest;

use super::AppContext;
use super::toolchain_commands::ensure_toolchain;
use crate::core::atomic_file::copy_if_changed as copy_file_if_changed;

use super::transaction::FileTransaction;

/// Reconcile Lake's locked package set with lev's shared local stores.
///
/// Manifest and integrity-lock changes commit together. Lake remains the only
/// resolver; lev validates and materializes the revisions Lake selected.
pub(super) fn sync(context: &AppContext, project: &Project, args: SyncArgs) -> Result<SyncStats> {
    let manifest_path = project.manifest_path();
    let lock_path = project.lock_path();
    if args.frozen {
        lockfile::verify(project)?;
    }
    if (args.locked || args.frozen) && !manifest_path.is_file() {
        bail!(
            "{} is missing; remove --locked/--frozen to let Lake create it",
            manifest_path.display()
        );
    }
    if !manifest_path.is_file() && args.offline {
        bail!(
            "{} is missing and cannot be created in offline mode",
            manifest_path.display()
        );
    }
    let mut transaction = FileTransaction::capture([&manifest_path, &lock_path])?;

    ensure_toolchain(context, project, !args.offline)?;
    context.cache.ensure()?;

    if args.update || !manifest_path.is_file() {
        context.info("resolving dependencies with Lake");
        let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
        let mut command =
            context.runtime_command(&project.toolchain, std::ffi::OsStr::new("lake"), true)?;
        command.arg("update");
        context.command_env(&mut command, project)?;
        checked_status(&mut command)?;
    }

    let manifest = LakeManifest::read(&manifest_path)?;
    LevConfig::read(&project.root)?.verify_constraints(&manifest)?;
    let dependency_environment =
        if let Some((stats, lock)) = dependency_env::attach(&context.cache, project, &manifest)? {
            context.detail(format!(
                "shared dependency environment {} ({})",
                &stats.key[..12],
                if stats.migrated {
                    "migrated"
                } else if stats.reused {
                    "reused"
                } else {
                    "created"
                }
            ));
            Some(lock)
        } else {
            None
        };
    let packages_dir = manifest.packages_path(&project.root)?;
    let packages = manifest.git_packages()?;
    let seed_packages = workspace::source_packages_dir(project)?;
    let cache = GitCache::new(&context.cache, &context.git, args.offline)
        .with_seed_packages_dir(seed_packages.as_deref())
        .with_deferred_mirrors_for_existing(dependency_environment.is_some());
    let stats = cache.sync(&packages_dir, &packages)?;
    if !args.frozen {
        lockfile::refresh(project)?;
    }
    registry::record(&context.cache, project)?;
    transaction.commit();

    let changed = stats.packages_created
        + stats.packages_updated
        + stats.mirrors_created
        + stats.mirrors_fetched;
    let summary = format!(
        "{} Git dependencies: {} created, {} reused, {} updated; {} mirrors created, {} fetched",
        packages.len(),
        stats.packages_created,
        stats.packages_reused,
        stats.packages_updated,
        stats.mirrors_created,
        stats.mirrors_fetched
    );
    if changed > 0 {
        context.info(format!("synchronized {summary}"));
    } else {
        context.detail(format!("already synchronized {summary}"));
    }
    if stats.mirrors_deferred > 0 {
        context.detail(format!(
            "deferred {} redundant Git mirror creation{} for managed checkouts",
            stats.mirrors_deferred,
            if stats.mirrors_deferred == 1 { "" } else { "s" }
        ));
    }
    Ok(stats)
}

/// Sync an execution copy and publish matching locks back to its source.
pub(super) fn sync_execution(
    context: &AppContext,
    source: &Project,
    project: &Project,
    args: SyncArgs,
) -> Result<SyncStats> {
    let stats = sync(context, project, args)?;
    if source.root != project.root && source.toolchain == project.toolchain {
        publish_workspace_locks(context, source, project)?;
    }
    Ok(stats)
}

/// Atomically copy a managed local workspace's locks to its source project.
fn publish_workspace_locks(
    context: &AppContext,
    source: &Project,
    workspace: &Project,
) -> Result<()> {
    if !workspace::is_managed(&context.cache, workspace) {
        return Ok(());
    }
    let source_manifest = source.manifest_path();
    let source_lock = source.lock_path();
    let workspace_manifest = workspace.manifest_path();
    let workspace_lock = workspace.lock_path();
    if !workspace_manifest.is_file() || !workspace_lock.is_file() {
        bail!("local synchronization did not produce both project lock files");
    }
    let mut transaction = FileTransaction::capture([&source_manifest, &source_lock])?;
    copy_file_if_changed(&workspace_manifest, &source_manifest)?;
    copy_file_if_changed(&workspace_lock, &source_lock)?;
    registry::record(&context.cache, source)?;
    transaction.commit();
    context.detail("published lake-manifest.json and lev.lock to the source project");
    Ok(())
}
