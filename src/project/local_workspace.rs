//! Persistent cache-local copies of a Lean project.
//!
//! Source files are mirrored into a toolchain-specific directory while build
//! outputs stay local to that copy.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::cache::{CacheLayout, digest};
use crate::core::atomic_file::replace as write_atomic;
use crate::core::clock::modified_seconds;
use crate::core::platform;
use crate::project::Project;
use crate::project::manifest::LakeManifest;

const STATE_VERSION: u32 = 2;
const MAX_SOURCE_FILES: usize = 250_000;
const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct WorkspaceStats {
    pub copied: u64,
    pub reused: u64,
    pub removed: u64,
    pub copied_bytes: u64,
}

pub struct MaterializedWorkspace {
    pub project: Project,
    pub stats: WorkspaceStats,
}

pub fn is_managed(cache: &CacheLayout, project: &Project) -> bool {
    project.root.starts_with(cache.workspace_root())
        && project
            .root
            .parent()
            .is_some_and(|container| container.join("state.json").is_file())
}

pub fn gc_paths(
    cache: &CacheLayout,
    cutoff: u64,
    live: &std::collections::HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    // A workspace matching its source project's current toolchain remains a
    // durable fast path. Other environments are retained only by registry
    // reachability or recent state-file activity.
    let root = cache.workspace_root();
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for project in fs::read_dir(&root)? {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for toolchain in fs::read_dir(project.path())? {
            let toolchain = toolchain?;
            if !toolchain.file_type()?.is_dir() {
                continue;
            }
            let container = toolchain.path();
            if live.contains(&container) {
                continue;
            }
            let state_path = container.join("state.json");
            let state = fs::read(&state_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<WorkspaceState>(&bytes).ok());
            let current_source_pin = state.as_ref().is_some_and(|state| {
                state.workspace_key == state.toolchain
                    && Project::load(state.source.clone())
                        .ok()
                        .is_some_and(|project| project.toolchain == state.toolchain)
            });
            if current_source_pin || modified_seconds(&state_path) >= cutoff {
                continue;
            }
            candidates.push(container);
        }
    }
    candidates.sort();
    Ok(candidates)
}

