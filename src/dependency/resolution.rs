//! Cross-project reuse of exact dependency resolutions.
//!
//! A cache key binds the Lake configuration, starting manifest, toolchain, and
//! host platform. A hit skips resolution, but normal checkout verification
//! still runs. `lake-manifest.json` remains the project lock.

use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::cache::{CacheLayout, digest};
use crate::core::atomic_file::replace as atomic_write;
use crate::project::lockfile::LockedEnvironment;

const CACHE_VERSION: u32 = 1;

/// Stable key for one complete set of resolver inputs.
#[derive(Debug, Clone)]
pub struct ResolutionIdentity {
    key: String,
    toolchain: String,
    policy_sha256: String,
    base_manifest_sha256: String,
    os: String,
    arch: String,
}

/// Exclusive writer guard held from lookup through publication.
pub struct ResolutionLock(File);

/// Aggregate verification result used by `lev cache verify`.
#[derive(Debug, Default)]
pub struct VerificationStats {
    pub records: usize,
}

#[derive(Debug, Serialize)]
struct IdentityMaterial<'a> {
    version: u32,
    toolchain: &'a str,
    policy_sha256: &'a str,
    base_manifest_sha256: &'a str,
    os: &'a str,
    arch: &'a str,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedResolution {
    version: u32,
    key: String,
    toolchain: String,
    policy_sha256: String,
    base_manifest_sha256: String,
    os: String,
    arch: String,
    environment: LockedEnvironment,
}

impl ResolutionIdentity {
    /// Build a key from the toolchain, policy, manifest, and host platform.
    pub fn new(toolchain: &str, policy_sha256: &str, base_manifest: &[u8]) -> Result<Self> {
        if toolchain.trim().is_empty() || toolchain.chars().any(char::is_whitespace) {
            bail!("resolution-cache toolchain is malformed");
        }
        validate_digest("environment policy", policy_sha256)?;

        let base_manifest_sha256 = digest(base_manifest);
        let os = std::env::consts::OS.to_owned();
        let arch = std::env::consts::ARCH.to_owned();
        let material = IdentityMaterial {
            version: CACHE_VERSION,
            toolchain,
            policy_sha256,
            base_manifest_sha256: &base_manifest_sha256,
            os: &os,
            arch: &arch,
        };
        let key = digest(&serde_json::to_vec(&material)?);
        Ok(Self {
            key,
            toolchain: toolchain.to_owned(),
            policy_sha256: policy_sha256.to_owned(),
            base_manifest_sha256,
            os,
            arch,
        })
    }

    /// Short stable identifier suitable for verbose diagnostics.
    pub fn short_key(&self) -> &str {
        &self.key[..12]
    }

    /// Serialize access to this resolver input across all source checkouts.
    pub fn lock(&self, cache: &CacheLayout) -> Result<ResolutionLock> {
        cache.ensure()?;
        let path = cache
            .lock_root()
            .join(format!("resolution-{}.lock", self.key));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(ResolutionLock(file))
    }

    /// Load and fully validate a matching cached Lake resolution.
    pub fn load(&self, cache: &CacheLayout) -> Result<Option<LockedEnvironment>> {
        let path = self.path(cache);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let record: CachedResolution = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        record
            .validate()
            .with_context(|| format!("invalid cached resolution {}", path.display()))?;
        if record.key != self.key
            || record.toolchain != self.toolchain
            || record.policy_sha256 != self.policy_sha256
            || record.base_manifest_sha256 != self.base_manifest_sha256
            || record.os != self.os
            || record.arch != self.arch
        {
            bail!(
                "cached resolution identity does not match requested key {}",
                self.key
            );
        }
        Ok(Some(record.environment))
    }

    /// Store a portable resolution. Path dependencies are not portable.
    pub fn store(&self, cache: &CacheLayout, environment: &LockedEnvironment) -> Result<bool> {
        environment.verify_policy(&self.toolchain, &self.policy_sha256)?;
        let platform = format!("{}-{}", self.os, self.arch);
        if environment.platform() != platform {
            bail!(
                "resolved environment platform {} does not match cache platform {platform}",
                environment.platform()
            );
        }
        if !environment.is_resolution_cacheable()? {
            return Ok(false);
        }

        let path = self.path(cache);
        let parent = path
            .parent()
            .context("resolution-cache record has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let record = CachedResolution {
            version: CACHE_VERSION,
            key: self.key.clone(),
            toolchain: self.toolchain.clone(),
            policy_sha256: self.policy_sha256.clone(),
            base_manifest_sha256: self.base_manifest_sha256.clone(),
            os: self.os.clone(),
            arch: self.arch.clone(),
            environment: environment.clone(),
        };
        record.validate()?;
        atomic_write(&path, &serde_json::to_vec_pretty(&record)?)?;
        Ok(true)
    }

    /// Remove one invalid local record before an online fallback resolution.
    pub fn remove(&self, cache: &CacheLayout) -> Result<()> {
        let path = self.path(cache);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to remove {}", path.display()))
            }
        }
    }

    fn path(&self, cache: &CacheLayout) -> PathBuf {
        cache.resolution_root().join(format!("{}.json", self.key))
    }
}

