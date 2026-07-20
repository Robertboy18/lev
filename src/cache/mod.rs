//! Versioned cache paths and aggregate size accounting.
//!
//! Domain modules own the formats stored under these paths. Version suffixes
//! are part of the on-disk contract: incompatible layouts get a new directory
//! instead of silently reinterpreting old cache data.

pub(crate) mod lake_artifacts;
pub(crate) mod registry;
pub(crate) mod remote;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::core::platform_dirs;

#[derive(Debug, Clone)]
pub struct CacheLayout {
    pub root: PathBuf,
}

#[derive(Debug, Default)]
pub struct CacheStats {
    pub bytes: u64,
    pub dependency_environments: u64,
    pub files: u64,
    pub git_mirrors: u64,
    pub lake_toolchains: u64,
    pub projects: u64,
    pub reservoir_bytes: u64,
    pub reservoir_files: u64,
    pub resolution_bytes: u64,
    pub resolution_files: u64,
    pub script_environments: u64,
    pub workspaces: u64,
}

impl CacheLayout {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let root = if let Some(path) = explicit {
            path
        } else if let Some(path) = std::env::var_os("LEV_CACHE_DIR") {
            PathBuf::from(path)
        } else if let Some(path) = platform_dirs::cache_root() {
            path
        } else {
            bail!("cannot determine cache directory; set LEV_CACHE_DIR")
        };

        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .context("failed to determine current directory")?
                .join(root)
        };
        Ok(Self { root })
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(self.git_root())
            .with_context(|| format!("failed to create {}", self.git_root().display()))?;
        fs::create_dir_all(self.lock_root())
            .with_context(|| format!("failed to create {}", self.lock_root().display()))?;
        Ok(())
    }

    pub fn git_root(&self) -> PathBuf {
        self.root.join("git-v1")
    }

    pub fn lock_root(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub fn registry_root(&self) -> PathBuf {
        self.root.join("projects-v1")
    }

    pub fn lake_root(&self) -> PathBuf {
        self.root.join("lake-v1")
    }

    pub fn workspace_root(&self) -> PathBuf {
        self.root.join("workspaces-v1")
    }

    pub fn reservoir_root(&self) -> PathBuf {
        self.root.join("reservoir-v1")
    }

    pub fn dependency_root(&self) -> PathBuf {
        self.root.join("dependencies-v1")
    }

    pub fn resolution_root(&self) -> PathBuf {
        self.root.join("resolutions-v1")
    }

    pub fn script_root(&self) -> PathBuf {
        self.root.join("scripts-v1")
    }

    pub fn script_environment_root(&self) -> PathBuf {
        self.script_root().join("environments")
    }

    pub fn dependency_environment(&self, key: &str) -> PathBuf {
        self.dependency_root().join(key)
    }

    pub fn mirror_path(&self, url: &str) -> PathBuf {
        // The fan-out prefix prevents a large cache from putting every bare
        // mirror in one directory.
        let hash = digest(url.as_bytes());
        self.git_root().join(&hash[..2]).join(format!("{hash}.git"))
    }

    pub fn mirror_lock_path(&self, url: &str) -> PathBuf {
        self.lock_root()
            .join(format!("git-{}.lock", digest(url.as_bytes())))
    }

    pub fn lake_dir(&self, toolchain: &str) -> PathBuf {
        let hash = digest(toolchain.as_bytes());
        self.lake_root().join(&hash[..16])
    }

    pub fn lake_lock_path(&self, toolchain: &str) -> PathBuf {
        let hash = digest(toolchain.as_bytes());
        self.lock_root().join(format!("lake-{}.lock", &hash[..16]))
    }

    pub fn ensure_lake_dir(&self, toolchain: &str) -> Result<PathBuf> {
        let path = self.lake_dir(toolchain);
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(path)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        // One traversal computes byte totals; the direct-child counters below
        // describe logical cache entries rather than every nested directory.
        let mut stats = CacheStats::default();
        if self.root.exists() {
            let reservoir = self.reservoir_root();
            let resolutions = self.resolution_root();
            let roots = StatsRoots {
                reservoir: &reservoir,
                resolutions: &resolutions,
            };
            collect_stats(&self.root, &roots, StatsRegion::default(), &mut stats)?;
        }
        if self.git_root().exists() {
            stats.git_mirrors = self.mirror_paths()?.len() as u64;
        }
        if self.dependency_root().exists() {
            stats.dependency_environments = count_directories(&self.dependency_root())?;
        }
        if self.lake_root().exists() {
            stats.lake_toolchains = count_directories(&self.lake_root())?;
        }
        if self.registry_root().exists() {
            stats.projects = count_json_files(&self.registry_root())?;
        }
        if self.script_environment_root().exists() {
            stats.script_environments = count_directories(&self.script_environment_root())?;
        }
        if self.workspace_root().exists() {
            stats.workspaces = count_workspaces(&self.workspace_root())?;
        }
        Ok(stats)
    }

    pub fn mirror_paths(&self) -> Result<Vec<PathBuf>> {
        let mut mirrors = Vec::new();
        let root = self.git_root();
        if !root.exists() {
            return Ok(mirrors);
        }
        for prefix in
            fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let prefix =
                prefix.with_context(|| format!("failed to read entry in {}", root.display()))?;
            if !prefix
                .file_type()
                .with_context(|| format!("failed to inspect {}", prefix.path().display()))?
                .is_dir()
            {
                continue;
            }
            for entry in fs::read_dir(prefix.path())
                .with_context(|| format!("failed to read {}", prefix.path().display()))?
            {
                let entry = entry.with_context(|| {
                    format!("failed to read entry in {}", prefix.path().display())
                })?;
                if entry
                    .file_type()
                    .with_context(|| format!("failed to inspect {}", entry.path().display()))?
                    .is_dir()
                    && entry.path().extension().is_some_and(|value| value == "git")
                {
                    mirrors.push(entry.path());
                }
            }
        }
        mirrors.sort();
        Ok(mirrors)
    }
}

