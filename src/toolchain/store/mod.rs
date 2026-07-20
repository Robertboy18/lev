//! Content-addressed storage for Lean toolchains.
//!
//! Files are keyed by bytes and executable mode, then hard-linked into
//! read-only views. Store manifests, not elan links, define installed state.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::digest;
use crate::core::clock::{now_nanos, now_seconds};
use crate::core::file_hash;
use crate::core::platform;
use crate::core::platform_dirs;

mod archive;
mod manifest;

use manifest::{MANIFEST_VERSION, ToolchainInstallation};

const MAX_ARCHIVE_ENTRIES: u64 = 2_000_000;
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ToolchainStore {
    pub root: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub alias: String,
    pub view: PathBuf,
    pub files: u64,
    pub logical_bytes: u64,
    pub new_object_bytes: u64,
    pub reused_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct StoredToolchain {
    pub source_toolchain: String,
    pub alias: String,
    pub view: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveProvenance {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub verified: bool,
}

/// Provenance recorded for a toolchain reconstructed from signed chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkProvenance {
    /// Configured immutable-object remote, without credentials.
    pub remote: String,
    /// Relative signed-manifest object fetched from the remote.
    pub manifest: String,
    /// SHA-256 of the exact signed manifest bytes.
    pub manifest_sha256: String,
    /// SHA-256 fingerprint of the explicit Ed25519 trust anchor.
    pub signing_key_fingerprint: String,
    /// Native platform bound by the manifest.
    pub platform: String,
}

/// Existing immutable file object that may supply chunks for a new version.
#[derive(Debug, Clone)]
pub struct StoredFileSource {
    pub object: PathBuf,
    pub mode: u32,
    pub hash: String,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct StoreStats {
    pub manifests: u64,
    pub views: u64,
    pub objects: u64,
    pub logical_bytes: u64,
    pub object_bytes: u64,
}

#[derive(Debug, Default)]
pub struct VerifyStats {
    pub manifests: u64,
    pub views: u64,
    pub objects: u64,
}

#[derive(Debug, Default)]
pub struct StoreGcReport {
    pub manifests: u64,
    pub views: u64,
    pub objects: u64,
    pub object_bytes: u64,
}

/// Prepared direct-download import returned by [`ToolchainStore::prepare_tar`].
pub type PendingToolchainImport = archive::PendingToolchainImport;

#[derive(Debug)]
struct ToolchainManifest {
    version: u32,
    source_toolchain: String,
    alias: String,
    fingerprint: String,
    view: PathBuf,
    logical_bytes: u64,
    created_at: u64,
    archive: Option<ArchiveProvenance>,
    chunks: Option<ChunkProvenance>,
    entries: Vec<StoreEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum StoreEntry {
    Directory {
        path: PathBuf,
    },
    File {
        path: PathBuf,
        mode: u32,
        hash: String,
        bytes: u64,
    },
    Symlink {
        path: PathBuf,
        target: PathBuf,
    },
}

struct ImportState<'a> {
    store: &'a ToolchainStore,
    stage: &'a Path,
    entries: Vec<StoreEntry>,
    logical_bytes: u64,
    new_object_bytes: u64,
    reused_bytes: u64,
    files: u64,
}

impl ToolchainStore {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let root = if let Some(path) = explicit {
            path
        } else if let Some(path) = std::env::var_os("LEV_DATA_DIR") {
            PathBuf::from(path)
        } else if let Some(path) = platform_dirs::data_root() {
            path
        } else {
            bail!("cannot determine data directory; set LEV_DATA_DIR")
        };
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .context("failed to determine current directory")?
                .join(root)
        };
        Ok(Self {
            root: root.join("toolchains-v1"),
        })
    }

    pub fn find_source(&self, toolchain: &str) -> Result<Option<StoredToolchain>> {
        self.find_matching(|manifest| manifest.source_toolchain == toolchain)
    }

    /// Resolve either a canonical source name or lev's compatibility alias.
    pub fn find(&self, selector: &str) -> Result<Option<StoredToolchain>> {
        self.find_matching(|manifest| {
            manifest.source_toolchain == selector || manifest.alias == selector
        })
    }

    /// Resolve a toolchain installed from an official release archive.
    ///
    /// An explicit direct-backend request must not be satisfied by an
    /// arbitrary imported tree or by a differently trusted chunk publisher.
    /// Unverified archive records are eligible only when the caller repeats
    /// the corresponding opt-in.
    pub fn find_direct(
        &self,
        selector: &str,
        allow_unverified: bool,
    ) -> Result<Option<StoredToolchain>> {
        self.find_matching(|manifest| {
            (manifest.source_toolchain == selector || manifest.alias == selector)
                && manifest
                    .archive
                    .as_ref()
                    .is_some_and(|archive| archive.verified || allow_unverified)
        })
    }

    fn find_matching(
        &self,
        mut eligible: impl FnMut(&ToolchainInstallation) -> bool,
    ) -> Result<Option<StoredToolchain>> {
        let manifest = self
            .read_installations()?
            .into_iter()
            .filter(|manifest| manifest.view.is_dir() && eligible(manifest))
            .max_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.fingerprint.cmp(&right.fingerprint))
            });
        Ok(manifest.map(|manifest| StoredToolchain {
            source_toolchain: manifest.source_toolchain,
            alias: manifest.alias,
            view: manifest.view,
        }))
    }

    /// List the newest installed view for every canonical source toolchain.
    pub fn installed(&self) -> Result<Vec<StoredToolchain>> {
        let mut latest = HashMap::<String, ToolchainInstallation>::new();
        for manifest in self.read_installations()? {
            if !manifest.view.is_dir() {
                continue;
            }
            let replace = latest
                .get(&manifest.source_toolchain)
                .is_none_or(|current| {
                    (manifest.created_at, &manifest.fingerprint)
                        > (current.created_at, &current.fingerprint)
                });
            if replace {
                latest.insert(manifest.source_toolchain.clone(), manifest);
            }
        }
        let mut installed = latest
            .into_values()
            .map(|manifest| StoredToolchain {
                source_toolchain: manifest.source_toolchain,
                alias: manifest.alias,
                view: manifest.view,
            })
            .collect::<Vec<_>>();
        installed.sort_by(|left, right| left.source_toolchain.cmp(&right.source_toolchain));
        Ok(installed)
    }

    /// Remove installation records selected by canonical name or lev alias.
    ///
    /// Shared views and objects are reclaimed afterward only when no remaining
    /// installed manifest references them.
    pub fn remove(&self, selector: &str) -> Result<u64> {
        self.ensure()?;
        let removed = {
            let _lock = self.import_lock()?;
            let records = self.read_installation_records()?;
            let paths = records
                .into_iter()
                .filter_map(|(path, manifest)| {
                    (manifest.source_toolchain == selector || manifest.alias == selector)
                        .then_some(path)
                })
                .collect::<Vec<_>>();
            for path in &paths {
                fs::remove_file(path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            paths.len() as u64
        };
        if removed > 0 {
            self.gc(true)?;
        }
        Ok(removed)
    }

    pub fn import(&self, toolchain: &str, source: &Path) -> Result<ImportResult> {
        self.import_tree(toolchain, source, None)
    }

    /// Import a verified reconstructed tree and retain its signed provenance.
    pub fn import_chunks(
        &self,
        toolchain: &str,
        source: &Path,
        provenance: ChunkProvenance,
    ) -> Result<ImportResult> {
        validate_chunk_provenance(&provenance)?;
        self.import_tree(toolchain, source, Some(provenance))
    }

    fn import_tree(
        &self,
        toolchain: &str,
        source: &Path,
        chunks: Option<ChunkProvenance>,
    ) -> Result<ImportResult> {
        if !source.join("bin/lean").is_file() && !source.join("bin/lean.exe").is_file() {
            bail!("{} is not a Lean toolchain root", source.display());
        }
        self.ensure()?;
        let _lock = self.import_lock()?;
        let alias = alias_for(toolchain);
        let stage = self.create_import_stage()?;

        let result = (|| {
            let mut state = ImportState::new(self, &stage);
            import_directory(source, Path::new(""), &mut state)?;
            let fingerprint = fingerprint(&state.entries);
            let view = self.views_root().join(&fingerprint);
            publish_toolchain_view(&stage, &view)?;

            let manifest = ToolchainManifest {
                version: MANIFEST_VERSION,
                source_toolchain: toolchain.to_owned(),
                alias: alias.clone(),
                fingerprint: fingerprint.clone(),
                view: view.clone(),
                logical_bytes: state.logical_bytes,
                created_at: now_seconds(),
                archive: None,
                chunks,
                entries: state.entries,
            };
            self.write_manifest(&manifest)?;
            Ok(ImportResult {
                alias,
                view,
                files: state.files,
                logical_bytes: state.logical_bytes,
                new_object_bytes: state.new_object_bytes,
                reused_bytes: state.reused_bytes,
            })
        })();

        if result.is_err() && stage.exists() {
            let _ = make_tree_writable(&stage);
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    /// Hold a shared store lock while inspecting reusable object files.
    ///
    /// Import and GC take the same lock exclusively. Callers must release this
    /// guard before invoking [`ToolchainStore::import`] or
    /// [`ToolchainStore::import_chunks`].
    pub fn read_lock(&self) -> Result<ToolchainStoreReadLock> {
        self.ensure()?;
        let file = self.open_lock_file()?;
        FileExt::lock_shared(&file)
            .with_context(|| format!("failed to lock {}", self.import_lock_path().display()))?;
        Ok(ToolchainStoreReadLock(file))
    }

    /// Locate the exact complete object described by a signed file record.
    pub fn existing_object(&self, mode: u32, hash: &str, bytes: u64) -> Result<Option<PathBuf>> {
        validate_object_identity(mode, hash)?;
        let object = self.object_path(mode, hash);
        match fs::symlink_metadata(&object) {
            Ok(metadata) if metadata.file_type().is_file() => {
                if metadata.len() != bytes {
                    bail!("toolchain object has the wrong size: {}", object.display());
                }
                Ok(Some(object))
            }
            Ok(_) => bail!(
                "toolchain object is not a regular file: {}",
                object.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect {}", object.display()))
            }
        }
    }

    /// Return older immutable objects recorded at the same toolchain path.
    ///
    /// Content-defined chunk installation uses these candidates to recover
    /// matching byte ranges locally before requesting missing chunks.
    pub fn file_sources(&self, relative: &Path) -> Result<Vec<StoredFileSource>> {
        if !is_relative_store_path(relative) {
            bail!("invalid reusable toolchain path {}", relative.display());
        }
        let mut sources = HashMap::<(u32, String), StoredFileSource>::new();
        for manifest in self.read_manifests()? {
            for entry in manifest.entries {
                let StoreEntry::File {
                    path,
                    mode,
                    hash,
                    bytes,
                } = entry
                else {
                    continue;
                };
                if path != relative {
                    continue;
                }
                let Some(object) = self.existing_object(mode, &hash, bytes)? else {
                    continue;
                };
                sources
                    .entry((mode, hash.clone()))
                    .or_insert(StoredFileSource {
                        object,
                        mode,
                        hash,
                        bytes,
                    });
            }
        }
        let mut sources = sources.into_values().collect::<Vec<_>>();
        sources.sort_by(|left, right| {
            left.hash
                .cmp(&right.hash)
                .then_with(|| left.mode.cmp(&right.mode))
        });
        Ok(sources)
    }

    pub fn stats(&self) -> Result<StoreStats> {
        // Status is an integrity-facing operation rather than a launch path,
        // so retain full manifest validation before reporting aggregate size.
        let manifests = self.read_manifests()?;
        let mut stats = StoreStats {
            manifests: manifests.len() as u64,
            logical_bytes: manifests
                .iter()
                .map(|manifest| manifest.logical_bytes)
                .sum(),
            ..StoreStats::default()
        };
        if self.views_root().exists() {
            stats.views = count_directories(&self.views_root())?;
        }
        let objects = self.object_paths()?;
        stats.objects = objects.len() as u64;
        stats.object_bytes = objects.iter().try_fold(0_u64, |total, object| {
            total
                .checked_add(fs::metadata(object)?.len())
                .context("toolchain object size overflow")
        })?;
        Ok(stats)
    }

    pub fn verify(&self) -> Result<VerifyStats> {
        let manifests = self.read_manifests()?;
        let mut verified_objects = HashSet::new();
        let mut verified_views = HashSet::new();

        for manifest in &manifests {
            if verified_views.insert(manifest.view.clone()) {
                verify_directory_mode(&manifest.view)?;
            }
            for entry in &manifest.entries {
                match entry {
                    StoreEntry::Directory { path } => {
                        let directory = manifest.view.join(path);
                        if !directory.is_dir() {
                            bail!(
                                "missing directory {} in {}",
                                path.display(),
                                manifest.view.display()
                            );
                        }
                        verify_directory_mode(&directory)?;
                    }
                    StoreEntry::File {
                        path,
                        mode,
                        hash,
                        bytes,
                    } => {
                        let object = self.object_path(*mode, hash);
                        if verified_objects.insert(object.clone()) {
                            let metadata = fs::metadata(&object).with_context(|| {
                                format!("failed to inspect {}", object.display())
                            })?;
                            if metadata.len() != *bytes {
                                bail!("toolchain object has the wrong size: {}", object.display());
                            }
                            verify_mode(&object, *mode)?;
                            let actual = file_hash::sha256(&object)?;
                            if actual != *hash {
                                bail!("toolchain object hash mismatch: {}", object.display());
                            }
                        }
                        let materialized = manifest.view.join(path);
                        verify_hard_link(&object, &materialized)?;
                    }
                    StoreEntry::Symlink { path, target } => {
                        let materialized = manifest.view.join(path);
                        let actual = fs::read_link(&materialized).with_context(|| {
                            format!("failed to read symlink {}", materialized.display())
                        })?;
                        if actual != *target {
                            bail!("toolchain symlink mismatch: {}", materialized.display());
                        }
                    }
                }
            }
        }

        Ok(VerifyStats {
            manifests: manifests.len() as u64,
            views: verified_views.len() as u64,
            objects: verified_objects.len() as u64,
        })
    }

    /// Collect superseded manifests and data unreachable from installed ones.
    ///
    /// The newest manifest for each canonical source toolchain is live. Older
    /// imports of that same source are replaceable history; manifests for
    /// different versions remain independently installed until `remove`.
    pub fn gc(&self, apply: bool) -> Result<StoreGcReport> {
        self.ensure()?;
        let _lock = self.import_lock()?;
        let records = self.read_manifest_records()?;
        let mut newest = HashMap::<String, (u64, String)>::new();
        for (_, manifest) in &records {
            let candidate = (manifest.created_at, manifest.fingerprint.clone());
            if newest
                .get(&manifest.source_toolchain)
                .is_none_or(|current| candidate > *current)
            {
                newest.insert(manifest.source_toolchain.clone(), candidate);
            }
        }

        let mut live_views = HashSet::new();
        let mut live_objects = HashSet::new();
        let mut live_manifest_entries = HashSet::new();
        let mut stale_manifests = self.temporary_manifest_paths()?;

        for (path, manifest) in records {
            let identity = (manifest.created_at, manifest.fingerprint.clone());
            if newest.get(&manifest.source_toolchain) == Some(&identity) {
                if manifest.version == MANIFEST_VERSION {
                    live_manifest_entries.insert(self.manifest_entries_path(&path)?);
                }
                live_views.insert(
                    fs::canonicalize(&manifest.view).unwrap_or_else(|_| manifest.view.clone()),
                );
                for entry in &manifest.entries {
                    if let StoreEntry::File { mode, hash, .. } = entry {
                        live_objects.insert(self.object_path(*mode, hash));
                    }
                }
            } else {
                stale_manifests.push(path);
            }
        }
        let stale_manifest_entries = self.stale_manifest_entry_paths(&live_manifest_entries)?;

        let mut stale_views = Vec::new();
        if self.views_root().exists() {
            for entry in fs::read_dir(self.views_root())? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let canonical = fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path());
                if !live_views.contains(&canonical) {
                    stale_views.push(entry.path());
                }
            }
        }

        let mut stale_objects = Vec::new();
        let mut object_bytes = 0;
        for object in self.object_paths()? {
            if !live_objects.contains(&object) {
                object_bytes += fs::metadata(&object)?.len();
                stale_objects.push(object);
            }
        }

        let report = StoreGcReport {
            manifests: stale_manifests.len() as u64,
            views: stale_views.len() as u64,
            objects: stale_objects.len() as u64,
            object_bytes,
        };
        if apply {
            for view in stale_views {
                make_tree_writable(&view)?;
                fs::remove_dir_all(&view)
                    .with_context(|| format!("failed to remove {}", view.display()))?;
            }
            for manifest in stale_manifests {
                fs::remove_file(&manifest)
                    .with_context(|| format!("failed to remove {}", manifest.display()))?;
            }
            for entries in stale_manifest_entries {
                fs::remove_file(&entries)
                    .with_context(|| format!("failed to remove {}", entries.display()))?;
            }
            for object in stale_objects {
                fs::remove_file(&object)
                    .with_context(|| format!("failed to remove {}", object.display()))?;
            }
            remove_empty_directories(&self.objects_root())?;
        }
        Ok(report)
    }

    fn ensure(&self) -> Result<()> {
        for path in [
            self.objects_root(),
            self.views_root(),
            self.manifests_root(),
            self.manifest_entries_root(),
            self.root.join("locks"),
        ] {
            fs::create_dir_all(&path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        Ok(())
    }

    fn objects_root(&self) -> PathBuf {
        self.root.join("objects")
    }

    fn views_root(&self) -> PathBuf {
        self.root.join("views")
    }

    fn manifests_root(&self) -> PathBuf {
        self.root.join("manifests")
    }

    fn object_path(&self, mode: u32, hash: &str) -> PathBuf {
        self.objects_root()
            .join(format!("{mode:04o}"))
            .join(&hash[..2])
            .join(hash)
    }

    fn create_import_stage(&self) -> Result<PathBuf> {
        let stage = self
            .views_root()
            .join(format!(".tmp-{}-{}", std::process::id(), now_nanos()));
        fs::create_dir(&stage).with_context(|| format!("failed to create {}", stage.display()))?;
        Ok(stage)
    }

    fn import_lock(&self) -> Result<StoreLock> {
        let path = self.import_lock_path();
        let file = self.open_lock_file()?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(StoreLock(file))
    }

    fn import_lock_path(&self) -> PathBuf {
        self.root.join("locks/import.lock")
    }

    fn open_lock_file(&self) -> Result<File> {
        let path = self.import_lock_path();
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))
    }

    fn object_paths(&self) -> Result<Vec<PathBuf>> {
        let root = self.objects_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut objects = Vec::new();
        for mode in fs::read_dir(&root)? {
            let mode = mode?;
            if mode.file_type()?.is_file() {
                objects.push(mode.path());
                continue;
            }
            if !mode.file_type()?.is_dir() {
                continue;
            }
            for prefix in fs::read_dir(mode.path())? {
                let prefix = prefix?;
                if !prefix.file_type()?.is_dir() {
                    continue;
                }
                for object in fs::read_dir(prefix.path())? {
                    let object = object?;
                    if object.file_type()?.is_file() {
                        objects.push(object.path());
                    }
                }
            }
        }
        Ok(objects)
    }
}

impl<'a> ImportState<'a> {
    fn new(store: &'a ToolchainStore, stage: &'a Path) -> Self {
        Self {
            store,
            stage,
            entries: Vec::new(),
            logical_bytes: 0,
            new_object_bytes: 0,
            reused_bytes: 0,
            files: 0,
        }
    }

    fn import_file(
        &mut self,
        source: &Path,
        relative: &Path,
        metadata: &fs::Metadata,
    ) -> Result<()> {
        let hash = file_hash::sha256(source)?;
        let mode = platform::read_only_mode(metadata);
        let bytes = metadata.len();
        let object = self.store.object_path(mode, &hash);
        if object.exists() {
            if fs::metadata(&object)
                .with_context(|| format!("failed to inspect {}", object.display()))?
                .len()
                != bytes
            {
                bail!("toolchain object has the wrong size: {}", object.display());
            }
            self.reused_bytes += bytes;
        } else {
            if let Some(parent) = object.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            let temporary =
                object.with_extension(format!("tmp-{}-{}", std::process::id(), self.files));
            copy_object(source, &temporary, mode)?;
            fs::rename(&temporary, &object)
                .with_context(|| format!("failed to store {}", object.display()))?;
            self.new_object_bytes += bytes;
        }

        let logical_bytes = self
            .logical_bytes
            .checked_add(bytes)
            .context("toolchain size overflow")?;
        self.link_file_object(&object, relative, mode, hash, bytes, logical_bytes)
    }

    fn import_reader(
        &mut self,
        source: &mut impl Read,
        relative: &Path,
        mode: u32,
        expected_bytes: u64,
    ) -> Result<()> {
        if expected_bytes > MAX_ARCHIVE_BYTES {
            bail!(
                "toolchain archive entry is too large: {} ({expected_bytes} bytes)",
                relative.display()
            );
        }
        let logical_bytes = self
            .logical_bytes
            .checked_add(expected_bytes)
            .context("toolchain archive size overflow")?;
        if logical_bytes > MAX_ARCHIVE_BYTES {
            bail!("toolchain archive expands beyond the 64 GiB safety limit");
        }

        let temporary = self.store.objects_root().join(format!(
            ".tmp-{}-{}-{}",
            std::process::id(),
            now_nanos(),
            self.files
        ));
        let result = (|| {
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .with_context(|| format!("failed to create {}", temporary.display()))?;
            let mut hash = Sha256::new();
            let mut bytes = 0_u64;
            let mut buffer = vec![0_u8; 1024 * 1024];
            loop {
                let read = source
                    .read(&mut buffer)
                    .with_context(|| format!("failed to read {}", relative.display()))?;
                if read == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..read])
                    .with_context(|| format!("failed to write {}", temporary.display()))?;
                hash.update(&buffer[..read]);
                bytes = bytes
                    .checked_add(read as u64)
                    .context("toolchain archive entry size overflow")?;
                if bytes > expected_bytes {
                    bail!(
                        "toolchain archive entry exceeded its declared size: {}",
                        relative.display()
                    );
                }
            }
            if bytes != expected_bytes {
                bail!(
                    "toolchain archive entry has size {bytes}, expected {expected_bytes}: {}",
                    relative.display()
                );
            }
            output
                .flush()
                .with_context(|| format!("failed to flush {}", temporary.display()))?;
            drop(output);

            let hash = format!("{:x}", hash.finalize());
            let mode = mode & 0o555;
            if mode & 0o444 == 0 {
                bail!(
                    "toolchain archive file is not readable: {}",
                    relative.display()
                );
            }
            let object = self.store.object_path(mode, &hash);
            if object.exists() {
                if fs::metadata(&object)
                    .with_context(|| format!("failed to inspect {}", object.display()))?
                    .len()
                    != bytes
                {
                    bail!("toolchain object has the wrong size: {}", object.display());
                }
                fs::remove_file(&temporary)
                    .with_context(|| format!("failed to remove {}", temporary.display()))?;
                self.reused_bytes = self
                    .reused_bytes
                    .checked_add(bytes)
                    .context("reused toolchain size overflow")?;
            } else {
                if let Some(parent) = object.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                platform::set_read_only_mode(&temporary, mode)?;
                fs::rename(&temporary, &object)
                    .with_context(|| format!("failed to store {}", object.display()))?;
                self.new_object_bytes = self
                    .new_object_bytes
                    .checked_add(bytes)
                    .context("new toolchain size overflow")?;
            }

            self.link_file_object(&object, relative, mode, hash, bytes, logical_bytes)
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn link_file_object(
        &mut self,
        object: &Path,
        relative: &Path,
        mode: u32,
        hash: String,
        bytes: u64,
        logical_bytes: u64,
    ) -> Result<()> {
        let destination = self.stage.join(relative);
        fs::hard_link(object, &destination).with_context(|| {
            format!(
                "failed to hard-link {} to {}",
                object.display(),
                destination.display()
            )
        })?;
        self.entries.push(StoreEntry::File {
            path: relative.to_owned(),
            mode,
            hash,
            bytes,
        });
        self.logical_bytes = logical_bytes;
        self.files += 1;
        Ok(())
    }
}

fn import_directory(source: &Path, relative: &Path, state: &mut ImportState<'_>) -> Result<()> {
    if !relative.as_os_str().is_empty() {
        let destination = state.stage.join(relative);
        fs::create_dir(&destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        state.entries.push(StoreEntry::Directory {
            path: relative.to_owned(),
        });
    }

    let directory = source.join(relative);
    let mut entries = fs::read_dir(&directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative_path = relative.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            platform::create_symlink_from_source(
                &target,
                &state.stage.join(&relative_path),
                &path,
            )?;
            state.entries.push(StoreEntry::Symlink {
                path: relative_path,
                target,
            });
        } else if metadata.is_dir() {
            import_directory(source, &relative_path, state)?;
        } else if metadata.is_file() {
            state.import_file(&path, &relative_path, &metadata)?;
        } else {
            bail!("unsupported file type in toolchain: {}", path.display());
        }
    }
    Ok(())
}

fn copy_object(source: &Path, destination: &Path, mode: u32) -> Result<()> {
    let mut input =
        File::open(source).with_context(|| format!("failed to open {}", source.display()))?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    std::io::copy(&mut input, &mut output)
        .with_context(|| format!("failed to copy {}", source.display()))?;
    output
        .flush()
        .with_context(|| format!("failed to flush {}", destination.display()))?;
    platform::set_read_only_mode(destination, mode)?;
    Ok(())
}

fn fingerprint(entries: &[StoreEntry]) -> String {
    let mut hash = Sha256::new();
    for entry in entries {
        match entry {
            StoreEntry::Directory { path } => {
                hash.update(b"d\0");
                hash.update(path.to_string_lossy().as_bytes());
            }
            StoreEntry::File {
                path,
                mode,
                hash: object_hash,
                ..
            } => {
                hash.update(b"f\0");
                hash.update(path.to_string_lossy().as_bytes());
                hash.update(mode.to_le_bytes());
                hash.update(object_hash.as_bytes());
            }
            StoreEntry::Symlink { path, target } => {
                hash.update(b"l\0");
                hash.update(path.to_string_lossy().as_bytes());
                hash.update(target.to_string_lossy().as_bytes());
            }
        }
        hash.update(b"\0");
    }
    format!("{:x}", hash.finalize())
}

fn store_entry_path(entry: &StoreEntry) -> &Path {
    match entry {
        StoreEntry::Directory { path }
        | StoreEntry::File { path, .. }
        | StoreEntry::Symlink { path, .. } => path,
    }
}

fn store_entry_kind(entry: &StoreEntry) -> u8 {
    match entry {
        StoreEntry::Directory { .. } => 0,
        StoreEntry::File { .. } => 1,
        StoreEntry::Symlink { .. } => 2,
    }
}

fn validate_symlink_target(path: &Path, target: &Path) -> Result<()> {
    let mut depth = path
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "toolchain symlink escapes the archive root: {} -> {}",
                    path.display(),
                    target.display()
                )
            }
        }
    }
    Ok(())
}