impl CachedResolution {
    fn validate(&self) -> Result<()> {
        if self.version != CACHE_VERSION {
            bail!("unsupported resolution-cache version {}", self.version);
        }
        validate_digest("resolution key", &self.key)?;
        validate_digest("environment policy", &self.policy_sha256)?;
        validate_digest("base manifest", &self.base_manifest_sha256)?;
        if self.toolchain.trim().is_empty()
            || self.toolchain.chars().any(char::is_whitespace)
            || self.os.trim().is_empty()
            || self.arch.trim().is_empty()
        {
            bail!("resolution-cache identity is malformed");
        }

        let material = IdentityMaterial {
            version: CACHE_VERSION,
            toolchain: &self.toolchain,
            policy_sha256: &self.policy_sha256,
            base_manifest_sha256: &self.base_manifest_sha256,
            os: &self.os,
            arch: &self.arch,
        };
        if digest(&serde_json::to_vec(&material)?) != self.key {
            bail!("resolution-cache key does not match its identity fields");
        }
        self.environment
            .verify_policy(&self.toolchain, &self.policy_sha256)?;
        let platform = format!("{}-{}", self.os, self.arch);
        if self.environment.platform() != platform {
            bail!("resolution-cache environment platform does not match its identity");
        }
        if !self.environment.is_resolution_cacheable()? {
            bail!("resolution-cache entry contains location-dependent packages");
        }
        Ok(())
    }
}

/// Verify every persisted resolver result without requiring a project.
pub fn verify(cache: &CacheLayout) -> Result<VerificationStats> {
    let root = cache.resolution_root();
    if !root.is_dir() {
        return Ok(VerificationStats::default());
    }
    let mut records = 0;
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        if !entry.file_type()?.is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let bytes = fs::read(entry.path())
            .with_context(|| format!("failed to read {}", entry.path().display()))?;
        let record: CachedResolution = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", entry.path().display()))?;
        record
            .validate()
            .with_context(|| format!("invalid cached resolution {}", entry.path().display()))?;
        records += 1;
    }
    Ok(VerificationStats { records })
}

/// Return cached resolutions old enough for GC.
///
/// Project locks contain their own manifests, so these records are disposable.
pub fn gc_paths(cache: &CacheLayout, cutoff: u64) -> Result<Vec<PathBuf>> {
    let root = cache.resolution_root();
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        if !entry.file_type()?.is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let modified = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        if modified <= cutoff {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn validate_digest(label: &str, value: &str) -> Result<()> {
    if !crate::core::hex::is_sha256(value) {
        bail!("{label} digest is malformed");
    }
    Ok(())
}

impl Drop for ResolutionLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::tempdir;

    use crate::cache::CacheLayout;
    use crate::project::Project;
    use crate::project::lockfile::LockedEnvironment;

    use super::{ResolutionIdentity, gc_paths, verify};

    #[test]
    fn round_trips_only_portable_matching_resolutions() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let project = make_project(
            temp.path().join("resolved"),
            r#"{"packagesDir":".lake/packages","packages":[],"name":"demo"}"#,
        );
        let policy = "a".repeat(64);
        let identity =
            ResolutionIdentity::new(&project.toolchain, &policy, b"base manifest").unwrap();
        let environment =
            LockedEnvironment::capture(&project, policy.clone(), &BTreeMap::new()).unwrap();

        assert!(identity.store(&cache, &environment).unwrap());
        assert_eq!(identity.load(&cache).unwrap(), Some(environment));
        assert_eq!(verify(&cache).unwrap().records, 1);
        assert_eq!(gc_paths(&cache, u64::MAX).unwrap().len(), 1);

        let other =
            ResolutionIdentity::new(&project.toolchain, &"b".repeat(64), b"base manifest").unwrap();
        assert!(other.load(&cache).unwrap().is_none());
    }

    #[test]
    fn refuses_path_dependent_and_corrupt_records() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let project = make_project(
            temp.path().join("resolved"),
            r#"{
                "packagesDir": ".lake/packages",
                "packages": [{"name":"local","type":"path","dir":"../local"}],
                "name": "demo"
            }"#,
        );
        let policy = "c".repeat(64);
        let identity =
            ResolutionIdentity::new(&project.toolchain, &policy, b"base manifest").unwrap();
        let environment = LockedEnvironment::capture(&project, policy, &BTreeMap::new()).unwrap();
        assert!(!identity.store(&cache, &environment).unwrap());

        let portable = make_project(
            temp.path().join("portable"),
            r#"{"packagesDir":".lake/packages","packages":[],"name":"demo"}"#,
        );
        let policy = "d".repeat(64);
        let identity =
            ResolutionIdentity::new(&portable.toolchain, &policy, b"base manifest").unwrap();
        let environment = LockedEnvironment::capture(&portable, policy, &BTreeMap::new()).unwrap();
        assert!(identity.store(&cache, &environment).unwrap());
        let path = fs::read_dir(cache.resolution_root())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(path, b"{not json").unwrap();
        assert!(identity.load(&cache).is_err());
        assert!(verify(&cache).is_err());
    }

    fn make_project(root: std::path::PathBuf, manifest: &str) -> Project {
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        fs::write(root.join("lakefile.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(root.join("lake-manifest.json"), manifest).unwrap();
        Project::load(root).unwrap()
    }
}
