//! The projects lev has built, so cache collection knows what is still in use.
//!
//! `lev inspect build` and `lev check` record their project. A record stays live while its
//! project still has a `lean-toolchain` file, or for the age gate after the
//! project disappears. Records are advisory: a project lev never built is not
//! protected by them.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{CacheLayout, GcCandidate, digest};
use crate::core::atomic_file;
use crate::core::clock::{modified_seconds, now_seconds};
use crate::core::hex::is_lower_hex;
use crate::project::Project;

const RECORD_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectRecord {
    version: u32,
    pub root: PathBuf,
    pub toolchain: String,
    /// The Lake artifact cache the project's last `lev inspect build` or
    /// `lev check` used.
    pub lake_cache: Option<PathBuf>,
    pub last_used: u64,
}

impl ProjectRecord {
    /// Whether the project still exists.
    pub fn exists(&self) -> bool {
        self.root.join("lean-toolchain").is_file()
    }

    /// Whether collection must still treat the project as in use.
    pub fn is_live(&self, cutoff: u64) -> bool {
        self.exists() || self.last_used > cutoff
    }
}

/// Record that lev built `project` now, using `lake_cache` when it is known.
pub fn record(cache: &CacheLayout, project: &Project, lake_cache: Option<&Path>) -> Result<()> {
    let root = cache.registry_root();
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    let path = record_path(cache, &project.root);
    let known_cache = || {
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ProjectRecord>(&bytes).ok())
            .and_then(|previous| previous.lake_cache)
    };
    let record = ProjectRecord {
        version: RECORD_VERSION,
        root: project.root.clone(),
        toolchain: project.toolchain.clone(),
        lake_cache: lake_cache.map(Path::to_owned).or_else(known_cache),
        last_used: now_seconds(),
    };
    atomic_file::replace(&path, &serde_json::to_vec_pretty(&record)?)
}

/// Every record, with the file it was read from, in file-name order.
pub fn load(cache: &CacheLayout) -> Result<Vec<(PathBuf, ProjectRecord)>> {
    let root = cache.registry_root();
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for path in record_files(&root)? {
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let record: ProjectRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if record.version != RECORD_VERSION {
            bail!(
                "{} has unsupported record version {}",
                path.display(),
                record.version
            );
        }
        records.push((path, record));
    }
    Ok(records)
}

/// Records that are no longer live, and record writes that were interrupted,
/// both older than `cutoff`.
pub fn gc_candidates(
    cache: &CacheLayout,
    records: &[(PathBuf, ProjectRecord)],
    cutoff: u64,
) -> Result<Vec<GcCandidate>> {
    let mut candidates = Vec::new();
    for (path, record) in records {
        if !record.is_live(cutoff) {
            candidates.push(candidate("project-record", path)?);
        }
    }
    let root = cache.registry_root();
    if root.is_dir() {
        for path in record_files(&root)? {
            if is_staging_file(&path) && modified_seconds(&path) <= cutoff {
                candidates.push(candidate("staging-file", &path)?);
            }
        }
    }
    Ok(candidates)
}

fn candidate(kind: &'static str, path: &Path) -> Result<GcCandidate> {
    let bytes = fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .len();
    Ok(GcCandidate {
        kind,
        path: path.to_owned(),
        bytes,
    })
}

fn record_path(cache: &CacheLayout, root: &Path) -> PathBuf {
    cache.registry_root().join(format!(
        "{}.json",
        digest(root.to_string_lossy().as_bytes())
    ))
}

fn record_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// `atomic_file` stages a record as `.<sha256>.json.lev-tmp-<16 hex>`.
fn is_staging_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix('.'))
        .and_then(|name| name.split_once(".json.lev-tmp-"))
        .is_some_and(|(key, suffix)| is_lower_hex(key, 64) && is_lower_hex(suffix, 16))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{gc_candidates, load, record};
    use crate::cache::{CacheLayout, digest};
    use crate::project::Project;

    fn project(root: &std::path::Path) -> Project {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        Project::discover(root).unwrap()
    }

    #[test]
    fn records_projects_and_keeps_the_known_lake_cache() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let project = project(&temp.path().join("project"));
        let lake = temp.path().join("lake-cache");

        record(&cache, &project, Some(&lake)).unwrap();
        record(&cache, &project, None).unwrap();
        let records = load(&cache).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1.root, project.root);
        assert_eq!(records[0].1.lake_cache.as_deref(), Some(lake.as_path()));
        assert!(
            gc_candidates(&cache, &records, u64::MAX)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn collects_records_of_vanished_projects_and_interrupted_writes() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = temp.path().join("project");
        record(&cache, &project(&root), None).unwrap();
        fs::remove_dir_all(&root).unwrap();
        let staging = cache
            .registry_root()
            .join(format!(".{}.json.lev-tmp-0123456789abcdef", digest(b"x")));
        fs::write(&staging, "{").unwrap();

        let records = load(&cache).unwrap();
        assert_eq!(records.len(), 1);
        assert!(!records[0].1.exists());
        // A recently used record survives until the age gate passes.
        assert_eq!(gc_candidates(&cache, &records, 0).unwrap().len(), 0);
        let candidates = gc_candidates(&cache, &records, u64::MAX).unwrap();
        let kinds = candidates
            .iter()
            .map(|candidate| candidate.kind)
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["project-record", "staging-file"]);
    }

    #[test]
    fn rejects_records_it_cannot_read() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        fs::create_dir_all(cache.registry_root()).unwrap();
        fs::write(cache.registry_root().join("broken.json"), "{not json").unwrap();
        let error = load(&cache).unwrap_err().to_string();
        assert!(error.contains("broken.json"), "{error}");
    }
}
