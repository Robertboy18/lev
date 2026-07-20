//! Bounded tar ingestion for direct toolchain installs.
//!
//! Extraction stays in staging under the store lock until validation and
//! publication finish.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::core::{hex, platform};

use super::manifest::MANIFEST_VERSION;
use super::{
    ArchiveProvenance, ImportResult, ImportState, MAX_ARCHIVE_BYTES, MAX_ARCHIVE_ENTRIES,
    StoreEntry, StoreLock, ToolchainManifest, ToolchainStore, alias_for, fingerprint,
    make_tree_writable, now_seconds, publish_toolchain_view, store_entry_kind, store_entry_path,
    validate_symlink_target,
};

/// A fully extracted but not yet visible toolchain transaction.
///
/// Publication records verified archive provenance and atomically makes the
/// immutable view discoverable. Dropping this value removes its private stage.
pub struct PendingToolchainImport {
    store: ToolchainStore,
    source_toolchain: String,
    stage: Option<PathBuf>,
    entries: Vec<StoreEntry>,
    logical_bytes: u64,
    new_object_bytes: u64,
    reused_bytes: u64,
    files: u64,
    _lock: StoreLock,
}

#[derive(Debug, Clone)]
enum ImportedArchiveEntry {
    Directory,
    File { mode: u32, hash: String, bytes: u64 },
    Symlink,
}

struct ArchiveImportState<'a, 'b> {
    import: &'a mut ImportState<'b>,
    root: Option<OsString>,
    entries: HashMap<PathBuf, ImportedArchiveEntry>,
    archive_entries: u64,
}

impl ToolchainStore {
    /// Extract a single-root tar stream into a private content-store view.
    pub fn prepare_tar<R: Read>(
        &self,
        toolchain: &str,
        reader: R,
    ) -> Result<super::PendingToolchainImport> {
        self.ensure()?;
        let lock = self.import_lock()?;
        let stage = self.create_import_stage()?;

        let result = (|| {
            let mut state = ImportState::new(self, &stage);
            import_tar(reader, &mut state)?;
            if !stage.join("bin/lean").is_file() && !stage.join("bin/lean.exe").is_file() {
                bail!("archive is not a Lean toolchain: bin/lean is missing");
            }
            state.entries.sort_by(|left, right| {
                store_entry_path(left)
                    .cmp(store_entry_path(right))
                    .then_with(|| store_entry_kind(left).cmp(&store_entry_kind(right)))
            });

            Ok(PendingToolchainImport {
                store: self.clone(),
                source_toolchain: toolchain.to_owned(),
                stage: Some(stage.clone()),
                entries: state.entries,
                logical_bytes: state.logical_bytes,
                new_object_bytes: state.new_object_bytes,
                reused_bytes: state.reused_bytes,
                files: state.files,
                _lock: lock,
            })
        })();

        if result.is_err() && stage.exists() {
            let _ = make_tree_writable(&stage);
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }
}

impl PendingToolchainImport {
    /// Seal and publish the prepared tree after archive digest verification.
    pub fn publish(mut self, archive: ArchiveProvenance) -> Result<ImportResult> {
        if archive.name.is_empty() || archive.url.is_empty() || !hex::is_sha256(&archive.sha256) {
            bail!("invalid archive provenance");
        }

        let stage = self
            .stage
            .as_ref()
            .context("toolchain import has already been published")?;
        let fingerprint = fingerprint(&self.entries);
        let view = self.store.views_root().join(&fingerprint);
        publish_toolchain_view(stage, &view)?;
        self.stage = None;

        let alias = alias_for(&self.source_toolchain);
        let manifest = ToolchainManifest {
            version: MANIFEST_VERSION,
            source_toolchain: self.source_toolchain.clone(),
            alias: alias.clone(),
            fingerprint,
            view: view.clone(),
            logical_bytes: self.logical_bytes,
            created_at: now_seconds(),
            archive: Some(archive),
            chunks: None,
            entries: std::mem::take(&mut self.entries),
        };
        self.store.write_manifest(&manifest)?;

        Ok(ImportResult {
            alias,
            view,
            files: self.files,
            logical_bytes: self.logical_bytes,
            new_object_bytes: self.new_object_bytes,
            reused_bytes: self.reused_bytes,
        })
    }
}

impl Drop for PendingToolchainImport {
    fn drop(&mut self) {
        let Some(stage) = self.stage.take() else {
            return;
        };
        if stage.exists() {
            let _ = make_tree_writable(&stage);
            let _ = fs::remove_dir_all(stage);
        }
    }
}

fn import_tar<R: Read>(reader: R, import: &mut ImportState<'_>) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut state = ArchiveImportState {
        import,
        root: None,
        entries: HashMap::new(),
        archive_entries: 0,
    };

