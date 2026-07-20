//! Signed transport for Lake's trace-keyed artifact cache.
//!
//! Lake still owns trace keys and output validity. lev adds:
//!
//! 1. immutable, SHA-256-addressed compressed blobs;
//! 2. an Ed25519-signed manifest binding blobs to exact Lake destinations;
//! 3. transactional import that publishes artifacts before mappings.
//!
//! The layout works in a directory or behind an HTTPS object server:
//!
//! ```text
//! remote-v1/
//! |-- blobs/sha256/ab/abcdef....zst
//! `-- manifests/<namespace>/<toolchain-sha256>/<platform>/<revision>.json
//!     manifests/<namespace>/<toolchain-sha256>/<platform>/<revision>.sig
//! ```
//!
//! Blob names use the uncompressed SHA-256. Pulls verify everything in staging
//! before taking the cache lock, and never replace conflicting files.
//!
//! Remote content is untrusted. Every path and digest is validated before
//! publication beneath the local cache root.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::lake_artifacts::{self as lake_artifact_cache, ExportEntryKind};
use crate::cache::{CacheLayout, digest};
use crate::core::atomic_file::{create_new_file, create_real_directory, write_new_file};
use crate::core::bounded_io;
use crate::core::file_hash;
use crate::core::object_transport::{ObjectTransport, split_relative_path};
use crate::core::signing::{ManifestSigner, ManifestVerifier};

const MANIFEST_SCHEMA: &str = "lev.remote-cache/v1";
const MANIFEST_KIND: &str = "lake-artifact-cache";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
const MAX_MAPPING_BYTES: u64 = 64 * 1024 * 1024;
const MAX_COMPRESSED_BLOB_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_UNCOMPRESSED_BLOB_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 1_000_000;
const ZSTD_LEVEL: i32 = 3;

/// Logical identity of one immutable remote-cache snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCacheIdentity {
    /// User-controlled hierarchy, commonly `organization/project`.
    pub namespace: String,
    /// Immutable revision, commit, lock digest, or explicitly managed channel.
    pub revision: String,
    /// Canonical elan toolchain name bound into the signed manifest.
    pub toolchain: String,
    /// Host platform because native Lean outputs are not generally portable.
    pub platform: String,
}

impl RemoteCacheIdentity {
    /// Validate all identity fields before using them in object names.
    pub fn new(
        namespace: impl Into<String>,
        revision: impl Into<String>,
        toolchain: impl Into<String>,
        platform: impl Into<String>,
    ) -> Result<Self> {
        let identity = Self {
            namespace: namespace.into(),
            revision: revision.into(),
            toolchain: toolchain.into(),
            platform: platform.into(),
        };
        validate_namespace(&identity.namespace)?;
        validate_component("revision", &identity.revision, 256)?;
        validate_component("platform", &identity.platform, 128)?;
        if identity.toolchain.is_empty()
            || identity.toolchain.len() > 512
            || identity.toolchain.chars().any(char::is_control)
        {
            bail!("invalid remote-cache toolchain identity");
        }
        Ok(identity)
    }
}

/// Machine-readable result of a successful remote push.
#[derive(Debug, Serialize)]
pub struct PushReport {
    pub namespace: String,
    pub revision: String,
    pub toolchain: String,
    pub platform: String,
    pub manifest: String,
    pub signing_key_fingerprint: String,
    pub entries: u64,
    pub artifacts: u64,
    pub mappings: u64,
    pub logical_bytes: u64,
    pub unique_blobs: u64,
    pub blobs_uploaded: u64,
    pub blobs_reused: u64,
    pub compressed_bytes_uploaded: u64,
}