pub fn source_packages_dir(workspace: &Project) -> Result<Option<PathBuf>> {
    // Artifact reuse may need to inspect the source checkout's dependency
    // tree. State validation prevents a directory that only resembles a
    // managed workspace from redirecting that lookup.
    let Some(container) = workspace.root.parent() else {
        return Ok(None);
    };
    let state_path = container.join("state.json");
    if !state_path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&state_path)
        .with_context(|| format!("failed to read {}", state_path.display()))?;
    let state: WorkspaceState = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", state_path.display()))?;
    if state.version != STATE_VERSION || state.toolchain != workspace.toolchain {
        return Ok(None);
    }
    let manifest_path = state.source.join("lake-manifest.json");
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let manifest = LakeManifest::read(&manifest_path)?;
    Ok(Some(manifest.packages_path(&state.source)?))
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceState {
    version: u32,
    source: PathBuf,
    toolchain: String,
    /// Toolchain name for ordinary local mode, or a dependency-lock digest.
    #[serde(default)]
    workspace_key: String,
    entries: Vec<StateEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StateEntry {
    path: PathBuf,
    kind: EntryKind,
    bytes: u64,
    modified_secs: u64,
    modified_nanos: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum EntryKind {
    File,
    Symlink,
}

pub fn materialize(
    cache: &CacheLayout,
    source: &Project,
    toolchain: &str,
    git: &OsStr,
) -> Result<MaterializedWorkspace> {
    materialize_keyed(cache, source, toolchain, toolchain, git)
}

/// Materialize a project copy isolated by toolchain and dependency lock.
///
/// A changed lock receives a fresh `.lake` build tree. Reusing the same lock
/// returns to the same tree, which makes version switching cheap after the
/// first successful build.
pub fn materialize_keyed(
    cache: &CacheLayout,
    source: &Project,
    toolchain: &str,
    workspace_key: &str,
    git: &OsStr,
) -> Result<MaterializedWorkspace> {
    if workspace_key.trim().is_empty() {
        bail!("local workspace key cannot be empty");
    }
    reject_external_path_dependencies(source)?;
    // Source identity, toolchain, and dependency lock choose independent
    // build trees. Only source files are synchronized between those trees.
    let identity = digest(source.root.to_string_lossy().as_bytes());
    let toolchain_key = digest(toolchain.as_bytes());
    let environment_key = if workspace_key == toolchain {
        toolchain_key
    } else {
        let mut input = toolchain.as_bytes().to_vec();
        input.push(0);
        input.extend_from_slice(workspace_key.as_bytes());
        digest(&input)
    };
    let container = cache
        .workspace_root()
        .join(&identity[..16])
        .join(&environment_key[..16]);
    let root = container.join("project");
    if container.starts_with(&source.root) {
        bail!(
            "local workspace {} is inside the source project; move LEV_CACHE_DIR outside {}",
            container.display(),
            source.root.display()
        );
    }

    cache.ensure()?;
    fs::create_dir_all(&container)
        .with_context(|| format!("failed to create {}", container.display()))?;
    let _lock = workspace_lock(cache, &identity, &environment_key)?;
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;

    let state_path = container.join("state.json");
    let previous = read_state(&state_path, &source.root, toolchain, workspace_key)?;
    let files = source_files(&source.root, git)?;
    let mut stats = WorkspaceStats::default();
    let mut current = Vec::with_capacity(files.len());
    let previous_entries = previous
        .map(|state| {
            state
                .entries
                .into_iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut current_paths = BTreeSet::new();
    let mut source_bytes = 0_u64;

    // State entries use source metadata as the cheap change detector, then
    // confirm that the destination still has the expected kind and size.
    for relative in files {
        validate_relative(&relative)?;
        let source_path = source.root.join(&relative);
        let entry = state_entry(&source_path, relative.clone())?;
        source_bytes = source_bytes
            .checked_add(entry.bytes)
            .context("local workspace source size overflow")?;
        if source_bytes > MAX_SOURCE_BYTES {
            bail!("local workspace source exceeds the 16 GiB safety limit");
        }
        let destination = root.join(&relative);
        let reusable = previous_entries.get(&relative) == Some(&entry)
            && destination_matches(&destination, &entry)?;
        if reusable {
            stats.reused += 1;
        } else {
            copy_entry(&source_path, &destination, &entry)?;
            stats.copied += 1;
            stats.copied_bytes = stats
                .copied_bytes
                .checked_add(entry.bytes)
                .context("local workspace copied size overflow")?;
        }
        current_paths.insert(relative);
        current.push(entry);
    }

    // Remove only paths recorded in the previous state. Files created by Lake
    // inside the managed workspace remain outside this source synchronization
    // contract and are not swept by accident.
    for relative in previous_entries.keys() {
        if current_paths.contains(relative) {
            continue;
        }
        let destination = root.join(relative);
        if remove_managed_entry(&destination)? {
            stats.removed += 1;
            remove_empty_parents(destination.parent(), &root)?;
        }
    }

    write_atomic(
        &root.join("lean-toolchain"),
        format!("{toolchain}\n").as_bytes(),
    )?;
    current.sort_by(|left, right| left.path.cmp(&right.path));
    write_state(
        &state_path,
        &WorkspaceState {
            version: STATE_VERSION,
            source: source.root.clone(),
            toolchain: toolchain.to_owned(),
            workspace_key: workspace_key.to_owned(),
            entries: current,
        },
    )?;
    Ok(MaterializedWorkspace {
        project: Project::load(root)?,
        stats,
    })
}

/// Move a just-resolved workspace under its final dependency-lock key.
///
/// Resolution cannot know the complete lock digest until Lake has produced a
/// manifest. Re-keying preserves any resolver work and avoids retaining a
/// second temporary `.lake` tree beside the environment used for builds.
pub fn rekey(
    cache: &CacheLayout,
    source: &Project,
    workspace: &Project,
    workspace_key: &str,
) -> Result<Project> {
    if workspace_key.trim().is_empty() {
        bail!("local workspace key cannot be empty");
    }
    let old_container = workspace
        .root
        .parent()
        .context("managed workspace has no container directory")?;
    if !is_managed(cache, workspace) {
        bail!(
            "refusing to re-key unmanaged workspace {}",
            workspace.root.display()
        );
    }

    let identity = digest(source.root.to_string_lossy().as_bytes());
    let mut key_input = workspace.toolchain.as_bytes().to_vec();
    key_input.push(0);
    key_input.extend_from_slice(workspace_key.as_bytes());
    let environment_key = digest(&key_input);
    let new_container = cache
        .workspace_root()
        .join(&identity[..16])
        .join(&environment_key[..16]);
    if old_container == new_container {
        return Ok(workspace.clone());
    }

    let _lock = workspace_lock(cache, &identity, &environment_key)?;
    if new_container.exists() {
        fs::remove_dir_all(old_container)
            .with_context(|| format!("failed to remove {}", old_container.display()))?;
        return Project::load(new_container.join("project"));
    }

    let state_path = old_container.join("state.json");
    let bytes = fs::read(&state_path)
        .with_context(|| format!("failed to read {}", state_path.display()))?;
    let mut state: WorkspaceState = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", state_path.display()))?;
    if state.version != STATE_VERSION
        || state.source != source.root
        || state.toolchain != workspace.toolchain
    {
        bail!("managed workspace state changed before it could be re-keyed");
    }
    state.workspace_key = workspace_key.to_owned();
    write_state(&state_path, &state)?;
    fs::rename(old_container, &new_container).with_context(|| {
        format!(
            "failed to move resolved workspace {} to {}",
            old_container.display(),
            new_container.display()
        )
    })?;
    Project::load(new_container.join("project"))
}

fn reject_external_path_dependencies(project: &Project) -> Result<()> {
    let manifest_path = project.manifest_path();
    if !manifest_path.is_file() {
        return Ok(());
    }
    let manifest = LakeManifest::read(&manifest_path)?;
    for package in manifest
        .packages
        .iter()
        .filter(|package| package.kind == "path")
    {
        let directory = package
            .dir
            .as_deref()
            .with_context(|| format!("path dependency {} has no directory", package.name))?;
        let resolved = lexical_normalize(&project.root.join(directory))?;
        let root = lexical_normalize(&project.root)?;
        // Copying just the project root cannot preserve an external relative
        // dependency, and silently resolving it from the cache would change
        // the package graph.
        if !resolved.starts_with(&root) {
            bail!(
                "local workspace cannot reproduce external path dependency {} at {}; run in place",
                package.name,
                directory.display()
            );
        }
    }
    Ok(())
}

fn source_files(root: &Path, git: &OsStr) -> Result<Vec<PathBuf>> {
    // Git excludes ignored build output without walking the entire checkout.
    // The fallback keeps plain directories supported.
    let mut files = git_files(root, git)?.unwrap_or_else(Vec::new);
    if files.is_empty() {
        collect_directory(root, Path::new(""), &mut files)?;
    }
    for name in [
        "lakefile.toml",
        "lakefile.lean",
        "lake-manifest.json",
        "lev.toml",
        "lev.lock",
    ] {
        let path = PathBuf::from(name);
        if root.join(&path).is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
    files.retain(|path| path != Path::new("lean-toolchain") && !is_excluded(path));
    files.sort();
    files.dedup();
    if files.len() > MAX_SOURCE_FILES {
        bail!("local workspace source contains more than {MAX_SOURCE_FILES} files");
    }
    Ok(files)
}

fn git_files(root: &Path, git: &OsStr) -> Result<Option<Vec<PathBuf>>> {
    let output = Command::new(git)
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .arg("-z")
        .arg("--cached")
        .arg("--others")
        .arg("--exclude-standard")
        .output()
        .with_context(|| format!("failed to inspect Git files in {}", root.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let mut files = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = path_from_git(raw)?;
        if is_excluded(&path) {
            continue;
        }
        let source = root.join(&path);
        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("failed to inspect {}", source.display()))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_directory(root, &path, &mut files)?;
        } else {
            files.push(path);
        }
    }
    Ok(Some(files))
}

fn collect_directory(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let directory = root.join(relative);
    let mut entries = fs::read_dir(&directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = relative.join(entry.file_name());
        if is_excluded(&path) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_directory(root, &path, files)?;
        } else if metadata.is_file() || metadata.file_type().is_symlink() {
            files.push(path);
            if files.len() > MAX_SOURCE_FILES {
                bail!("local workspace source contains more than {MAX_SOURCE_FILES} files");
            }
        }
    }
    Ok(())
}

fn is_excluded(path: &Path) -> bool {
    let Some(Component::Normal(first)) = path.components().next() else {
        return true;
    };
    matches!(
        first.to_str(),
        Some(
            ".git"
                | ".lake"
                | ".lev"
                | ".direnv"
                | ".venv"
                | "lake-packages"
                | "lean_packages"
                | "node_modules"
                | "target"
                | "__pycache__"
        )
    )
}

fn state_entry(source: &Path, relative: PathBuf) -> Result<StateEntry> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)
            .with_context(|| format!("failed to read symlink {}", source.display()))?;
        return Ok(StateEntry {
            path: relative,
            kind: EntryKind::Symlink,
            bytes: 0,
            modified_secs: 0,
            modified_nanos: 0,
            target: Some(target),
        });
    }
    if !metadata.is_file() {
        bail!("unsupported source file type: {}", source.display());
    }
    let modified = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    Ok(StateEntry {
        path: relative,
        kind: EntryKind::File,
        bytes: metadata.len(),
        modified_secs: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
        target: None,
    })
}