pub fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

/// Encode bytes as allocation-sized lowercase hexadecimal text.
///
/// Random staging names use this helper instead of repeatedly allocating one
/// temporary `String` per byte through `format!`.
pub fn lowercase_hex(value: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
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

struct StatsRoots<'a> {
    reservoir: &'a Path,
    resolutions: &'a Path,
}

#[derive(Clone, Copy, Default)]
struct StatsRegion {
    reservoir: bool,
    resolutions: bool,
}

fn collect_stats(
    path: &Path,
    roots: &StatsRoots<'_>,
    region: StatsRegion,
    stats: &mut CacheStats,
) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        let metadata = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if metadata.is_dir() {
            let entry_path = entry.path();
            let child_region = StatsRegion {
                reservoir: region.reservoir || entry_path == roots.reservoir,
                resolutions: region.resolutions || entry_path == roots.resolutions,
            };
            collect_stats(&entry_path, roots, child_region, stats)?;
        } else if metadata.is_file() {
            let bytes = entry
                .metadata()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?
                .len();
            stats.files += 1;
            stats.bytes += bytes;
            if region.reservoir {
                stats.reservoir_files += 1;
                stats.reservoir_bytes += bytes;
            }
            if region.resolutions {
                stats.resolution_files += 1;
                stats.resolution_bytes += bytes;
            }
        }
    }
    Ok(())
}

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

fn count_directories(path: &Path) -> Result<u64> {
    let mut count = 0;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            count += 1;
        }
    }
    Ok(count)
}

fn count_json_files(path: &Path) -> Result<u64> {
    let mut count = 0;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|value| value == "json")
        {
            count += 1;
        }
    }
    Ok(count)
}

fn count_workspaces(path: &Path) -> Result<u64> {
    let mut count = 0;
    for project in
        fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
    {
        let project =
            project.with_context(|| format!("failed to read entry in {}", path.display()))?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for toolchain in fs::read_dir(project.path())? {
            let toolchain = toolchain?;
            if toolchain.file_type()?.is_dir() && toolchain.path().join("project").is_dir() {
                count += 1;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::{digest, human_bytes, lowercase_hex};

    #[test]
    fn digest_is_stable() {
        assert_eq!(
            digest(b"https://example.invalid/repo"),
            "bdb664ccdebb6f2f604126d92cbfa69e4c780b8315ec74cfd4ab9f0385869a58"
        );
        assert_eq!(lowercase_hex(&[]), "");
        assert_eq!(lowercase_hex(&[0x00, 0x09, 0xaf, 0xff]), "0009afff");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }
}