fn validate_object_identity(mode: u32, hash: &str) -> Result<()> {
    if mode & !0o555 != 0 || mode & 0o444 == 0 || !crate::core::hex::is_sha256(hash) {
        bail!("invalid toolchain object identity");
    }
    Ok(())
}

fn validate_chunk_provenance(provenance: &ChunkProvenance) -> Result<()> {
    if provenance.remote.is_empty()
        || provenance.remote.chars().any(char::is_control)
        || provenance.manifest.is_empty()
        || provenance.manifest.chars().any(char::is_control)
        || provenance.platform.is_empty()
        || provenance.platform.chars().any(char::is_control)
        || !crate::core::hex::is_sha256(&provenance.manifest_sha256)
        || !crate::core::hex::is_sha256(&provenance.signing_key_fingerprint)
    {
        bail!("invalid signed chunk provenance");
    }
    Ok(())
}

fn is_relative_store_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn alias_for(toolchain: &str) -> String {
    let channel = toolchain
        .rsplit_once(':')
        .map_or(toolchain, |(_, value)| value);
    let channel = channel
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '.' || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("lev-{channel}-{}", &digest(toolchain.as_bytes())[..8])
}

fn count_directories(path: &Path) -> Result<u64> {
    let mut count = 0;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
            && !entry.file_name().to_string_lossy().starts_with(".tmp-")
        {
            count += 1;
        }
    }
    Ok(count)
}

