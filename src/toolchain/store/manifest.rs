//! On-disk records for the toolchain store.
//!
//! Runtime metadata stays small. The full file list lives in a
//! SHA-256-bound companion file used by verification, GC, and chunk reuse.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cache::digest;
use crate::core::atomic_file::replace as write_atomic;
use crate::core::hex;

use super::{
    ArchiveProvenance, ChunkProvenance, StoreEntry, ToolchainManifest, ToolchainStore, alias_for,
    fingerprint, is_relative_store_path, validate_chunk_provenance,
};

const LEGACY_MANIFEST_VERSION: u32 = 1;
pub(super) const MANIFEST_VERSION: u32 = 2;

/// Metadata sufficient to select a toolchain without loading its entry list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ToolchainInstallation {
    pub(super) version: u32,
    pub(super) source_toolchain: String,
    pub(super) alias: String,
    pub(super) fingerprint: String,
    pub(super) view: PathBuf,
    pub(super) logical_bytes: u64,
    pub(super) created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) archive: Option<ArchiveProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) chunks: Option<ChunkProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries_sha256: Option<String>,
}

/// Deserialization shape shared by legacy inline and split manifests.
#[derive(Debug, Serialize, Deserialize)]
struct ManifestDocument {
    #[serde(flatten)]
    installation: ToolchainInstallation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<StoreEntry>>,
}

impl ToolchainStore {
    /// Atomically publish companion entries before their small runtime record.
    pub(super) fn write_manifest(&self, manifest: &ToolchainManifest) -> Result<()> {
        let key = manifest_key(&manifest.source_toolchain, &manifest.fingerprint);
        let entries_path = self.manifest_entries_root().join(format!("{key}.json"));
        let entries = serde_json::to_vec(&manifest.entries)?;
        write_atomic(&entries_path, &entries)?;

        let document = ManifestDocument {
            installation: ToolchainInstallation {
                version: MANIFEST_VERSION,
                source_toolchain: manifest.source_toolchain.clone(),
                alias: manifest.alias.clone(),
                fingerprint: manifest.fingerprint.clone(),
                view: manifest.view.clone(),
                logical_bytes: manifest.logical_bytes,
                created_at: manifest.created_at,
                archive: manifest.archive.clone(),
                chunks: manifest.chunks.clone(),
                entries_sha256: Some(digest(&entries)),
            },
            entries: None,
        };
        let path = self.manifests_root().join(format!("{key}.json"));
        write_atomic(&path, &serde_json::to_vec_pretty(&document)?)
    }

    /// Read only launch-time records.
    ///
    /// Version-2 records never load their companion arrays. Legacy records are
    /// fully validated because their old inline layout has no independently
    /// authenticated launch-time header.
    pub(super) fn read_installations(&self) -> Result<Vec<ToolchainInstallation>> {
        Ok(self
            .read_installation_records()?
            .into_iter()
            .map(|(_, installation)| installation)
            .collect())
    }

    /// Preserve record paths for removal while avoiding full entry parsing.
    pub(super) fn read_installation_records(
        &self,
    ) -> Result<Vec<(PathBuf, ToolchainInstallation)>> {
        let mut installations = Vec::new();
        for path in json_files(&self.manifests_root())? {
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let installation: ToolchainInstallation = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            self.validate_installation(&path, &installation)?;
            if installation.version == LEGACY_MANIFEST_VERSION {
                self.parse_manifest(&path, &bytes)?;
            }
            installations.push((path, installation));
        }
        Ok(installations)
    }

    pub(super) fn read_manifests(&self) -> Result<Vec<ToolchainManifest>> {
        Ok(self
            .read_manifest_records()?
            .into_iter()
            .map(|(_, manifest)| manifest)
            .collect())
    }

    /// Load and fully validate entry lists for integrity and lifecycle work.
    pub(super) fn read_manifest_records(&self) -> Result<Vec<(PathBuf, ToolchainManifest)>> {
        let mut manifests = Vec::new();
        for path in json_files(&self.manifests_root())? {
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let manifest = self.parse_manifest(&path, &bytes)?;
            manifests.push((path, manifest));
        }
        Ok(manifests)
    }

