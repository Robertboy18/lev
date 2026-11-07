//! lev's cache directory and the plan every collector produces.
//!
//! lev keeps little state of its own: a registry of the projects it has built
//! or checked, the lock files that keep `lev cache gc` away from a running
//! build or check, and the exporter builds, references and verdicts of
//! `lev check`. The directory names carry a version; an incompatible layout
//! gets a new name.

pub(crate) mod check;
pub(crate) mod lake;
pub(crate) mod registry;

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::core::platform_dirs;

#[derive(Debug, Clone)]
pub struct CacheLayout {
    pub root: PathBuf,
}

impl CacheLayout {
    /// Use `--cache-dir`, then `LEV_CACHE_DIR`, then the platform default.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let root = if let Some(path) = explicit {
            path
        } else if let Some(path) = std::env::var_os("LEV_CACHE_DIR") {
            PathBuf::from(path)
        } else if let Some(path) = platform_dirs::cache_root() {
            path
        } else {
            bail!("cannot determine the cache directory; set LEV_CACHE_DIR")
        };
        Ok(Self {
            root: crate::project::absolute(&root)?,
        })
    }

    pub fn registry_root(&self) -> PathBuf {
        self.root.join("projects-v2")
    }

    /// The lock lev holds on one Lake cache: shared while it runs a build,
    /// exclusive while it collects garbage.
    pub fn lake_lock_path(&self, lake_cache: &Path) -> PathBuf {
        let key = digest(lake_cache.to_string_lossy().as_bytes());
        self.root
            .join("locks")
            .join(format!("lake-{}.lock", &key[..16]))
    }

    /// The caches of `lev check`.
    pub fn check(&self) -> check::CheckLayout {
        check::CheckLayout {
            root: self.root.join("check-v1"),
            lock: self.root.join("locks").join("check.lock"),
        }
    }
}

/// One file a collector would remove.
#[derive(Debug, Serialize)]
pub struct GcCandidate {
    pub kind: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
}

/// The files a collection removes, listed before anything is deleted.
#[derive(Debug, Default, Serialize)]
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

    /// Remove every candidate, directories with their contents. An entry
    /// that is already gone is not an error.
    pub fn apply(&self) -> Result<()> {
        for candidate in &self.candidates {
            let is_directory =
                fs::symlink_metadata(&candidate.path).is_ok_and(|metadata| metadata.is_dir());
            let removed = if is_directory {
                fs::remove_dir_all(&candidate.path)
            } else {
                fs::remove_file(&candidate.path)
            };
            match removed {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to remove {}", candidate.path.display()));
                }
            }
        }
        Ok(())
    }
}

/// Lowercase SHA-256 of `value`.
pub fn digest(value: &[u8]) -> String {
    crate::core::hex::lowercase(&Sha256::digest(value))
}

/// Lowercase SHA-256 of a file's bytes, read in 1 MiB blocks.
pub fn file_digest(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::core::hex::lowercase(&hasher.finalize()))
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The size of a file, or of every file below a directory. Symbolic links
/// count as zero and are not followed.
pub fn path_bytes(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    let mut bytes = 0;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        bytes += path_bytes(&entry.path())?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{GcCandidate, GcPlan, digest, human_bytes, path_bytes};

    #[test]
    fn digest_is_stable() {
        assert_eq!(
            digest(b"https://example.invalid/repo"),
            "bdb664ccdebb6f2f604126d92cbfa69e4c780b8315ec74cfd4ab9f0385869a58"
        );
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn plans_count_bytes_and_tolerate_vanished_files() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("dir")).unwrap();
        fs::write(temp.path().join("dir/a"), b"abc").unwrap();
        fs::write(temp.path().join("b"), b"de").unwrap();
        assert_eq!(path_bytes(temp.path()).unwrap(), 5);

        let plan = GcPlan {
            candidates: vec![
                GcCandidate {
                    kind: "test",
                    path: temp.path().join("b"),
                    bytes: 2,
                },
                GcCandidate {
                    kind: "test",
                    path: temp.path().join("never-existed"),
                    bytes: 0,
                },
            ],
        };
        assert_eq!(plan.bytes(), 2);
        plan.apply().unwrap();
        assert!(!temp.path().join("b").exists());
        assert!(temp.path().join("dir/a").exists());
    }
}