    for entry in archive
        .entries()
        .context("failed to read toolchain tar archive")?
    {
        state.archive_entries += 1;
        if state.archive_entries > MAX_ARCHIVE_ENTRIES {
            bail!("toolchain archive contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let mut entry = entry.context("failed to read toolchain tar entry")?;
        let archive_path = entry
            .path()
            .context("toolchain archive contains an invalid path")?
            .into_owned();
        let relative = state.relative_path(&archive_path)?;
        if relative.as_os_str().is_empty() {
            if !entry.header().entry_type().is_dir() {
                bail!(
                    "toolchain archive root is not a directory: {}",
                    archive_path.display()
                );
            }
            continue;
        }
        if relative.components().count() > 256 {
            bail!("toolchain archive path is too deep: {}", relative.display());
        }
        if relative.as_os_str().len() > 32 * 1024 {
            bail!("toolchain archive path is too long");
        }

        let kind = entry.header().entry_type();
        if kind.is_dir() {
            state.ensure_directory(&relative)?;
        } else if kind.is_file() {
            state.ensure_parent(&relative)?;
            state.ensure_vacant(&relative)?;
            let mode = entry
                .header()
                .mode()
                .with_context(|| format!("invalid mode for {}", archive_path.display()))?;
            let bytes = entry
                .header()
                .size()
                .with_context(|| format!("invalid size for {}", archive_path.display()))?;
            state
                .import
                .import_reader(&mut entry, &relative, mode, bytes)?;
            let StoreEntry::File {
                mode, hash, bytes, ..
            } = state
                .import
                .entries
                .last()
                .context("internal error: imported file was not recorded")?
            else {
                bail!("internal error: imported file has the wrong type");
            };
            state.entries.insert(
                relative,
                ImportedArchiveEntry::File {
                    mode: *mode,
                    hash: hash.clone(),
                    bytes: *bytes,
                },
            );
        } else if kind.is_symlink() {
            state.ensure_parent(&relative)?;
            state.ensure_vacant(&relative)?;
            let target = entry
                .link_name()
                .context("failed to read toolchain symlink target")?
                .with_context(|| {
                    format!(
                        "toolchain symlink has no target: {}",
                        archive_path.display()
                    )
                })?
                .into_owned();
            validate_symlink_target(&relative, &target)?;
            platform::create_symlink_from_source(
                &target,
                &state.import.stage.join(&relative),
                Path::new("archive"),
            )?;
            state.import.entries.push(StoreEntry::Symlink {
                path: relative.clone(),
                target,
            });
            state
                .entries
                .insert(relative, ImportedArchiveEntry::Symlink);
        } else if kind.is_hard_link() {
            state.ensure_parent(&relative)?;
            state.ensure_vacant(&relative)?;
            let target = entry
                .link_name()
                .context("failed to read toolchain hard-link target")?
                .with_context(|| {
                    format!(
                        "toolchain hard link has no target: {}",
                        archive_path.display()
                    )
                })?
                .into_owned();
            let target = state.relative_path(&target)?;
            state.add_hard_link(&relative, &target)?;
        } else {
            bail!(
                "unsupported entry type in toolchain archive: {}",
                archive_path.display()
            );
        }
    }

    Ok(())
}

impl ArchiveImportState<'_, '_> {
    /// Strip and consistently enforce the archive's one top-level directory.
    fn relative_path(&mut self, path: &Path) -> Result<PathBuf> {
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => components.push(value.to_os_string()),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("unsafe path in toolchain archive: {}", path.display())
                }
            }
        }
        let (root, remainder) = components
            .split_first()
            .with_context(|| format!("empty path in toolchain archive: {}", path.display()))?;
        if let Some(expected) = &self.root {
            if expected != root {
                bail!(
                    "toolchain archive has multiple roots: {:?} and {:?}",
                    expected,
                    root
                );
            }
        } else {
            self.root = Some(root.clone());
        }
        Ok(remainder.iter().collect())
    }

    fn ensure_parent(&mut self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                self.ensure_directory(parent)?;
            }
        }
        Ok(())
    }

    /// Materialize missing parents while rejecting file/directory collisions.
    fn ensure_directory(&mut self, path: &Path) -> Result<()> {
        let mut current = PathBuf::new();
        for component in path.components() {
            let Component::Normal(component) = component else {
                bail!("unsafe directory in toolchain archive: {}", path.display());
            };
            current.push(component);
            match self.entries.get(&current) {
                Some(ImportedArchiveEntry::Directory) => continue,
                Some(_) => {
                    bail!(
                        "toolchain archive path is both a directory and another entry: {}",
                        current.display()
                    )
                }
                None => {}
            }
            let destination = self.import.stage.join(&current);
            fs::create_dir(&destination)
                .with_context(|| format!("failed to create {}", destination.display()))?;
            self.import.entries.push(StoreEntry::Directory {
                path: current.clone(),
            });
            self.entries
                .insert(current.clone(), ImportedArchiveEntry::Directory);
        }
        Ok(())
    }

    fn ensure_vacant(&self, path: &Path) -> Result<()> {
        if self.entries.contains_key(path) {
            bail!("duplicate path in toolchain archive: {}", path.display());
        }
        Ok(())
    }

    /// Resolve only already-seen regular-file hard links.
    ///
    /// Requiring the target to precede the link avoids deferred path
    /// resolution and ensures the link comes from a validated store object.
    fn add_hard_link(&mut self, path: &Path, target: &Path) -> Result<()> {
        let ImportedArchiveEntry::File { mode, hash, bytes } = self
            .entries
            .get(target)
            .with_context(|| {
                format!(
                    "toolchain hard link references an entry that has not been imported: {}",
                    target.display()
                )
            })?
            .clone()
        else {
            bail!(
                "toolchain hard link target is not a file: {}",
                target.display()
            );
        };
        let logical_bytes = self
            .import
            .logical_bytes
            .checked_add(bytes)
            .context("toolchain archive size overflow")?;
        if logical_bytes > MAX_ARCHIVE_BYTES {
            bail!("toolchain archive expands beyond the 64 GiB safety limit");
        }
        let object = self.import.store.object_path(mode, &hash);
        let destination = self.import.stage.join(path);
        fs::hard_link(&object, &destination).with_context(|| {
            format!(
                "failed to hard-link {} to {}",
                object.display(),
                destination.display()
            )
        })?;
        self.import.entries.push(StoreEntry::File {
            path: path.to_owned(),
            mode,
            hash: hash.clone(),
            bytes,
        });
        self.import.logical_bytes = logical_bytes;
        self.import.reused_bytes = self
            .import
            .reused_bytes
            .checked_add(bytes)
            .context("reused toolchain size overflow")?;
        self.import.files += 1;
        self.entries.insert(
            path.to_owned(),
            ImportedArchiveEntry::File { mode, hash, bytes },
        );
        Ok(())
    }
}