fn make_tree_writable(path: &Path) -> Result<()> {
    make_directory_writable(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_tree_writable(&entry.path())?;
        }
    }
    Ok(())
}

fn seal_directories(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            seal_directories(&entry.path())?;
        }
    }
    platform::set_read_only_mode(path, 0o555)
}

fn publish_toolchain_view(stage: &Path, view: &Path) -> Result<()> {
    seal_directories(stage)?;
    // macOS refuses to rename a directory after its own write bit has been
    // removed. The children stay sealed while the staging root is renamed.
    make_directory_writable(stage)?;

    if view.exists() {
        make_tree_writable(stage)?;
        fs::remove_dir_all(stage)
            .with_context(|| format!("failed to remove {}", stage.display()))?;
        return seal_directories(view);
    }

    fs::rename(stage, view)
        .with_context(|| format!("failed to materialize toolchain view {}", view.display()))?;
    if let Err(error) = platform::set_read_only_mode(view, 0o555) {
        let _ = make_tree_writable(view);
        let _ = fs::remove_dir_all(view);
        return Err(error)
            .with_context(|| format!("failed to seal toolchain view {}", view.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn make_directory_writable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut mode = fs::metadata(path)?.permissions().mode();
    mode |= 0o700;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to make {} writable", path.display()))
}

#[cfg(not(unix))]
fn make_directory_writable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make {} writable", path.display()))
}

fn remove_empty_directories(path: &Path) -> Result<bool> {
    if !path.is_dir() {
        return Ok(false);
    }
    let entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    for entry in entries {
        if entry.file_type()?.is_dir() && remove_empty_directories(&entry.path())? {
            fs::remove_dir(entry.path())?;
        }
    }
    Ok(fs::read_dir(path)?.next().is_none())
}

struct StoreLock(File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// Shared guard that prevents toolchain-store import or GC while old objects
/// are being inspected for reusable chunks.
#[must_use = "dropping the guard releases the toolchain-store read lock"]
pub struct ToolchainStoreReadLock(File);

impl Drop for ToolchainStoreReadLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(unix)]
fn verify_mode(path: &Path, expected: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let actual = fs::metadata(path)?.permissions().mode() & 0o777;
    if actual != expected {
        bail!(
            "toolchain object has mode {actual:04o}, expected {expected:04o}: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_mode(_path: &Path, _expected: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn verify_directory_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o222 != 0 {
        bail!(
            "toolchain view directory is writable ({mode:04o}): {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_directory_mode(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn verify_hard_link(object: &Path, materialized: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let object_metadata =
        fs::metadata(object).with_context(|| format!("failed to inspect {}", object.display()))?;
    let view_metadata = fs::metadata(materialized)
        .with_context(|| format!("failed to inspect {}", materialized.display()))?;
    if object_metadata.dev() != view_metadata.dev() || object_metadata.ino() != view_metadata.ino()
    {
        bail!(
            "{} is not hard-linked to its toolchain object",
            materialized.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_hard_link(object: &Path, materialized: &Path) -> Result<()> {
    if fs::metadata(object)?.len() != fs::metadata(materialized)?.len() {
        bail!(
            "toolchain view file has the wrong size: {}",
            materialized.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::ToolchainStore;

    fn only_file(root: &Path) -> PathBuf {
        let files = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1, "expected one file in {}", root.display());
        files.into_iter().next().unwrap()
    }

    #[test]
    fn imports_and_deduplicates_toolchain_files() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::create_dir_all(source.join("lib")).unwrap();
        fs::write(source.join("bin/lean"), "same bytes").unwrap();
        fs::write(source.join("lib/copy"), "same bytes").unwrap();

        let imported = store.import("leanprover/lean4:v4.test", &source).unwrap();
        assert_eq!(imported.files, 2);
        assert_eq!(imported.logical_bytes, 20);
        assert_eq!(imported.new_object_bytes, 10);
        assert_eq!(imported.reused_bytes, 10);

        let stats = store.stats().unwrap();
        assert_eq!(stats.manifests, 1);
        assert_eq!(stats.views, 1);
        assert_eq!(stats.objects, 1);
        assert_eq!(stats.object_bytes, 10);

        let verified = store.verify().unwrap();
        assert_eq!(verified.manifests, 1);
        assert_eq!(verified.views, 1);
        assert_eq!(verified.objects, 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&imported.view).unwrap().permissions().mode() & 0o222,
                0
            );
            fs::set_permissions(&imported.view, fs::Permissions::from_mode(0o755)).unwrap();
            let error = store.verify().unwrap_err().to_string();
            assert!(error.contains("view directory is writable"), "{error}");
            fs::set_permissions(&imported.view, fs::Permissions::from_mode(0o555)).unwrap();
        }

        fs::create_dir(store.root.join("views/.tmp-interrupted")).unwrap();
        fs::write(
            store.root.join("manifests/interrupted.json.tmp-1"),
            "partial",
        )
        .unwrap();
        let report = store.gc(false).unwrap();
        assert_eq!(report.manifests, 1);
        assert_eq!(report.views, 1);
        assert_eq!(report.objects, 0);
        assert_eq!(report.object_bytes, 0);

        store.gc(true).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.manifests, 1);
        assert_eq!(stats.views, 1);
        assert_eq!(stats.objects, 1);
    }

    #[test]
    fn gc_preserves_objects_shared_between_installed_versions() {
        let temp = tempdir().unwrap();
        let first_source = temp.path().join("first");
        let second_source = temp.path().join("second");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        for source in [&first_source, &second_source] {
            fs::create_dir_all(source.join("bin")).unwrap();
            fs::create_dir_all(source.join("lib")).unwrap();
            fs::write(source.join("bin/lean"), "shared lean").unwrap();
            fs::write(source.join("lib/shared"), "shared library").unwrap();
        }
        fs::write(first_source.join("lib/version"), "first").unwrap();
        fs::write(second_source.join("lib/version"), "second").unwrap();

        store
            .import("leanprover/lean4:v4.first", &first_source)
            .unwrap();
        store
            .import("leanprover/lean4:v4.second", &second_source)
            .unwrap();

        let report = store.gc(false).unwrap();
        assert_eq!(report.manifests, 0);
        assert_eq!(report.views, 0);
        assert_eq!(report.objects, 0);

        store.gc(true).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.manifests, 2);
        assert_eq!(stats.views, 2);
        assert_eq!(stats.objects, 4);
        store.verify().unwrap();

        assert_eq!(store.remove("leanprover/lean4:v4.second").unwrap(), 1);
        let stats = store.stats().unwrap();
        assert_eq!(stats.manifests, 1);
        assert_eq!(stats.views, 1);
        assert_eq!(stats.objects, 3);
        store.verify().unwrap();
    }

    #[test]
    fn selectors_find_sources_and_aliases_and_remove_idempotently() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/lean"), "lean").unwrap();
        let imported = store.import("leanprover/lean4:v4.test", &source).unwrap();

        assert_eq!(
            store
                .find("leanprover/lean4:v4.test")
                .unwrap()
                .unwrap()
                .view,
            imported.view
        );
        assert_eq!(
            store.find(&imported.alias).unwrap().unwrap().view,
            imported.view
        );
        assert_eq!(store.installed().unwrap().len(), 1);
        assert_eq!(store.remove(&imported.alias).unwrap(), 1);
        assert_eq!(store.remove(&imported.alias).unwrap(), 0);
        assert!(store.installed().unwrap().is_empty());
    }

    #[test]
    fn runtime_lookup_reads_only_the_validated_installation_record() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/lean"), "lean").unwrap();
        let imported = store.import("leanprover/lean4:v4.test", &source).unwrap();

        let manifest = only_file(&store.root.join("manifests"));
        let entries = only_file(&store.root.join("manifest-entries"));
        let original_entries = fs::read(&entries).unwrap();
        fs::write(&entries, "corrupt companion").unwrap();

        assert_eq!(
            store
                .find("leanprover/lean4:v4.test")
                .unwrap()
                .unwrap()
                .view,
            imported.view
        );
        let error = store.verify().unwrap_err().to_string();
        assert!(error.contains("entry-list digest mismatch"), "{error}");

        fs::write(&entries, original_entries).unwrap();
        store.verify().unwrap();

        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        record["alias"] = serde_json::Value::String("untrusted-alias".to_owned());
        fs::write(&manifest, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        let error = store
            .find("leanprover/lean4:v4.test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid toolchain alias"), "{error}");
    }

    #[test]
    fn legacy_inline_manifests_remain_readable_and_verifiable() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/lean"), "legacy lean").unwrap();
        let imported = store.import("leanprover/lean4:v4.legacy", &source).unwrap();

        let manifest = only_file(&store.root.join("manifests"));
        let entries_path = only_file(&store.root.join("manifest-entries"));
        let entries: serde_json::Value =
            serde_json::from_slice(&fs::read(&entries_path).unwrap()).unwrap();
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        let object = record.as_object_mut().unwrap();
        object.insert("version".to_owned(), serde_json::Value::from(1));
        object.remove("entries_sha256");
        object.insert("entries".to_owned(), entries);
        fs::write(&manifest, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        fs::remove_file(entries_path).unwrap();

        assert_eq!(
            store
                .find("leanprover/lean4:v4.legacy")
                .unwrap()
                .unwrap()
                .view,
            imported.view
        );
        assert_eq!(store.verify().unwrap().manifests, 1);
        assert_eq!(store.gc(false).unwrap().manifests, 0);
    }
}