/// Machine-readable result of a successful remote pull.
#[derive(Debug, Serialize)]
pub struct PullReport {
    pub namespace: String,
    pub revision: String,
    pub toolchain: String,
    pub platform: String,
    pub manifest: String,
    pub trusted_key_fingerprint: String,
    pub entries: u64,
    pub unique_blobs: u64,
    pub compressed_bytes_downloaded: u64,
    pub files_created: u64,
    pub files_reused: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum ManifestEntryKind {
    Artifact,
    Mapping,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    kind: ManifestEntryKind,
    path: String,
    sha256: String,
    bytes: u64,
    compressed_sha256: String,
    compressed_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RemoteManifest {
    schema: String,
    kind: String,
    namespace: String,
    revision: String,
    toolchain: String,
    platform: String,
    entries: Vec<ManifestEntry>,
}

struct PreparedEntry {
    kind: ManifestEntryKind,
    relative_path: String,
    blob: PreparedBlob,
    mapping_references: Option<BTreeSet<String>>,
}

#[derive(Clone)]
struct PreparedBlob {
    sha256: String,
    bytes: u64,
    compressed_sha256: String,
    compressed_bytes: u64,
    compressed_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedBlob {
    bytes: u64,
    compressed_sha256: String,
    compressed_bytes: u64,
}

/// Push a complete, self-contained snapshot of one toolchain's Lake mappings.
///
/// The shared cache lock embedded in Lake's export snapshot remains alive
/// through all source reads. Blobs are published before the signed manifest,
/// and the detached signature is the final object made visible.
pub fn push(
    cache: &CacheLayout,
    remote: &str,
    identity: &RemoteCacheIdentity,
    signing_key: &Path,
    allow_insecure_http: bool,
) -> Result<PushReport> {
    validate_identity(identity)?;
    let transport = ObjectTransport::parse(remote, allow_insecure_http)?;
    let signer = ManifestSigner::load(signing_key)?;
    let export = lake_artifact_cache::export(cache, &identity.toolchain)?;
    let staging = StagingDirectory::create(cache)?;
    let compressed_root = staging.path.join("compressed");
    fs::create_dir_all(&compressed_root)
        .with_context(|| format!("failed to create {}", compressed_root.display()))?;

    let mut prepared = Vec::with_capacity(export.entries.len());
    for (index, entry) in export.entries.iter().enumerate() {
        let kind = match entry.kind {
            ExportEntryKind::Artifact => ManifestEntryKind::Artifact,
            ExportEntryKind::Mapping => ManifestEntryKind::Mapping,
        };
        let relative_path = manifest_path_from_local(&entry.relative_path)?;
        validate_entry_path(kind, &relative_path)?;
        let destination = compressed_root.join(format!("{index:016x}.zst"));
        let (blob, mapping_references) = match kind {
            ManifestEntryKind::Artifact => (compress_file(&entry.source_path, &destination)?, None),
            ManifestEntryKind::Mapping => {
                let bytes = bounded_io::read_file(&entry.source_path, MAX_MAPPING_BYTES)?;
                let references = lake_artifact_cache::mapping_artifacts(&bytes)?;
                (
                    compress_reader(Cursor::new(&bytes), &destination)?,
                    Some(references),
                )
            }
        };
        prepared.push(PreparedEntry {
            kind,
            relative_path,
            blob,
            mapping_references,
        });
    }
    validate_prepared_references(&prepared)?;

    // Resolve each uncompressed digest to the representation already stored
    // remotely, or atomically publish a newly compressed representation.
    // Existing representations may have been produced by another zstd
    // version, so their compressed hash is measured rather than assumed.
    let existing_root = staging.path.join("existing");
    fs::create_dir_all(&existing_root)
        .with_context(|| format!("failed to create {}", existing_root.display()))?;
    let mut resolved = BTreeMap::<String, PreparedBlob>::new();
    let mut uploaded = 0_u64;
    let mut reused = 0_u64;
    let mut uploaded_bytes = 0_u64;
    for entry in &prepared {
        if let Some(previous) = resolved.get(&entry.blob.sha256) {
            if previous.bytes != entry.blob.bytes {
                bail!(
                    "internal digest collision for SHA-256 {}",
                    entry.blob.sha256
                );
            }
            continue;
        }

        let object = blob_object_path(&entry.blob.sha256)?;
        let existing = existing_root.join(format!("{}.zst", entry.blob.sha256));
        let resolved_blob = if transport.fetch(&object, &existing, MAX_COMPRESSED_BLOB_BYTES)? {
            reused = reused
                .checked_add(1)
                .context("remote blob count overflow")?;
            inspect_existing_blob(&existing, &entry.blob.sha256, entry.blob.bytes)?
        } else if transport.publish_if_absent(&object, &entry.blob.compressed_path)? {
            uploaded = uploaded
                .checked_add(1)
                .context("remote blob count overflow")?;
            uploaded_bytes = uploaded_bytes
                .checked_add(entry.blob.compressed_bytes)
                .context("remote upload size overflow")?;
            entry.blob.clone()
        } else {
            // Another writer won the create-only race. Its representation is
            // valid if and only if it expands to the same signed content.
            if !transport.fetch(&object, &existing, MAX_COMPRESSED_BLOB_BYTES)? {
                bail!("remote blob {object} disappeared during concurrent publication");
            }
            reused = reused
                .checked_add(1)
                .context("remote blob count overflow")?;
            inspect_existing_blob(&existing, &entry.blob.sha256, entry.blob.bytes)?
        };
        resolved.insert(entry.blob.sha256.clone(), resolved_blob);
    }

    let mut entries = Vec::with_capacity(prepared.len());
    let mut artifacts = 0_u64;
    let mut mappings = 0_u64;
    let mut logical_bytes = 0_u64;
    for entry in prepared {
        let blob = resolved
            .get(&entry.blob.sha256)
            .context("prepared blob was not resolved")?;
        match entry.kind {
            ManifestEntryKind::Artifact => artifacts += 1,
            ManifestEntryKind::Mapping => mappings += 1,
        }
        logical_bytes = logical_bytes
            .checked_add(blob.bytes)
            .context("remote manifest logical size overflow")?;
        entries.push(ManifestEntry {
            kind: entry.kind,
            path: entry.relative_path,
            sha256: blob.sha256.clone(),
            bytes: blob.bytes,
            compressed_sha256: blob.compressed_sha256.clone(),
            compressed_bytes: blob.compressed_bytes,
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    reject_duplicate_paths(&entries)?;

    let manifest = RemoteManifest {
        schema: MANIFEST_SCHEMA.to_owned(),
        kind: MANIFEST_KIND.to_owned(),
        namespace: identity.namespace.clone(),
        revision: identity.revision.clone(),
        toolchain: identity.toolchain.clone(),
        platform: identity.platform.clone(),
        entries,
    };
    let manifest_bytes = canonical_manifest_bytes(&manifest)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("remote-cache manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit");
    }
    let signature = signer.sign(&manifest_bytes);
    let object = manifest_object_path(identity);
    let manifest_file = staging.path.join("manifest.json");
    let signature_file = staging.path.join("manifest.sig");
    write_new_file(&manifest_file, &manifest_bytes)?;
    write_new_file(&signature_file, signature.as_bytes())?;

    transport.publish_immutable(&object, &manifest_file, MAX_MANIFEST_BYTES)?;
    transport.publish_immutable(
        &signature_object_path(identity),
        &signature_file,
        MAX_SIGNATURE_BYTES,
    )?;

    Ok(PushReport {
        namespace: identity.namespace.clone(),
        revision: identity.revision.clone(),
        toolchain: identity.toolchain.clone(),
        platform: identity.platform.clone(),
        manifest: object,
        signing_key_fingerprint: signer.fingerprint(),
        entries: manifest.entries.len() as u64,
        artifacts,
        mappings,
        logical_bytes,
        unique_blobs: resolved.len() as u64,
        blobs_uploaded: uploaded,
        blobs_reused: reused,
        compressed_bytes_uploaded: uploaded_bytes,
    })
}

/// Pull, authenticate, fully verify, and transactionally publish a snapshot.
pub fn pull(
    cache: &CacheLayout,
    remote: &str,
    identity: &RemoteCacheIdentity,
    public_key: &Path,
    allow_insecure_http: bool,
) -> Result<PullReport> {
    validate_identity(identity)?;
    let transport = ObjectTransport::parse(remote, allow_insecure_http)?;
    let verifier = ManifestVerifier::load(public_key)?;
    let staging = StagingDirectory::create(cache)?;
    let manifest_object = manifest_object_path(identity);
    let manifest_path = staging.path.join("manifest.json");
    let signature_path = staging.path.join("manifest.sig");
    if !transport.fetch(&manifest_object, &manifest_path, MAX_MANIFEST_BYTES)? {
        bail!("remote-cache manifest {manifest_object} does not exist");
    }
    if !transport.fetch(
        &signature_object_path(identity),
        &signature_path,
        MAX_SIGNATURE_BYTES,
    )? {
        bail!("remote-cache signature for {manifest_object} does not exist");
    }
    let manifest_bytes = fs::read(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let signature = fs::read(&signature_path)
        .with_context(|| format!("failed to read {}", signature_path.display()))?;
    verifier.verify(&manifest_bytes, &signature)?;

    let manifest: RemoteManifest = serde_json::from_slice(&manifest_bytes)
        .context("failed to parse signed remote-cache manifest")?;
    if canonical_manifest_bytes(&manifest)? != manifest_bytes {
        bail!("signed remote-cache manifest is not in canonical lev JSON form");
    }
    validate_manifest(&manifest, identity)?;

    let expected_blobs = collect_expected_blobs(&manifest.entries)?;
    let compressed_root = staging.path.join("downloaded");
    let raw_root = staging.path.join("raw");
    fs::create_dir_all(&compressed_root)
        .with_context(|| format!("failed to create {}", compressed_root.display()))?;
    fs::create_dir_all(&raw_root)
        .with_context(|| format!("failed to create {}", raw_root.display()))?;

    let mut downloaded_bytes = 0_u64;
    for (sha256, expected) in &expected_blobs {
        let compressed = compressed_root.join(format!("{sha256}.zst"));
        let object = blob_object_path(sha256)?;
        if !transport.fetch(&object, &compressed, expected.compressed_bytes)? {
            bail!("remote-cache blob {object} is missing");
        }
        downloaded_bytes = downloaded_bytes
            .checked_add(expected.compressed_bytes)
            .context("remote download size overflow")?;
        let raw = raw_root.join(sha256);
        decode_blob(&compressed, expected, sha256, &raw)?;
    }
    validate_downloaded_references(&manifest.entries, &raw_root)?;

    let cache_root = cache.lake_dir(&identity.toolchain);
    let _cache_lock = lake_artifact_cache::lock_exclusive(cache, &identity.toolchain)?;
    fs::create_dir_all(&cache_root)
        .with_context(|| format!("failed to create {}", cache_root.display()))?;

    // Complete the entire conflict preflight before creating one destination.
    // This makes ordinary conflicts a zero-mutation failure and leaves rollback
    // only for unexpected filesystem errors during publication.
    let mut existing = BTreeSet::new();
    for entry in &manifest.entries {
        let destination = local_destination(&cache_root, &entry.path)?;
        secure_destination_ancestors(&cache_root, &destination, false)?;
        if path_exists(&destination)? {
            validate_existing_destination(&destination, entry)?;
            existing.insert(entry.path.clone());
        }
    }

    let mut publication_order = manifest.entries.iter().collect::<Vec<_>>();
    publication_order.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut created = Vec::<PathBuf>::new();
    let publication = (|| -> Result<(u64, u64)> {
        let mut files_created = 0_u64;
        let mut files_reused = 0_u64;
        for entry in publication_order {
            if existing.contains(&entry.path) {
                files_reused += 1;
                continue;
            }
            let destination = local_destination(&cache_root, &entry.path)?;
            secure_destination_ancestors(&cache_root, &destination, true)?;
            let source = raw_root.join(&entry.sha256);
            match fs::hard_link(&source, &destination) {
                Ok(()) => {
                    created.push(destination);
                    files_created += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // A direct Lake process does not participate in lev's lock.
                    // Treat a matching race as reuse and reject a conflict.
                    validate_existing_destination(&destination, entry)?;
                    files_reused += 1;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to publish remote-cache file {}",
                            destination.display()
                        )
                    });
                }
            }
        }
        Ok((files_created, files_reused))
    })();
    let (files_created, files_reused) = match publication {
        Ok(counts) => counts,
        Err(error) => {
            for path in created.iter().rev() {
                let _ = fs::remove_file(path);
            }
            return Err(error).context("remote-cache publication rolled back");
        }
    };

    Ok(PullReport {
        namespace: identity.namespace.clone(),
        revision: identity.revision.clone(),
        toolchain: identity.toolchain.clone(),
        platform: identity.platform.clone(),
        manifest: manifest_object,
        trusted_key_fingerprint: verifier.fingerprint(),
        entries: manifest.entries.len() as u64,
        unique_blobs: expected_blobs.len() as u64,
        compressed_bytes_downloaded: downloaded_bytes,
        files_created,
        files_reused,
    })
}

fn validate_identity(identity: &RemoteCacheIdentity) -> Result<()> {
    RemoteCacheIdentity::new(
        &identity.namespace,
        &identity.revision,
        &identity.toolchain,
        &identity.platform,
    )
    .map(|_| ())
}

fn validate_manifest(manifest: &RemoteManifest, expected: &RemoteCacheIdentity) -> Result<()> {
    if manifest.schema != MANIFEST_SCHEMA {
        bail!("unsupported remote-cache schema {:?}", manifest.schema);
    }
    if manifest.kind != MANIFEST_KIND {
        bail!("unexpected remote-cache manifest kind {:?}", manifest.kind);
    }
    if manifest.namespace != expected.namespace
        || manifest.revision != expected.revision
        || manifest.toolchain != expected.toolchain
        || manifest.platform != expected.platform
    {
        bail!("signed remote-cache manifest identity does not match the requested snapshot");
    }
    if manifest.entries.len() > MAX_MANIFEST_ENTRIES {
        bail!(
            "remote-cache manifest has {} entries, limit is {MAX_MANIFEST_ENTRIES}",
            manifest.entries.len()
        );
    }
    for entry in &manifest.entries {
        validate_entry(entry)?;
    }
    reject_duplicate_paths(&manifest.entries)?;
    if !manifest
        .entries
        .windows(2)
        .all(|pair| pair[0].path < pair[1].path)
    {
        bail!("remote-cache manifest entries are not sorted by path");
    }
    Ok(())
}

fn validate_entry(entry: &ManifestEntry) -> Result<()> {
    validate_entry_path(entry.kind, &entry.path)?;
    validate_sha256(&entry.sha256, "uncompressed SHA-256")?;
    validate_sha256(&entry.compressed_sha256, "compressed SHA-256")?;
    if entry.bytes > MAX_UNCOMPRESSED_BLOB_BYTES {
        bail!(
            "remote-cache entry {} exceeds the uncompressed size limit",
            entry.path
        );
    }
    if entry.compressed_bytes > MAX_COMPRESSED_BLOB_BYTES {
        bail!(
            "remote-cache entry {} exceeds the compressed size limit",
            entry.path
        );
    }
    if entry.kind == ManifestEntryKind::Mapping && entry.bytes > MAX_MAPPING_BYTES {
        bail!("remote-cache mapping {} is implausibly large", entry.path);
    }
    Ok(())
}

fn validate_prepared_references(entries: &[PreparedEntry]) -> Result<()> {
    let artifacts = entries
        .iter()
        .filter(|entry| entry.kind == ManifestEntryKind::Artifact)
        .map(|entry| {
            entry
                .relative_path
                .strip_prefix("artifacts/")
                .expect("validated artifact path")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let referenced = entries
        .iter()
        .filter_map(|entry| entry.mapping_references.as_ref())
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>();
    if artifacts != referenced {
        report_reference_difference(&artifacts, &referenced)?;
    }
    Ok(())
}

fn validate_downloaded_references(entries: &[ManifestEntry], raw_root: &Path) -> Result<()> {
    let artifacts = entries
        .iter()
        .filter(|entry| entry.kind == ManifestEntryKind::Artifact)
        .map(|entry| {
            entry
                .path
                .strip_prefix("artifacts/")
                .expect("validated artifact path")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let mut referenced = BTreeSet::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == ManifestEntryKind::Mapping)
    {
        let bytes = bounded_io::read_file(&raw_root.join(&entry.sha256), MAX_MAPPING_BYTES)?;
        referenced.extend(
            lake_artifact_cache::mapping_artifacts(&bytes)
                .with_context(|| format!("invalid downloaded mapping {}", entry.path))?,
        );
    }
    if artifacts != referenced {
        report_reference_difference(&artifacts, &referenced)?;
    }
    Ok(())
}

fn report_reference_difference(
    artifacts: &BTreeSet<String>,
    referenced: &BTreeSet<String>,
) -> Result<()> {
    if let Some(missing) = referenced.difference(artifacts).next() {
        bail!("remote-cache mapping references undeclared artifact {missing:?}");
    }
    if let Some(extra) = artifacts.difference(referenced).next() {
        bail!("remote-cache manifest contains unreferenced artifact {extra:?}");
    }
    bail!("remote-cache artifact reference set is inconsistent")
}

fn collect_expected_blobs(entries: &[ManifestEntry]) -> Result<BTreeMap<String, ExpectedBlob>> {
    let mut blobs = BTreeMap::new();
    for entry in entries {
        let expected = ExpectedBlob {
            bytes: entry.bytes,
            compressed_sha256: entry.compressed_sha256.clone(),
            compressed_bytes: entry.compressed_bytes,
        };
        match blobs.insert(entry.sha256.clone(), expected.clone()) {
            Some(previous) if previous != expected => {
                bail!(
                    "remote-cache manifest gives conflicting metadata for blob {}",
                    entry.sha256
                );
            }
            _ => {}
        }
    }
    Ok(blobs)
}

fn reject_duplicate_paths(entries: &[ManifestEntry]) -> Result<()> {
    let mut paths = BTreeSet::new();
    for entry in entries {
        if !paths.insert(&entry.path) {
            bail!("duplicate remote-cache destination {:?}", entry.path);
        }
    }
    Ok(())
}

fn canonical_manifest_bytes(manifest: &RemoteManifest) -> Result<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec(manifest).context("failed to serialize remote-cache manifest")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn manifest_object_path(identity: &RemoteCacheIdentity) -> String {
    format!(
        "remote-v1/manifests/{}/{}/{}/{}.json",
        identity.namespace,
        digest(identity.toolchain.as_bytes()),
        identity.platform,
        identity.revision
    )
}

fn signature_object_path(identity: &RemoteCacheIdentity) -> String {
    format!(
        "remote-v1/manifests/{}/{}/{}/{}.sig",
        identity.namespace,
        digest(identity.toolchain.as_bytes()),
        identity.platform,
        identity.revision
    )
}

fn blob_object_path(sha256: &str) -> Result<String> {
    validate_sha256(sha256, "blob SHA-256")?;
    Ok(format!(
        "remote-v1/blobs/sha256/{}/{}.zst",
        &sha256[..2],
        sha256
    ))
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if !crate::core::hex::is_sha256(value) {
        bail!("invalid {label} {value:?}");
    }
    Ok(())
}

fn validate_namespace(namespace: &str) -> Result<()> {
    if namespace.len() > 512 {
        bail!("remote-cache namespace is too long");
    }
    let segments = split_relative_path(namespace, "remote-cache path")?;
    if segments.is_empty() {
        bail!("remote-cache namespace cannot be empty");
    }
    for segment in segments {
        validate_component("namespace segment", segment, 128)?;
    }
    Ok(())
}

fn validate_component(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || matches!(value, "." | "..")
    {
        bail!("invalid remote-cache {label} {value:?}");
    }
    Ok(())
}

fn validate_entry_path(kind: ManifestEntryKind, path: &str) -> Result<()> {
    let segments = split_relative_path(path, "remote-cache path")?;
    match kind {
        ManifestEntryKind::Artifact => {
            if segments.len() != 2 || segments[0] != "artifacts" {
                bail!("invalid artifact destination {path:?}");
            }
            lake_artifact_cache::validate_artifact_name(segments[1])?;
        }
        ManifestEntryKind::Mapping => {
            if segments.len() < 3
                || segments[0] != "outputs"
                || !segments.last().is_some_and(|name| name.ends_with(".json"))
            {
                bail!("invalid output-mapping destination {path:?}");
            }
        }
    }
    Ok(())
}

fn manifest_path_from_local(path: &Path) -> Result<String> {
    let mut segments = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!("unsafe local cache-relative path {}", path.display());
        };
        let segment = component
            .to_str()
            .with_context(|| format!("cache path is not UTF-8: {}", path.display()))?;
        if segment.contains('\\') || matches!(segment, "." | "..") {
            bail!(
                "cache path cannot be represented remotely: {}",
                path.display()
            );
        }
        segments.push(segment);
    }
    if segments.is_empty() {
        bail!("empty local cache-relative path");
    }
    Ok(segments.join("/"))
}

fn local_destination(root: &Path, path: &str) -> Result<PathBuf> {
    let mut destination = root.to_owned();
    for segment in split_relative_path(path, "remote-cache path")? {
        destination.push(segment);
    }
    Ok(destination)
}

fn compress_file(source: &Path, destination: &Path) -> Result<PreparedBlob> {
    let input =
        File::open(source).with_context(|| format!("failed to read {}", source.display()))?;
    compress_reader(input, destination)
        .with_context(|| format!("failed to compress {}", source.display()))
}

fn compress_reader(reader: impl Read, destination: &Path) -> Result<PreparedBlob> {
    let output = create_new_file(destination)?;
    let mut reader = DigestReader::new(reader);
    let mut encoder =
        zstd::stream::write::Encoder::new(output, ZSTD_LEVEL).context("failed to start zstd")?;
    io::copy(&mut reader, &mut encoder).context("failed to compress remote-cache blob")?;
    let output = encoder.finish().context("failed to finish zstd stream")?;
    output
        .sync_all()
        .with_context(|| format!("failed to sync {}", destination.display()))?;
    let (sha256, bytes) = reader.finish();
    if bytes > MAX_UNCOMPRESSED_BLOB_BYTES {
        bail!("remote-cache source exceeds the uncompressed blob limit");
    }
    let (compressed_sha256, compressed_bytes) = file_hash::sha256_with_size(destination)?;
    if compressed_bytes > MAX_COMPRESSED_BLOB_BYTES {
        bail!("remote-cache source exceeds the compressed blob limit");
    }
    Ok(PreparedBlob {
        sha256,
        bytes,
        compressed_sha256,
        compressed_bytes,
        compressed_path: destination.to_owned(),
    })
}

fn inspect_existing_blob(
    path: &Path,
    expected_sha256: &str,
    expected_bytes: u64,
) -> Result<PreparedBlob> {
    let (compressed_sha256, compressed_bytes) = file_hash::sha256_with_size(path)?;
    if compressed_bytes > MAX_COMPRESSED_BLOB_BYTES {
        bail!("existing remote-cache blob is too large");
    }
    let input = File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(input)
        .context("existing remote blob is not valid zstd")?;
    let mut digest = DigestWriter::new(io::sink());
    let copied = io::copy(
        &mut decoder.take(expected_bytes.saturating_add(1)),
        &mut digest,
    )
    .context("failed to decompress existing remote blob")?;
    let (actual_sha256, actual_bytes, _) = digest.finish();
    if copied != expected_bytes
        || actual_bytes != expected_bytes
        || actual_sha256 != expected_sha256
    {
        bail!(
            "remote blob {} does not match its uncompressed SHA-256 object name",
            path.display()
        );
    }
    Ok(PreparedBlob {
        sha256: actual_sha256,
        bytes: actual_bytes,
        compressed_sha256,
        compressed_bytes,
        compressed_path: path.to_owned(),
    })
}

fn decode_blob(
    compressed: &Path,
    expected: &ExpectedBlob,
    expected_sha256: &str,
    destination: &Path,
) -> Result<()> {
    let (compressed_sha256, compressed_bytes) = file_hash::sha256_with_size(compressed)?;
    if compressed_bytes != expected.compressed_bytes {
        bail!(
            "compressed size mismatch for blob {expected_sha256}: expected {}, received {}",
            expected.compressed_bytes,
            compressed_bytes
        );
    }
    if compressed_sha256 != expected.compressed_sha256 {
        bail!("compressed SHA-256 mismatch for blob {expected_sha256}");
    }

    let input = File::open(compressed)
        .with_context(|| format!("failed to read {}", compressed.display()))?;
    let decoder = zstd::stream::read::Decoder::new(input)
        .with_context(|| format!("blob {expected_sha256} is not valid zstd"))?;
    let output = create_new_file(destination)?;
    let mut writer = DigestWriter::new(output);
    let copied = io::copy(
        &mut decoder.take(expected.bytes.saturating_add(1)),
        &mut writer,
    )
    .with_context(|| format!("failed to decompress blob {expected_sha256}"))?;
    let (actual_sha256, actual_bytes, output) = writer.finish();
    output
        .sync_all()
        .with_context(|| format!("failed to sync {}", destination.display()))?;
    if copied != expected.bytes
        || actual_bytes != expected.bytes
        || actual_sha256 != expected_sha256
    {
        let _ = fs::remove_file(destination);
        bail!("uncompressed SHA-256 or size mismatch for blob {expected_sha256}");
    }
    Ok(())
}

fn validate_existing_destination(destination: &Path, entry: &ManifestEntry) -> Result<()> {
    let metadata = fs::symlink_metadata(destination)
        .with_context(|| format!("failed to inspect {}", destination.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "remote-cache destination {} exists but is not a regular file",
            destination.display()
        );
    }
    let (sha256, bytes) = file_hash::sha256_with_size(destination)?;
    if sha256 != entry.sha256 || bytes != entry.bytes {
        bail!(
            "refusing to replace conflicting remote-cache destination {}",
            destination.display()
        );
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn secure_destination_ancestors(
    root: &Path,
    destination: &Path,
    create_missing: bool,
) -> Result<()> {
    let parent = destination
        .parent()
        .context("remote-cache destination has no parent")?;
    let relative = parent
        .strip_prefix(root)
        .context("remote-cache destination escaped the Lake cache root")?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("unsafe remote-cache destination {}", destination.display());
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => bail!(
                "remote-cache destination ancestor {} is not a real directory",
                current.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound && create_missing => {
                create_real_directory(&current)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

struct DigestReader<R> {
    inner: R,
    digest: Sha256,
    bytes: u64,
}

impl<R> DigestReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (String, u64) {
        (format!("{:x}", self.digest.finalize()), self.bytes)
    }
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.digest.update(&buffer[..read]);
        self.bytes = self
            .bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("input size overflow"))?;
        Ok(read)
    }
}

struct DigestWriter<W> {
    inner: W,
    digest: Sha256,
    bytes: u64,
}

impl<W> DigestWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (String, u64, W) {
        (
            format!("{:x}", self.digest.finalize()),
            self.bytes,
            self.inner,
        )
    }
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.digest.update(&buffer[..written]);
        self.bytes = self
            .bytes
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("output size overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct StagingDirectory {
    path: PathBuf,
}

impl StagingDirectory {
    fn create(cache: &CacheLayout) -> Result<Self> {
        let root = cache.root.join("remote-staging-v1");
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        for _ in 0..32 {
            let mut random = [0_u8; 16];
            OsRng.fill_bytes(&mut random);
            let name = crate::cache::lowercase_hex(&random);
            let path = root.join(format!("{}-{name}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
        bail!("failed to allocate a unique remote-cache staging directory")
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::thread;

    use tempfile::tempdir;

    use crate::cache::CacheLayout;
    use crate::core::platform;
    use crate::core::signing::{ManifestSigner, generate_key_pair};

    use super::{
        RemoteCacheIdentity, RemoteManifest, canonical_manifest_bytes, pull, push,
        signature_object_path,
    };
    use crate::core::object_transport::ObjectTransport;

    fn populated_cache(root: &Path) -> CacheLayout {
        let cache = CacheLayout {
            root: root.to_owned(),
        };
        let lake = cache.lake_dir("leanprover/lean4:v4.test");
        fs::create_dir_all(lake.join("artifacts")).unwrap();
        fs::create_dir_all(lake.join("outputs/root")).unwrap();
        fs::write(lake.join("artifacts/0123456789abcdef.olean"), "olean").unwrap();
        fs::write(
            lake.join("outputs/root/1111111111111111.json"),
            r#"{"schemaVersion":"2026-02-25","service":null,
                "data":"0123456789abcdef.olean"}"#,
        )
        .unwrap();
        cache
    }

    fn identity() -> RemoteCacheIdentity {
        RemoteCacheIdentity::new(
            "tests/project",
            "revision-1",
            "leanprover/lean4:v4.test",
            platform::host_id(),
        )
        .unwrap()
    }

    #[test]
    fn local_remote_round_trip_is_signed_and_idempotent() {
        let temp = tempdir().unwrap();
        let source = populated_cache(&temp.path().join("source"));
        let destination = CacheLayout {
            root: temp.path().join("destination"),
        };
        let remote = temp.path().join("remote");
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();

        let first = push(
            &source,
            remote.to_str().unwrap(),
            &identity(),
            &private,
            false,
        )
        .unwrap();
        assert_eq!(first.artifacts, 1);
        assert_eq!(first.mappings, 1);
        assert_eq!(first.blobs_uploaded, 2);

        let second = push(
            &source,
            remote.to_str().unwrap(),
            &identity(),
            &private,
            false,
        )
        .unwrap();
        assert_eq!(second.blobs_uploaded, 0);
        assert_eq!(second.blobs_reused, 2);

        let pulled = pull(
            &destination,
            remote.to_str().unwrap(),
            &identity(),
            &public,
            false,
        )
        .unwrap();
        assert_eq!(pulled.files_created, 2);
        let lake = destination.lake_dir("leanprover/lean4:v4.test");
        assert_eq!(
            fs::read_to_string(lake.join("artifacts/0123456789abcdef.olean")).unwrap(),
            "olean"
        );
        assert!(lake.join("outputs/root/1111111111111111.json").is_file());

        let pulled_again = pull(
            &destination,
            remote.to_str().unwrap(),
            &identity(),
            &public,
            false,
        )
        .unwrap();
        assert_eq!(pulled_again.files_created, 0);
        assert_eq!(pulled_again.files_reused, 2);
    }

    #[test]
    fn signature_tampering_is_rejected_before_cache_mutation() {
        let temp = tempdir().unwrap();
        let source = populated_cache(&temp.path().join("source"));
        let destination = CacheLayout {
            root: temp.path().join("destination"),
        };
        let remote = temp.path().join("remote");
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();
        let report = push(
            &source,
            remote.to_str().unwrap(),
            &identity(),
            &private,
            false,
        )
        .unwrap();
        let manifest = remote.join(report.manifest);
        fs::write(&manifest, b"{}\n").unwrap();

        let error = pull(
            &destination,
            remote.to_str().unwrap(),
            &identity(),
            &public,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("signature is invalid"), "{error}");
        assert!(!destination.lake_dir(&identity().toolchain).exists());
    }

    #[test]
    fn corrupted_blob_and_local_conflict_are_rejected_transactionally() {
        let temp = tempdir().unwrap();
        let source = populated_cache(&temp.path().join("source"));
        let remote = temp.path().join("remote");
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();
        push(
            &source,
            remote.to_str().unwrap(),
            &identity(),
            &private,
            false,
        )
        .unwrap();

        let conflict = CacheLayout {
            root: temp.path().join("conflict"),
        };
        let conflict_path = conflict
            .lake_dir(&identity().toolchain)
            .join("outputs/root/1111111111111111.json");
        fs::create_dir_all(conflict_path.parent().unwrap()).unwrap();
        fs::write(&conflict_path, "conflict").unwrap();
        let error = pull(
            &conflict,
            remote.to_str().unwrap(),
            &identity(),
            &public,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("refusing to replace conflicting"), "{error}");
        assert_eq!(fs::read_to_string(&conflict_path).unwrap(), "conflict");
        assert!(
            !conflict
                .lake_dir(&identity().toolchain)
                .join("artifacts/0123456789abcdef.olean")
                .exists()
        );

        let blob = fs::read_dir(remote.join("remote-v1/blobs/sha256"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let blob = fs::read_dir(blob).unwrap().next().unwrap().unwrap().path();
        fs::write(&blob, "corrupt").unwrap();
        let clean = CacheLayout {
            root: temp.path().join("clean"),
        };
        let error = pull(
            &clean,
            remote.to_str().unwrap(),
            &identity(),
            &public,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("compressed size mismatch")
                || error.contains("compressed SHA-256 mismatch"),
            "{error}"
        );
        assert!(!clean.lake_dir(&identity().toolchain).exists());
    }

    #[test]
    fn signed_path_traversal_is_rejected_before_download() {
        let temp = tempdir().unwrap();
        let source = populated_cache(&temp.path().join("source"));
        let destination = CacheLayout {
            root: temp.path().join("destination"),
        };
        let remote = temp.path().join("remote");
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();
        let report = push(
            &source,
            remote.to_str().unwrap(),
            &identity(),
            &private,
            false,
        )
        .unwrap();

        let manifest_path = remote.join(&report.manifest);
        let mut manifest: RemoteManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest
            .entries
            .iter_mut()
            .find(|entry| entry.path.starts_with("outputs/"))
            .unwrap()
            .path = "outputs/root/../../escape.json".to_owned();
        manifest
            .entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        let bytes = canonical_manifest_bytes(&manifest).unwrap();
        let signature = ManifestSigner::load(&private).unwrap().sign(&bytes);
        fs::write(&manifest_path, bytes).unwrap();
        fs::write(
            remote.join(signature_object_path(&identity())),
            signature.as_bytes(),
        )
        .unwrap();

        let error = pull(
            &destination,
            remote.to_str().unwrap(),
            &identity(),
            &public,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unsafe remote-cache path"), "{error}");
        assert!(!destination.lake_dir(&identity().toolchain).exists());
        assert!(!temp.path().join("escape.json").exists());
    }

    #[test]
    fn deduplicates_equal_files_and_supports_concurrent_idempotent_pushes() {
        let temp = tempdir().unwrap();
        let source = populated_cache(&temp.path().join("source"));
        let lake = source.lake_dir(&identity().toolchain);
        let first_mapping = fs::read(lake.join("outputs/root/1111111111111111.json")).unwrap();
        fs::write(
            lake.join("outputs/root/2222222222222222.json"),
            first_mapping,
        )
        .unwrap();
        let remote = temp.path().join("remote");
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();

        let mut workers = Vec::new();
        for _ in 0..2 {
            let cache = source.clone();
            let remote = remote.clone();
            let private = private.clone();
            let identity = identity();
            workers.push(thread::spawn(move || {
                push(&cache, remote.to_str().unwrap(), &identity, &private, false)
            }));
        }
        let reports = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        for report in &reports {
            assert_eq!(report.entries, 3);
            assert_eq!(report.unique_blobs, 2);
        }
        assert_eq!(
            reports
                .iter()
                .map(|report| report.blobs_uploaded)
                .sum::<u64>(),
            2
        );
    }

    #[test]
    fn rejects_unsafe_identity_and_non_loopback_http() {
        assert!(
            RemoteCacheIdentity::new(
                "../escape",
                "revision",
                "leanprover/lean4:v4.test",
                platform::host_id()
            )
            .is_err()
        );
        let error = ObjectTransport::parse("http://example.com/cache", false)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("use HTTPS"), "{error}");
        assert!(ObjectTransport::parse("http://127.0.0.1:1234/cache", false).is_ok());
    }
}
