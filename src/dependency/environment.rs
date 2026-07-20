//! Shared package trees for matching local workspaces.
//!
//! Keys bind the toolchain, platform, and complete locked Git package set.
//! Existing unmanaged data is left alone.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::cache::{CacheLayout, digest};
use crate::core::atomic_file::replace as atomic_write;
use crate::core::clock::{modified_seconds, write_timestamp};
use crate::project::Project;
use crate::project::local_workspace as workspace;
use crate::project::manifest::LakeManifest;

const STATE_VERSION: u32 = 1;

#[derive(Debug)]
pub struct AttachStats {
    pub key: String,
    pub migrated: bool,
    pub reused: bool,
}

pub struct EnvironmentLock(File);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EnvironmentSpec {
    version: u32,
    toolchain: String,
    os: String,
    arch: String,
    packages: Vec<PackageSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct PackageSpec {
    name: String,
    url: String,
    revision: String,
    sub_dir: Option<PathBuf>,
    config_file: Option<PathBuf>,
    manifest_file: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Attachment {
    version: u32,
    key: String,
}

pub fn attach(
    cache: &CacheLayout,
    project: &Project,
    manifest: &LakeManifest,
) -> Result<Option<(AttachStats, EnvironmentLock)>> {
    // Sharing is an optimization for lev-managed local workspaces only. An
    // in-place project keeps Lake's ordinary private package directory.
    if !workspace::is_managed(cache, project) {
        return Ok(None);
    }
    if !cfg!(unix) {
        remove_attachment(project)?;
        return Ok(None);
    }
    let Some((key, spec)) = environment_spec(project, manifest)? else {
        remove_attachment(project)?;
        return Ok(None);
    };
    let lock = lock_key(cache, &key)?;
    let root = cache.dependency_environment(&key);
    let packages = root.join("packages");
    let project_packages = manifest.packages_path(&project.root)?;
    let previous = read_attachment(project)?;
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    validate_or_write_state(&root.join("state.json"), &spec)?;

    let mut migrated = false;
    let mut reused = packages.is_dir();
    // There are three safe states: our existing link, a private directory we
    // can migrate, or no package path yet. Foreign links and non-empty trees
    // are never replaced just to gain cache reuse.
    match fs::symlink_metadata(&project_packages) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(&project_packages)
                .with_context(|| format!("failed to read {}", project_packages.display()))?;
            if target == packages {
                validate_attachment(&project_packages, &packages)?;
                reused = true;
            } else if previous.as_ref().is_some_and(|attachment| {
                crate::core::hex::is_sha256(&attachment.key)
                    && target
                        == cache
                            .dependency_environment(&attachment.key)
                            .join("packages")
            }) {
                fs::remove_file(&project_packages)
                    .with_context(|| format!("failed to remove {}", project_packages.display()))?;
                let existed = packages.is_dir();
                fs::create_dir_all(&packages)
                    .with_context(|| format!("failed to create {}", packages.display()))?;
                create_directory_link(&packages, &project_packages)?;
                reused = existed;
            } else {
                bail!(
                    "refusing to replace unmanaged dependency link {} -> {}",
                    project_packages.display(),
                    target.display()
                );
            }
        }
        Ok(metadata) if metadata.is_dir() => {
            if !packages.exists() {
                fs::rename(&project_packages, &packages).with_context(|| {
                    format!(
                        "failed to move dependency environment {} to {}",
                        project_packages.display(),
                        packages.display()
                    )
                })?;
                migrated = true;
                reused = false;
            } else if fs::read_dir(&project_packages)?.next().is_none() {
                fs::remove_dir(&project_packages)
                    .with_context(|| format!("failed to remove {}", project_packages.display()))?;
                create_directory_link(&packages, &project_packages)?;
                reused = true;
            } else {
                remove_attachment(project)?;
                return Ok(None);
            }
        }
        Ok(_) => {
            bail!(
                "dependency packages path is not a directory: {}",
                project_packages.display()
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&packages)
                .with_context(|| format!("failed to create {}", packages.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", project_packages.display()));
        }
    }

    if !project_packages.exists() {
        create_directory_link(&packages, &project_packages)?;
    }
    write_attachment(
        project,
        &Attachment {
            version: STATE_VERSION,
            key: key.clone(),
        },
    )?;
    write_timestamp(&root.join(".last-used"))?;
    Ok(Some((
        AttachStats {
            key,
            migrated,
            reused,
        },
        lock,
    )))
}

pub fn lock_attached(cache: &CacheLayout, project: &Project) -> Result<Option<EnvironmentLock>> {
    let path = attachment_path(project);
    if !path.is_file() {
        return Ok(None);
    }
    let attachment: Attachment = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", path.display()))?;
    if attachment.version != STATE_VERSION || !crate::core::hex::is_sha256(&attachment.key) {
        bail!("invalid shared dependency attachment {}", path.display());
    }
    // Keep the environment locked for the caller's build, not merely while
    // validating the symlink. GC and another writer must wait for this guard.
    let lock = lock_key(cache, &attachment.key)?;
    let root = cache.dependency_environment(&attachment.key);
    let packages = root.join("packages");
    let manifest = LakeManifest::read(&project.manifest_path())?;
    validate_attachment(&manifest.packages_path(&project.root)?, &packages)?;
    write_timestamp(&root.join(".last-used"))?;
    Ok(Some(lock))
}

pub fn live_paths(cache: &CacheLayout) -> Result<HashSet<PathBuf>> {
    // Attachment files are the durable reachability edges from managed
    // workspaces to shared package trees. Malformed edges are ignored here
    // and diagnosed when that workspace is opened.
    let mut live = HashSet::new();
    let workspace_root = cache.workspace_root();
    if !workspace_root.is_dir() {
        return Ok(live);
    }
    for project in fs::read_dir(&workspace_root)? {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for toolchain in fs::read_dir(project.path())? {
            let toolchain = toolchain?;
            if !toolchain.file_type()?.is_dir() {
                continue;
            }
            let path = toolchain
                .path()
                .join("project/.lake/lev/dependency-env.json");
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(attachment) = serde_json::from_slice::<Attachment>(&bytes) else {
                continue;
            };
            if attachment.version == STATE_VERSION && crate::core::hex::is_sha256(&attachment.key) {
                live.insert(cache.dependency_environment(&attachment.key));
            }
        }
    }
    Ok(live)
}

pub fn gc_paths(cache: &CacheLayout, cutoff: u64, live: &HashSet<PathBuf>) -> Result<Vec<PathBuf>> {
    let root = cache.dependency_root();
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || live.contains(&entry.path()) {
            continue;
        }
        let freshness = entry.path().join(".last-used");
        let freshness = if freshness.is_file() {
            freshness
        } else {
            entry.path()
        };
        if modified_seconds(&freshness) <= cutoff {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn environment_spec(
    project: &Project,
    manifest: &LakeManifest,
) -> Result<Option<(String, EnvironmentSpec)>> {
    // A nonstandard packagesDir or a non-Git package may have mutation or
    // layout semantics lev cannot safely share, so it stays project-local.
    if manifest.packages_dir != Path::new(".lake/packages") {
        return Ok(None);
    }
    manifest.git_packages()?;
    let mut packages = manifest
        .packages
        .iter()
        .filter(|package| package.kind == "git")
        .map(|package| {
            Ok(PackageSpec {
                name: package.name.clone(),
                url: package
                    .url
                    .clone()
                    .with_context(|| format!("git package {} has no URL", package.name))?,
                revision: package
                    .rev
                    .clone()
                    .with_context(|| format!("git package {} has no revision", package.name))?,
                sub_dir: package.sub_dir.clone(),
                config_file: package.config_file.clone(),
                manifest_file: package.manifest_file.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if packages.is_empty() {
        return Ok(None);
    }
    // Sorting makes the digest independent of Lake's manifest ordering. OS,
    // architecture, and toolchain remain in the key because compiled package
    // outputs are not portable across those boundaries.
    packages.sort();
    let spec = EnvironmentSpec {
        version: STATE_VERSION,
        toolchain: project.toolchain.clone(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        packages,
    };
    let bytes = serde_json::to_vec(&spec)?;
    Ok(Some((digest(&bytes), spec)))
}

fn validate_or_write_state(path: &Path, expected: &EnvironmentSpec) -> Result<()> {
    if path.is_file() {
        let actual: EnvironmentSpec = serde_json::from_slice(
            &fs::read(path).with_context(|| format!("failed to read {}", path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", path.display()))?;
        if &actual != expected {
            // A digest collision is extraordinarily unlikely, but treating a
            // mismatched state as reusable would cross an isolation boundary.
            bail!(
                "shared dependency environment key collision at {}",
                path.display()
            );
        }
        return Ok(());
    }
    atomic_write(path, &serde_json::to_vec_pretty(expected)?)
}

fn validate_attachment(link: &Path, expected: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(link)
        .with_context(|| format!("failed to inspect {}", link.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!("{} is not a shared dependency link", link.display());
    }
    let target =
        fs::read_link(link).with_context(|| format!("failed to read {}", link.display()))?;
    if target != expected {
        bail!(
            "{} points to {}, expected {}",
            link.display(),
            target.display(),
            expected.display()
        );
    }
    if !expected.is_dir() {
        bail!(
            "shared dependency environment is missing: {}",
            expected.display()
        );
    }
    Ok(())
}

fn write_attachment(project: &Project, attachment: &Attachment) -> Result<()> {
    let path = attachment_path(project);
    let parent = path
        .parent()
        .context("dependency attachment has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    atomic_write(&path, &serde_json::to_vec_pretty(attachment)?)
}

fn read_attachment(project: &Project) -> Result<Option<Attachment>> {
    let path = attachment_path(project);
    if !path.is_file() {
        return Ok(None);
    }
    let attachment = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(attachment))
}

fn remove_attachment(project: &Project) -> Result<()> {
    let path = attachment_path(project);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn attachment_path(project: &Project) -> PathBuf {
    project.root.join(".lake/lev/dependency-env.json")
}

fn lock_key(cache: &CacheLayout, key: &str) -> Result<EnvironmentLock> {
    cache.ensure()?;
    let path = cache.lock_root().join(format!("dependency-{key}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    FileExt::lock_exclusive(&file).with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(EnvironmentLock(file))
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) -> Result<()> {
    let parent = link
        .parent()
        .context("dependency packages path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    // Publish through a sibling name so interruption cannot leave a partial
    // attachment at Lake's expected package path.
    let temporary = link.with_extension(format!("lev-link-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    std::os::unix::fs::symlink(target, &temporary)
        .with_context(|| format!("failed to link {}", temporary.display()))?;
    fs::rename(&temporary, link).with_context(|| format!("failed to publish {}", link.display()))
}

#[cfg(not(unix))]
fn create_directory_link(_target: &Path, _link: &Path) -> Result<()> {
    bail!("shared dependency environments are not yet supported on this platform")
}

impl Drop for EnvironmentLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::cache::CacheLayout;
    use crate::project::Project;
    use crate::project::local_workspace as workspace;
    use crate::project::manifest::LakeManifest;

    use super::{attach, gc_paths, live_paths, lock_attached};

    #[test]
    fn shares_exact_dependency_environments_without_discarding_existing_trees() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let manifest = r#"{
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "name": "dep",
                "type": "git",
                "url": "https://example.invalid/dep.git",
                "rev": "0123456789abcdef0123456789abcdef01234567"
            }]
        }"#;

        let first_source = make_source(temp.path(), "first", manifest);
        let first = workspace::materialize(
            &cache,
            &first_source,
            &first_source.toolchain,
            OsStr::new("git"),
        )
        .unwrap()
        .project;
        let artifact = first.root.join(".lake/packages/dep/.lake/build/artifact");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "compiled").unwrap();
        let first_manifest = LakeManifest::read(&first.manifest_path()).unwrap();
        let (first_stats, first_lock) = attach(&cache, &first, &first_manifest).unwrap().unwrap();
        assert!(first_stats.migrated);
        drop(first_lock);

        let first_packages = first.root.join(".lake/packages");
        assert!(
            fs::symlink_metadata(&first_packages)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&artifact).unwrap(), "compiled");

        let second_source = make_source(temp.path(), "second", manifest);
        let second = workspace::materialize(
            &cache,
            &second_source,
            &second_source.toolchain,
            OsStr::new("git"),
        )
        .unwrap()
        .project;
        let second_manifest = LakeManifest::read(&second.manifest_path()).unwrap();
        let (second_stats, second_lock) =
            attach(&cache, &second, &second_manifest).unwrap().unwrap();
        assert!(second_stats.reused);
        assert_eq!(first_stats.key, second_stats.key);
        assert_eq!(
            fs::read_link(first.root.join(".lake/packages")).unwrap(),
            fs::read_link(second.root.join(".lake/packages")).unwrap()
        );
        drop(second_lock);
        assert!(lock_attached(&cache, &second).unwrap().is_some());

        let live = live_paths(&cache).unwrap();
        assert_eq!(live.len(), 1);
        assert!(gc_paths(&cache, u64::MAX, &live).unwrap().is_empty());
        assert_eq!(
            gc_paths(&cache, u64::MAX, &Default::default())
                .unwrap()
                .len(),
            1
        );

        let third_source = make_source(temp.path(), "third", manifest);
        let third = workspace::materialize(
            &cache,
            &third_source,
            &third_source.toolchain,
            OsStr::new("git"),
        )
        .unwrap()
        .project;
        let private = third.root.join(".lake/packages/private");
        fs::create_dir_all(&private).unwrap();
        fs::write(private.join("keep"), "do not discard").unwrap();
        let third_manifest = LakeManifest::read(&third.manifest_path()).unwrap();
        assert!(attach(&cache, &third, &third_manifest).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(private.join("keep")).unwrap(),
            "do not discard"
        );
    }

    fn make_source(root: &Path, name: &str, manifest: &str) -> Project {
        let source = root.join(name);
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        fs::write(source.join("lakefile.toml"), "name = \"demo\"\n").unwrap();
        fs::write(source.join("lake-manifest.json"), manifest).unwrap();
        Project::discover(&source).unwrap()
    }
}
