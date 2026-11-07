//! The caches of `lev check`, as `lev cache status` and `lev cache gc` see
//! them.
//!
//! ```text
//! check-v1/exporters/<toolchain>-<key>/   lev-export, built with that toolchain
//! check-v1/references/<toolchain>-<key>/  lev-export's export of the toolchain's Init
//! check-v1/verdicts/                      kernel verdicts and reference pins
//! check-v1/scratch/                       exports and slice files of running checks
//! ```
//!
//! An exporter build and its reference are keyed by the toolchain and a hash
//! of the exporter's sources, and record their last use in a `last-used`
//! file. A check touches every verdict it reuses. Collection removes entries
//! that were not used within the age gate.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::{GcCandidate, path_bytes};
use crate::core::clock::modified_seconds;

/// The file whose modification time is an entry's last use.
pub const LAST_USED: &str = "last-used";

#[derive(Debug, Clone)]
pub struct CheckLayout {
    pub root: PathBuf,
    /// Held shared by `lev check` and exclusive by `lev cache gc`.
    pub lock: PathBuf,
}

impl CheckLayout {
    pub fn exporters(&self) -> PathBuf {
        self.root.join("exporters")
    }

    pub fn references(&self) -> PathBuf {
        self.root.join("references")
    }

    pub fn verdicts(&self) -> PathBuf {
        self.root.join("verdicts")
    }

    pub fn scratch(&self) -> PathBuf {
        self.root.join("scratch")
    }

    /// The lock that serializes building one exporter or reference entry.
    pub fn entry_lock(&self, name: &str) -> PathBuf {
        self.lock.with_file_name(format!("check-{name}.lock"))
    }
}

/// Record that the entry at `dir` was used now.
pub fn touch(dir: &Path) -> Result<()> {
    let path = dir.join(LAST_USED);
    fs::write(&path, b"").with_context(|| format!("failed to write {}", path.display()))
}

/// How many entries of one kind there are, and their size.
#[derive(Debug, Default, Serialize)]
pub struct Group {
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Inventory {
    pub path: PathBuf,
    pub exists: bool,
    pub exporters: Group,
    pub references: Group,
    pub verdicts: Group,
    /// Leftovers of checks that are running or were interrupted.
    pub scratch: Group,
}

pub fn inventory(layout: &CheckLayout) -> Result<Inventory> {
    let mut inventory = Inventory {
        path: layout.root.clone(),
        exists: layout.root.is_dir(),
        ..Inventory::default()
    };
    inventory.exporters = group(&children(&layout.exporters())?)?;
    inventory.references = group(&children(&layout.references())?)?;
    inventory.scratch = group(&children(&layout.scratch())?)?;
    let mut files = Vec::new();
    walk_files(&layout.verdicts(), &mut files)?;
    inventory.verdicts = group(&files)?;
    Ok(inventory)
}

/// Exporter builds, references, verdicts and scratch entries last used at
/// or before `cutoff`.
pub fn gc_candidates(layout: &CheckLayout, cutoff: u64) -> Result<Vec<GcCandidate>> {
    let mut candidates = Vec::new();
    for (dir, kind) in [
        (layout.exporters(), "check-exporter"),
        (layout.references(), "check-reference"),
    ] {
        for entry in children(&dir)? {
            let stamp = entry.join(LAST_USED);
            let used = if stamp.is_file() {
                modified_seconds(&stamp)
            } else {
                modified_seconds(&entry)
            };
            if used <= cutoff {
                candidates.push(candidate(kind, entry)?);
            }
        }
    }
    let mut files = Vec::new();
    walk_files(&layout.verdicts(), &mut files)?;
    for file in files {
        if modified_seconds(&file) <= cutoff {
            candidates.push(candidate("check-verdict", file)?);
        }
    }
    for entry in children(&layout.scratch())? {
        if modified_seconds(&entry) <= cutoff {
            candidates.push(candidate("check-scratch", entry)?);
        }
    }
    Ok(candidates)
}

fn candidate(kind: &'static str, path: PathBuf) -> Result<GcCandidate> {
    Ok(GcCandidate {
        kind,
        bytes: path_bytes(&path)?,
        path,
    })
}

fn group(paths: &[PathBuf]) -> Result<Group> {
    let mut group = Group::default();
    for path in paths {
        group.entries += 1;
        group.bytes += path_bytes(path)?;
    }
    Ok(group)
}

/// The entries of `dir` in name order; none when it does not exist.
fn children(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", dir.display()));
        }
    };
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", dir.display()))?;
    paths.sort();
    Ok(paths)
}

/// Every file below `dir`, without following symbolic links.
fn walk_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for path in children(dir)? {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.is_dir() {
            walk_files(&path, files)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{CheckLayout, gc_candidates, inventory, touch};

    #[test]
    fn inventories_and_ages_every_kind_of_entry() {
        let temp = tempdir().unwrap();
        let layout = CheckLayout {
            root: temp.path().join("check-v1"),
            lock: temp.path().join("locks/check.lock"),
        };
        assert!(!inventory(&layout).unwrap().exists);
        assert!(gc_candidates(&layout, u64::MAX).unwrap().is_empty());

        let exporter = layout.exporters().join("toolchain-0123");
        fs::create_dir_all(exporter.join("bin")).unwrap();
        fs::write(exporter.join("bin/lev-export"), b"exe").unwrap();
        touch(&exporter).unwrap();
        let reference = layout.references().join("toolchain-0123");
        fs::create_dir_all(&reference).unwrap();
        fs::write(reference.join("manifest.json"), b"{}").unwrap();
        let verdict = layout.verdicts().join("identity/ab/abcd.json");
        fs::create_dir_all(verdict.parent().unwrap()).unwrap();
        fs::write(&verdict, b"{\"v\":1}").unwrap();
        fs::create_dir_all(layout.scratch().join("export-x")).unwrap();

        let inventory = inventory(&layout).unwrap();
        assert!(inventory.exists);
        assert_eq!(
            (inventory.exporters.entries, inventory.exporters.bytes),
            (1, 3)
        );
        assert_eq!(
            (inventory.references.entries, inventory.references.bytes),
            (1, 2)
        );
        assert_eq!(
            (inventory.verdicts.entries, inventory.verdicts.bytes),
            (1, 7)
        );
        assert_eq!(inventory.scratch.entries, 1);

        // Everything was used just now, so a cutoff in the past keeps it all.
        assert!(gc_candidates(&layout, 0).unwrap().is_empty());
        let kinds = gc_candidates(&layout, u64::MAX)
            .unwrap()
            .into_iter()
            .map(|candidate| (candidate.kind, candidate.path))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                ("check-exporter", exporter),
                ("check-reference", reference),
                ("check-verdict", verdict),
                ("check-scratch", layout.scratch().join("export-x")),
            ]
        );
    }
}
