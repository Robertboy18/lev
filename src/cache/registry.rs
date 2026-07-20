//! Cache reachability records and garbage-collection planning.
//!
//! Records are advisory; deletion also requires the age gate and `--apply`.
//! A stale record may retain cache data, and a missing record may make old
//! cache entries reclaimable, but project source directories are never GC
//! candidates.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache::{CacheLayout, digest, path_bytes};
use crate::core::atomic_file::replace as atomic_replace;
use crate::core::clock::now_seconds;
use crate::dependency::environment as dependency_env;
use crate::dependency::resolution as resolution_cache;
use crate::project::Project;
use crate::project::local_workspace as workspace;
use crate::project::manifest::LakeManifest;

const RECORD_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectRecord {
    version: u32,
    pub root: PathBuf,
    pub toolchain: String,
    pub mirror_keys: Vec<String>,
    pub manifest_digest: Option<String>,
    pub last_used: u64,
}

#[derive(Debug)]
pub struct GcCandidate {
    pub kind: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct GcPlan {
    pub candidates: Vec<GcCandidate>,
}

impl GcPlan {
    pub fn bytes(&self) -> u64 {
        self.candidates
            .iter()
            .map(|candidate| candidate.bytes)
            .sum()
    }

    pub fn apply(&self) -> Result<()> {
        // Planning and deletion are separate so the CLI can display the exact
        // candidate set before the user opts in with `--apply`.
        for candidate in &self.candidates {
            let metadata = fs::symlink_metadata(&candidate.path)
                .with_context(|| format!("failed to inspect {}", candidate.path.display()))?;
            if metadata.is_dir() {
                fs::remove_dir_all(&candidate.path)
                    .with_context(|| format!("failed to remove {}", candidate.path.display()))?;
            } else {
                fs::remove_file(&candidate.path)
                    .with_context(|| format!("failed to remove {}", candidate.path.display()))?;
            }
        }
        Ok(())
    }
}

pub fn record(cache: &CacheLayout, project: &Project) -> Result<()> {
    cache.ensure()?;
    fs::create_dir_all(cache.registry_root()).with_context(|| {
        format!(
            "failed to create project registry {}",
            cache.registry_root().display()
        )
    })?;

    // Mirror reachability comes from exact URLs in the current Lake manifest.
    // The manifest digest later lets verification identify records that no
    // longer describe their project.
    let manifest_path = project.manifest_path();
    let (mirror_keys, manifest_digest) = if manifest_path.is_file() {
        let bytes = fs::read(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?;
        let manifest = LakeManifest::read(&manifest_path)?;
        let mut keys = manifest
            .packages
            .iter()
            .filter_map(|package| package.url.as_deref())
            .map(|url| digest(url.as_bytes()))
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        (keys, Some(digest(&bytes)))
    } else {
        (Vec::new(), None)
    };

    let record = ProjectRecord {
        version: RECORD_VERSION,
        root: project.root.clone(),
        toolchain: project.toolchain.clone(),
        mirror_keys,
        manifest_digest,
        last_used: now_seconds(),
    };
    let path = record_path(cache, project);
    atomic_replace(&path, &serde_json::to_vec_pretty(&record)?)?;
    Ok(())
}

/// Replace the advisory record after a managed workspace changes its key.
pub fn replace(cache: &CacheLayout, previous: &Project, current: &Project) -> Result<()> {
    let previous_path = record_path(cache, previous);
    let current_path = record_path(cache, current);
    record(cache, current)?;
    if previous_path != current_path && previous_path.is_file() {
        fs::remove_file(&previous_path)
            .with_context(|| format!("failed to remove {}", previous_path.display()))?;
    }
    Ok(())
}

pub fn load(cache: &CacheLayout) -> Result<Vec<(PathBuf, ProjectRecord)>> {
    let root = cache.registry_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let bytes = fs::read(entry.path())
            .with_context(|| format!("failed to read {}", entry.path().display()))?;
        let record: ProjectRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", entry.path().display()))?;
        if record.version != RECORD_VERSION {
            anyhow::bail!(
                "{} uses unsupported project record version {}",
                entry.path().display(),
                record.version
            );
        }
        records.push((entry.path(), record));
    }
    Ok(records)
}

pub fn gc_plan(cache: &CacheLayout, max_age_days: u64) -> Result<GcPlan> {
    let cutoff = now_seconds().saturating_sub(max_age_days.saturating_mul(86_400));
    let records = load(cache)?;
    let mut live_mirrors = HashSet::new();
    let mut live_lake = HashSet::new();
    let mut live_workspaces = HashSet::new();
    let mut candidates = Vec::new();
    let workspace_root = cache.workspace_root();

    for (path, record) in records {
        let workspace = record.root.starts_with(&workspace_root);
        // Ordinary source projects remain pinned while their lean-toolchain
        // still agrees with the record. Managed copies are instead kept by
        // recent use because their source project owns the durable pin.
        let current_pin = !workspace
            && Project::load(record.root.clone())
                .ok()
                .is_some_and(|project| project.toolchain == record.toolchain);
        let recently_used = record.last_used >= cutoff;
        if current_pin || recently_used {
            live_mirrors.extend(record.mirror_keys);
            live_lake.insert(toolchain_key(&record.toolchain));
            if workspace {
                if let Some(container) = record.root.parent() {
                    live_workspaces.insert(container.to_owned());
                }
            }
        } else {
            candidates.push(GcCandidate {
                kind: "project-record",
                bytes: path_bytes(&path)?,
                path,
            });
        }
    }

    let registry_root = cache.registry_root();
    if registry_root.exists() {
        for entry in fs::read_dir(&registry_root)
            .with_context(|| format!("failed to read {}", registry_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", registry_root.display()))?;
            if entry.file_type()?.is_file()
                && is_temporary_record(&entry.path())
                && old_enough(&entry.path(), cutoff)?
            {
                candidates.push(GcCandidate {
                    kind: "temporary-project-record",
                    bytes: path_bytes(&entry.path())?,
                    path: entry.path(),
                });
            }
        }
    }

    // Everything below is collected only when it is both unreachable from
    // the live records above and older than the requested cutoff.
    for mirror in cache.mirror_paths()? {
        let Some(name) = mirror.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if !live_mirrors.contains(name) && old_enough(&mirror, cutoff)? {
            candidates.push(GcCandidate {
                kind: "git-mirror",
                bytes: path_bytes(&mirror)?,
                path: mirror,
            });
        }
    }

    let lake_root = cache.lake_root();
    if lake_root.exists() {
        for entry in fs::read_dir(&lake_root)
            .with_context(|| format!("failed to read {}", lake_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", lake_root.display()))?;
            if !entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?
                .is_dir()
            {
                continue;
            }
            let key = entry.file_name().to_string_lossy().into_owned();
            if !live_lake.contains(&key) && old_enough(&entry.path(), cutoff)? {
                candidates.push(GcCandidate {
                    kind: "lake-artifacts",
                    bytes: path_bytes(&entry.path())?,
                    path: entry.path(),
                });
            }
        }
    }

    for path in workspace::gc_paths(cache, cutoff, &live_workspaces)? {
        candidates.push(GcCandidate {
            kind: "local-workspace",
            bytes: path_bytes(&path)?,
            path,
        });
    }

    let live_dependency_environments = dependency_env::live_paths(cache)?;
    for path in dependency_env::gc_paths(cache, cutoff, &live_dependency_environments)? {
        candidates.push(GcCandidate {
            kind: "dependency-environment",
            bytes: path_bytes(&path)?,
            path,
        });
    }

    for path in resolution_cache::gc_paths(cache, cutoff)? {
        candidates.push(GcCandidate {
            kind: "dependency-resolution",
            bytes: path_bytes(&path)?,
            path,
        });
    }

    let reservoir_root = cache.reservoir_root();
    if reservoir_root.is_dir() {
        for entry in fs::read_dir(&reservoir_root)
            .with_context(|| format!("failed to read {}", reservoir_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", reservoir_root.display()))?;
            if entry.file_type()?.is_file() && old_enough(&entry.path(), cutoff)? {
                candidates.push(GcCandidate {
                    kind: "reservoir-metadata",
                    bytes: path_bytes(&entry.path())?,
                    path: entry.path(),
                });
            }
        }
    }

    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(GcPlan { candidates })
}

pub fn toolchain_key(toolchain: &str) -> String {
    digest(toolchain.as_bytes())[..16].to_owned()
}

fn record_path(cache: &CacheLayout, project: &Project) -> PathBuf {
    let identity = format!("{}\0{}", project.root.to_string_lossy(), project.toolchain);
    cache
        .registry_root()
        .join(format!("{}.json", digest(identity.as_bytes())))
}

fn is_temporary_record(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if let Some((key, suffix)) = name.split_once(".json.tmp-") {
        return valid_record_key(key) && !suffix.is_empty();
    }
    let Some(name) = name.strip_prefix('.') else {
        return false;
    };
    let Some((key, suffix)) = name.split_once(".json.lev-tmp-") else {
        return false;
    };
    valid_record_key(key) && crate::core::hex::is_lower_hex(suffix, 16)
}

fn valid_record_key(value: &str) -> bool {
    crate::core::hex::is_sha256(value)
}

fn old_enough(path: &Path, cutoff: u64) -> Result<bool> {
    let modified = fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    Ok(modified <= cutoff)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use crate::cache::{CacheLayout, digest};
    use crate::project::Project;

    use super::{gc_plan, load, record};

    #[test]
    fn records_projects_and_preserves_live_cache_entries() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("project");
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        fs::write(
            root.join("lake-manifest.json"),
            r#"{"packagesDir":".lake/packages","packages":[]}"#,
        )
        .unwrap();
        let project = Project::discover(&root).unwrap();

        record(&cache, &project).unwrap();
        assert_eq!(load(&cache).unwrap().len(), 1);
        fs::create_dir_all(cache.lake_dir(&project.toolchain)).unwrap();
        thread::sleep(Duration::from_millis(10));
        assert!(gc_plan(&cache, 0).unwrap().candidates.is_empty());
    }

    #[test]
    fn ignores_and_collects_interrupted_record_writes() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        fs::create_dir_all(cache.registry_root()).unwrap();
        let key = digest(b"interrupted-record");
        let legacy = cache.registry_root().join(format!("{key}.json.tmp-123"));
        let current = cache
            .registry_root()
            .join(format!(".{key}.json.lev-tmp-0123456789abcdef"));
        fs::write(&legacy, "{").unwrap();
        fs::write(&current, "{").unwrap();

        assert!(load(&cache).unwrap().is_empty());
        let plan = gc_plan(&cache, 0).unwrap();
        assert_eq!(plan.candidates.len(), 2);
        assert!(
            plan.candidates
                .iter()
                .all(|candidate| candidate.kind == "temporary-project-record")
        );
        assert!(
            plan.candidates
                .iter()
                .any(|candidate| candidate.path == legacy)
        );
        assert!(
            plan.candidates
                .iter()
                .any(|candidate| candidate.path == current)
        );
        plan.apply().unwrap();
        assert!(!legacy.exists());
        assert!(!current.exists());
    }
}