fn destination_matches(destination: &Path, entry: &StateEntry) -> Result<bool> {
    let Ok(metadata) = fs::symlink_metadata(destination) else {
        return Ok(false);
    };
    match entry.kind {
        EntryKind::File => Ok(metadata.is_file() && metadata.len() == entry.bytes),
        EntryKind::Symlink => {
            if !metadata.file_type().is_symlink() {
                return Ok(false);
            }
            Ok(fs::read_link(destination).ok().as_ref() == entry.target.as_ref())
        }
    }
}

fn copy_entry(source: &Path, destination: &Path, entry: &StateEntry) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if fs::symlink_metadata(destination).is_ok() {
        remove_existing(destination)?;
    }
    match entry.kind {
        EntryKind::File => {
            fs::copy(source, destination).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        }
        EntryKind::Symlink => {
            let target = entry
                .target
                .as_deref()
                .context("workspace symlink has no target")?;
            platform::create_symlink_from_source(target, destination, source)?;
        }
    }
    Ok(())
}

fn remove_existing(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
    }
}

fn remove_managed_entry(path: &Path) -> Result<bool> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(false);
    }
    remove_existing(path)?;
    Ok(true)
}

fn remove_empty_parents(mut current: Option<&Path>, root: &Path) -> Result<()> {
    while let Some(path) = current {
        if path == root || !path.starts_with(root) {
            break;
        }
        if fs::read_dir(path)?.next().is_some() {
            break;
        }
        fs::remove_dir(path).with_context(|| format!("failed to remove {}", path.display()))?;
        current = path.parent();
    }
    Ok(())
}

