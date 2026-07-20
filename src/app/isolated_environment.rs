//! Shared storage and locking for tool and script environments.
//!
//! An environment is reusable only after its specification marker is written.
//! Creation, validation, and repair all happen under the same per-key lock, so
//! another lev process never observes a half-built Lake project as complete.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};

use crate::cli::SyncArgs;
use crate::core::atomic_file::replace as atomic_write;
use crate::core::bounded_io;
use crate::core::clock::{modified_seconds, now_seconds, write_timestamp};
use crate::project::Project;

use super::AppContext;
use super::sync_commands::sync;
use super::toolchain_commands::ensure_toolchain_name;

const MAX_MARKER_BYTES: u64 = 1024 * 1024;
const COMPLETION_MARKER: &str = ".lev-environment.json";
const LAST_USED_MARKER: &str = ".lev-last-used";

pub(super) struct Request<'a, T> {
    pub environments: &'a Path,
    pub locks: &'a Path,
    pub key: &'a str,
    pub spec: &'a T,
    pub toolchain: &'a str,
    pub offline: bool,
}

/// Create or verify one specification-keyed Lake project under its lock.
pub(super) fn ensure<T, Initialize, Finalize>(
    context: &AppContext,
    request: Request<'_, T>,
    initialize: Initialize,
    finalize: Finalize,
) -> Result<Project>
where
    T: DeserializeOwned + PartialEq + Serialize,
    Initialize: FnOnce(&Path) -> Result<()>,
    Finalize: FnOnce(&Project) -> Result<()>,
{
    let Request {
        environments,
        locks,
        key,
        spec,
        toolchain,
        offline,
    } = request;
    validate_key(key)?;
    fs::create_dir_all(environments)
        .with_context(|| format!("failed to create {}", environments.display()))?;
    fs::create_dir_all(locks).with_context(|| format!("failed to create {}", locks.display()))?;
    let lock_path = locks.join(format!("{key}.lock"));
    // Hold this lock through validation as well as construction. A cached
    // environment can need repair, and repair must not race a concurrent run.
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;

    let project_root = environments.join(key);
    let marker = project_root.join(COMPLETION_MARKER);
    if marker.is_file() {
        let cached: T = bounded_io::read_json_file(&marker, MAX_MARKER_BYTES)?;
        if cached == *spec {
            let project = Project::load(project_root.clone())?;
            match sync(
                context,
                &project,
                SyncArgs {
                    offline,
                    update: false,
                    locked: true,
                    frozen: true,
                },
            ) {
                Ok(_) => {
                    write_timestamp(&project_root.join(LAST_USED_MARKER))?;
                    return Ok(project);
                }
                Err(error) if !offline => {
                    // Online mode may rebuild a cache entry whose dependencies
                    // or generated files were removed outside lev.
                    context.info(format!(
                        "rebuilding invalid cached environment {} ({error:#})",
                        &key[..12]
                    ));
                }
                Err(error) => return Err(error),
            }
        } else if offline {
            bail!("cached environment {key} does not match its specification");
        }
    } else if offline {
        bail!(
            "environment {} is not available offline; run once without --offline",
            &key[..12]
        );
    }

    remove_path_if_present(&project_root)?;
    let built = (|| {
        ensure_toolchain_name(context, toolchain, true)?;
        initialize(&project_root)?;
        let project = Project::load(project_root.clone())?;
        sync(
            context,
            &project,
            SyncArgs {
                offline: false,
                update: false,
                locked: false,
                frozen: false,
            },
        )?;
        finalize(&project)?;
        atomic_write(&marker, &serde_json::to_vec(spec)?)?;
        write_timestamp(&project_root.join(LAST_USED_MARKER))?;
        Ok(project)
    })();
    if built.is_err() {
        // Without the completion marker the directory is not reusable, but
        // removing it now also avoids accumulating failed build trees.
        let _ = remove_path_if_present(&project_root);
    }
    built
}

/// Return old, unreferenced environment directories with valid lev keys.
///
/// Unknown names are left alone because this collector only owns directories
/// whose names are canonical environment digests.
pub(super) fn gc_paths(
    environments: &Path,
    max_age_days: u64,
    live_keys: &HashSet<String>,
) -> Result<Vec<PathBuf>> {
    if !environments.is_dir() {
        return Ok(Vec::new());
    }
    let cutoff = now_seconds().saturating_sub(max_age_days.saturating_mul(86_400));
    let mut candidates = Vec::new();
    for entry in fs::read_dir(environments)
        .with_context(|| format!("failed to read {}", environments.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", environments.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let key = entry.file_name();
        let Some(key) = key.to_str() else {
            continue;
        };
        if !crate::core::hex::is_sha256(key) || live_keys.contains(key) {
            continue;
        }
        let freshness = freshness_path(&entry.path());
        if modified_seconds(&freshness) <= cutoff {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn validate_key(key: &str) -> Result<()> {
    if !crate::core::hex::is_sha256(key) {
        bail!("isolated environment key is malformed");
    }
    Ok(())
}

fn freshness_path(root: &Path) -> PathBuf {
    let last_used = root.join(LAST_USED_MARKER);
    if last_used.is_file() {
        return last_used;
    }
    let completion = root.join(COMPLETION_MARKER);
    if completion.is_file() {
        return completion;
    }
    root.to_owned()
}

fn remove_path_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))
        }
        Ok(_) => bail!("unsupported isolated environment entry: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;

    use super::gc_paths;

    #[test]
    fn gc_keeps_live_keys_and_ignores_unknown_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let environments = temporary.path().join("environments");
        fs::create_dir_all(&environments).unwrap();
        let stale_key = "1".repeat(64);
        let live_key = "2".repeat(64);
        fs::create_dir(environments.join(&stale_key)).unwrap();
        fs::create_dir(environments.join(&live_key)).unwrap();
        fs::create_dir(environments.join("not-managed-by-lev")).unwrap();

        let candidates = gc_paths(&environments, 0, &HashSet::from([live_key.clone()])).unwrap();

        assert_eq!(candidates, vec![environments.join(stale_key)]);
        assert!(environments.join(live_key).is_dir());
        assert!(environments.join("not-managed-by-lev").is_dir());
    }
}