    fn parse_manifest(&self, path: &Path, bytes: &[u8]) -> Result<ToolchainManifest> {
        let document: ManifestDocument = serde_json::from_slice(bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        self.validate_installation(path, &document.installation)?;

        let entries = match document.installation.version {
            LEGACY_MANIFEST_VERSION => document.entries.with_context(|| {
                format!(
                    "legacy toolchain manifest has no entries: {}",
                    path.display()
                )
            })?,
            MANIFEST_VERSION => {
                if document.entries.is_some() {
                    bail!(
                        "split toolchain manifest contains inline entries: {}",
                        path.display()
                    );
                }
                let entries_path = self.manifest_entries_path(path)?;
                let entries_bytes = fs::read(&entries_path)
                    .with_context(|| format!("failed to read {}", entries_path.display()))?;
                let expected = document
                    .installation
                    .entries_sha256
                    .as_deref()
                    .context("split toolchain manifest has no entry-list digest")?;
                if digest(&entries_bytes) != expected {
                    bail!(
                        "toolchain manifest entry-list digest mismatch in {}",
                        entries_path.display()
                    );
                }
                serde_json::from_slice(&entries_bytes)
                    .with_context(|| format!("failed to parse {}", entries_path.display()))?
            }
            version => {
                bail!(
                    "unsupported toolchain manifest version {version} in {}",
                    path.display()
                )
            }
        };

        let installation = document.installation;
        let manifest = ToolchainManifest {
            version: installation.version,
            source_toolchain: installation.source_toolchain,
            alias: installation.alias,
            fingerprint: installation.fingerprint,
            view: installation.view,
            logical_bytes: installation.logical_bytes,
            created_at: installation.created_at,
            archive: installation.archive,
            chunks: installation.chunks,
            entries,
        };
        self.validate_manifest(path, &manifest)?;
        Ok(manifest)
    }

    /// Validate all fields trusted by launch-time selection.
    fn validate_installation(
        &self,
        path: &Path,
        installation: &ToolchainInstallation,
    ) -> Result<()> {
        if !matches!(
            installation.version,
            LEGACY_MANIFEST_VERSION | MANIFEST_VERSION
        ) {
            bail!(
                "unsupported toolchain manifest version {} in {}",
                installation.version,
                path.display()
            );
        }
        let expected_name = format!(
            "{}.json",
            manifest_key(&installation.source_toolchain, &installation.fingerprint)
        );
        if path.file_name() != Some(OsStr::new(&expected_name)) {
            bail!(
                "toolchain manifest has an invalid filename: {}",
                path.display()
            );
        }
        if installation.alias != alias_for(&installation.source_toolchain) {
            bail!("invalid toolchain alias in {}", path.display());
        }
        if !hex::is_sha256(&installation.fingerprint) {
            bail!("invalid toolchain fingerprint in {}", path.display());
        }
        if installation.view != self.views_root().join(&installation.fingerprint) {
            bail!("invalid toolchain view path in {}", path.display());
        }
        if let Some(archive) = &installation.archive {
            if archive.name.is_empty() || archive.url.is_empty() || !hex::is_sha256(&archive.sha256)
            {
                bail!("invalid archive provenance in {}", path.display());
            }
        }
        if installation.archive.is_some() && installation.chunks.is_some() {
            bail!(
                "toolchain manifest has multiple provenances in {}",
                path.display()
            );
        }
        if let Some(chunks) = &installation.chunks {
            validate_chunk_provenance(chunks)
                .with_context(|| format!("invalid chunk provenance in {}", path.display()))?;
        }
        match installation.version {
            LEGACY_MANIFEST_VERSION if installation.entries_sha256.is_some() => {
                bail!(
                    "legacy toolchain manifest has a split-entry digest in {}",
                    path.display()
                );
            }
            MANIFEST_VERSION
                if !installation
                    .entries_sha256
                    .as_deref()
                    .is_some_and(hex::is_sha256) =>
            {
                bail!(
                    "split toolchain manifest has an invalid entry-list digest in {}",
                    path.display()
                );
            }
            _ => {}
        }
        Ok(())
    }

    /// Validate the complete entry closure after loading its companion file.
    fn validate_manifest(&self, path: &Path, manifest: &ToolchainManifest) -> Result<()> {
        if manifest.fingerprint != fingerprint(&manifest.entries) {
            bail!("invalid toolchain fingerprint in {}", path.display());
        }

        let mut paths = HashSet::new();
        let mut logical_bytes = 0_u64;
        for entry in &manifest.entries {
            let entry_path = match entry {
                StoreEntry::Directory { path } => path,
                StoreEntry::File {
                    path,
                    mode,
                    hash,
                    bytes,
                } => {
                    if mode & !0o555 != 0 || !hex::is_sha256(hash) {
                        bail!("invalid toolchain object in {}", path.display());
                    }
                    logical_bytes = logical_bytes
                        .checked_add(*bytes)
                        .with_context(|| format!("size overflow in {}", path.display()))?;
                    path
                }
                StoreEntry::Symlink { path, .. } => path,
            };
            if !is_relative_store_path(entry_path) || !paths.insert(entry_path.clone()) {
                bail!("invalid or duplicate entry path in {}", path.display());
            }
        }
        if logical_bytes != manifest.logical_bytes {
            bail!("invalid logical size in {}", path.display());
        }
        Ok(())
    }

    pub(super) fn temporary_manifest_paths(&self) -> Result<Vec<PathBuf>> {
        let root = self.manifests_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in
            fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
            if entry.file_type()?.is_file()
                && entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                paths.push(entry.path());
            }
        }
        paths.sort();
        Ok(paths)
    }

    /// Return companion files not referenced by a live version-2 record.
    pub(super) fn stale_manifest_entry_paths(
        &self,
        live: &HashSet<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        let root = self.manifest_entries_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut stale = Vec::new();
        for entry in
            fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
            if entry.file_type()?.is_file() && !live.contains(&entry.path()) {
                stale.push(entry.path());
            }
        }
        stale.sort();
        Ok(stale)
    }

    pub(super) fn manifest_entries_root(&self) -> PathBuf {
        self.root.join("manifest-entries")
    }

    pub(super) fn manifest_entries_path(&self, manifest_path: &Path) -> Result<PathBuf> {
        let name = manifest_path
            .file_name()
            .context("toolchain manifest path has no filename")?;
        Ok(self.manifest_entries_root().join(name))
    }
}

fn manifest_key(source_toolchain: &str, fingerprint: &str) -> String {
    digest(format!("{source_toolchain}\0{fingerprint}").as_bytes())
}

fn json_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}