fn read_state(
    path: &Path,
    source: &Path,
    toolchain: &str,
    workspace_key: &str,
) -> Result<Option<WorkspaceState>> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let state: WorkspaceState = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    // Old or differently keyed state is not migrated in place. Ignoring it
    // forces a fresh comparison while preserving the isolated build tree.
    if state.version != STATE_VERSION
        || state.source != source
        || state.toolchain != toolchain
        || state.workspace_key != workspace_key
    {
        return Ok(None);
    }
    Ok(Some(state))
}

fn write_state(path: &Path, state: &WorkspaceState) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(state)?)
}

fn workspace_lock(cache: &CacheLayout, project: &str, toolchain: &str) -> Result<WorkspaceLock> {
    let path = cache
        .lock_root()
        .join(format!("workspace-{project}-{toolchain}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    FileExt::lock_exclusive(&file).with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(WorkspaceLock(file))
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!("unsafe source path for local workspace: {}", path.display());
    }
    Ok(())
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path escapes its filesystem root: {}", path.display());
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

#[cfg(unix)]
fn path_from_git(raw: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(raw.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git(raw: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(
        String::from_utf8(raw.to_vec()).context("Git returned a non-UTF-8 path")?,
    ))
}

struct WorkspaceLock(File);

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ffi::OsStr;
    use std::fs;

    use tempfile::tempdir;

    use super::{gc_paths, materialize, materialize_keyed};
    use crate::cache::CacheLayout;
    use crate::project::Project;

    #[test]
    fn incrementally_materializes_toolchain_isolated_sources() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("Src")).unwrap();
        fs::write(
            source.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        fs::write(source.join("lakefile.toml"), "name = \"demo\"\n").unwrap();
        fs::write(source.join("Src/Main.lean"), "def value := 1\n").unwrap();
        fs::create_dir_all(source.join(".lake/build")).unwrap();
        fs::write(source.join(".lake/build/generated"), "ignore").unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let project = Project::load(source.clone()).unwrap();

        let first = materialize(
            &cache,
            &project,
            "leanprover/lean4:v4.fixture-c",
            OsStr::new("git"),
        )
        .unwrap();
        assert_eq!(first.project.toolchain, "leanprover/lean4:v4.fixture-c");
        assert_eq!(
            fs::read_to_string(first.project.root.join("Src/Main.lean")).unwrap(),
            "def value := 1\n"
        );
        assert!(!first.project.root.join(".lake/build/generated").exists());
        assert!(first.stats.copied >= 2);

        let second = materialize(
            &cache,
            &project,
            "leanprover/lean4:v4.fixture-c",
            OsStr::new("git"),
        )
        .unwrap();
        assert_eq!(second.stats.copied, 0);
        assert!(second.stats.reused >= 2);

        fs::write(source.join("Src/Main.lean"), "def value := 100\n").unwrap();
        let third = materialize(
            &cache,
            &project,
            "leanprover/lean4:v4.fixture-c",
            OsStr::new("git"),
        )
        .unwrap();
        assert_eq!(third.stats.copied, 1);
        assert_eq!(
            fs::read_to_string(third.project.root.join("Src/Main.lean")).unwrap(),
            "def value := 100\n"
        );

        let other = materialize(
            &cache,
            &project,
            "leanprover/lean4:v4.fixture-b",
            OsStr::new("git"),
        )
        .unwrap();
        assert_ne!(other.project.root, third.project.root);

        let current = materialize(&cache, &project, &project.toolchain, OsStr::new("git")).unwrap();
        let candidates = gc_paths(&cache, u64::MAX, &HashSet::new()).unwrap();
        assert!(candidates.contains(&third.project.root.parent().unwrap().to_owned()));
        assert!(candidates.contains(&other.project.root.parent().unwrap().to_owned()));
        assert!(!candidates.contains(&current.project.root.parent().unwrap().to_owned()));
    }

    #[test]
    fn dependency_lock_keys_isolate_and_reuse_build_trees() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        fs::write(source.join("lakefile.toml"), "name = \"demo\"\n").unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let project = Project::load(source).unwrap();

        let first = materialize_keyed(
            &cache,
            &project,
            "leanprover/lean4:v4.fixture-c",
            "lock-a",
            OsStr::new("git"),
        )
        .unwrap();
        let repeated = materialize_keyed(
            &cache,
            &project,
            "leanprover/lean4:v4.fixture-c",
            "lock-a",
            OsStr::new("git"),
        )
        .unwrap();
        let changed = materialize_keyed(
            &cache,
            &project,
            "leanprover/lean4:v4.fixture-c",
            "lock-b",
            OsStr::new("git"),
        )
        .unwrap();

        assert_eq!(first.project.root, repeated.project.root);
        assert_ne!(first.project.root, changed.project.root);
    }
}
