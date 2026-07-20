//! Signed incremental distribution for Lean toolchains.
//!
//! Regular files are split with FastCDC. Installation reuses data in this order:
//!
//! 1. an exact complete file object already in lev's toolchain CAS;
//! 2. matching FastCDC ranges recovered from older objects at the same path;
//! 3. authenticated compressed chunks fetched from the remote.
//!
//! Chunks live only in staging. The regular toolchain store remains the sole
//! persistent copy.
//!
//! Remote layout:
//!
//! ```text
//! toolchains-v1/
//! |-- chunks/sha256/<prefix>/<raw-sha256>.<compressed-sha256>.zst
//! `-- manifests/<toolchain-sha256>/<platform>.json
//!     manifests/<toolchain-sha256>/<platform>.sig
//! ```
//!
//! The signed manifest binds paths, modes, digests, chunks, toolchain, platform,
//! and chunking parameters.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use fastcdc::v2020::StreamCDC;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::digest;
use crate::core::atomic_file::{create_new_file, write_new_file};
use crate::core::file_hash;
use crate::core::object_transport::ObjectTransport;
use crate::core::platform;
use crate::core::signing::{ManifestSigner, ManifestVerifier};
use crate::toolchain::store::{ChunkProvenance, ImportResult, ToolchainStore};

const SCHEMA: &str = "lev.toolchain-chunks/v1";
const KIND: &str = "lean-toolchain";
// These values are part of the signed format. Changing them requires a new
// algorithm identifier so existing mirrors remain compatible.
const ALGORITHM: &str = "fastcdc-v2020-level1";
const CHUNK_MIN: usize = 256 * 1024;
const CHUNK_AVG: usize = 1024 * 1024;
const CHUNK_MAX: usize = 4 * 1024 * 1024;
const ZSTD_LEVEL: i32 = 3;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
const MAX_COMPRESSED_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 2_000_000;
const MAX_LOGICAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Chunking {
    algorithm: String,
    min_bytes: u64,
    average_bytes: u64,
    max_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ChunkReference {
    sha256: String,
    bytes: u64,
    compressed_sha256: String,
    compressed_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LayoutEntry {
    Directory {
        path: String,
    },
    File {
        path: String,
        mode: u32,
        sha256: String,
        bytes: u64,
        chunks: Vec<ChunkReference>,
    },
    Symlink {
        path: String,
        target: String,
    },
}

impl LayoutEntry {
    fn path(&self) -> &str {
        match self {
            Self::Directory { path } | Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }

    fn kind_order(&self) -> u8 {
        match self {
            Self::Directory { .. } => 0,
            Self::File { .. } => 1,
            Self::Symlink { .. } => 2,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ToolchainChunkManifest {
    schema: String,
    kind: String,
    toolchain: String,
    platform: String,
    chunking: Chunking,
    logical_bytes: u64,
    entries: Vec<LayoutEntry>,
}

#[derive(Debug, Clone)]
struct ResolvedChunk {
    reference: ChunkReference,
    compressed_path: PathBuf,
}

/// Accounting returned after publishing a signed toolchain tree.
#[derive(Debug, Serialize)]
pub struct PublishReport {
    pub toolchain: String,
    pub platform: String,
    pub manifest: String,
    pub signing_key_fingerprint: String,
    pub entries: u64,
    pub files: u64,
    pub logical_bytes: u64,
    pub unique_chunks: u64,
    pub chunks_uploaded: u64,
    pub chunks_reused: u64,
    pub compressed_bytes_uploaded: u64,
}

/// Accounting returned after an incremental signed installation.
#[derive(Debug, Serialize)]
pub struct InstallReport {
    pub toolchain: String,
    pub platform: String,
    pub manifest: String,
    pub trusted_key_fingerprint: String,
    pub manifest_sha256: String,
    pub unique_chunks: u64,
    pub complete_files_reused: u64,
    pub complete_bytes_reused: u64,
    pub local_chunks_reused: u64,
    pub local_chunk_bytes_reused: u64,
    pub chunks_downloaded: u64,
    pub compressed_bytes_downloaded: u64,
    pub imported: ImportResult,
}

/// Authenticated metadata extracted from one published chunk manifest.
///
/// The signed index records only this bounded summary. Installation still
/// downloads and verifies the complete original manifest before trusting any
/// tree entry or chunk reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishedManifestSummary {
    pub toolchain: String,
    pub platform: String,
    pub manifest: String,
    pub manifest_sha256: String,
    pub logical_bytes: u64,
    pub entries: u64,
    pub files: u64,
    pub unique_chunks: u64,
}

/// Authenticate and summarize exact published manifest bytes.
pub(crate) fn inspect_signed_manifest(
    manifest_object: &str,
    manifest_bytes: &[u8],
    signature: &[u8],
    verifier: &ManifestVerifier,
) -> Result<PublishedManifestSummary> {
    verifier.verify(manifest_bytes, signature)?;
    let manifest: ToolchainChunkManifest = serde_json::from_slice(manifest_bytes)
        .context("failed to parse signed toolchain chunk manifest")?;
    if canonical_manifest_bytes(&manifest)? != manifest_bytes {
        bail!("signed toolchain chunk manifest is not in canonical lev JSON form");
    }
    validate_manifest(&manifest, &manifest.toolchain, &manifest.platform)?;
    let expected_object = manifest_object_path(&manifest.toolchain, &manifest.platform);
    if manifest_object != expected_object {
        bail!("toolchain manifest object {manifest_object:?} does not match its signed identity");
    }
    let files = manifest
        .entries
        .iter()
        .filter(|entry| matches!(entry, LayoutEntry::File { .. }))
        .count() as u64;
    let unique_chunks = collect_chunks(&manifest.entries)?.len() as u64;
    Ok(PublishedManifestSummary {
        toolchain: manifest.toolchain,
        platform: manifest.platform,
        manifest: manifest_object.to_owned(),
        manifest_sha256: format!("{:x}", Sha256::digest(manifest_bytes)),
        logical_bytes: manifest.logical_bytes,
        entries: manifest.entries.len() as u64,
        files,
        unique_chunks,
    })
}

/// Publish a complete source tree as deduplicated signed chunks.
pub fn publish(
    store: &ToolchainStore,
    source: &Path,
    toolchain: &str,
    platform: &str,
    remote: &str,
    signing_key: &Path,
    allow_insecure_http: bool,
) -> Result<PublishReport> {
    validate_identity(toolchain, platform)?;
    let source = fs::canonicalize(source)
        .with_context(|| format!("failed to resolve toolchain root {}", source.display()))?;
    require_lean_root(&source)?;
    let signer = ManifestSigner::load(signing_key)?;
    let transport = ObjectTransport::parse(remote, allow_insecure_http)?;
    let staging = StagingDirectory::create(store, "publish")?;
    let compressed_root = staging.path.join("compressed");
    fs::create_dir_all(&compressed_root)
        .with_context(|| format!("failed to create {}", compressed_root.display()))?;

    let mut state = PublishState {
        transport: &transport,
        compressed_root: &compressed_root,
        chunks: BTreeMap::new(),
        chunks_uploaded: 0,
        compressed_bytes_uploaded: 0,
        entries: Vec::new(),
        logical_bytes: 0,
        files: 0,
    };
    scan_source_tree(&source, Path::new(""), &mut state)?;
    state.entries.sort_by(|left, right| {
        left.path()
            .cmp(right.path())
            .then_with(|| left.kind_order().cmp(&right.kind_order()))
    });

    let manifest = ToolchainChunkManifest {
        schema: SCHEMA.to_owned(),
        kind: KIND.to_owned(),
        toolchain: toolchain.to_owned(),
        platform: platform.to_owned(),
        chunking: expected_chunking(),
        logical_bytes: state.logical_bytes,
        entries: state.entries,
    };
    validate_manifest(&manifest, toolchain, platform)?;
    let manifest_bytes = canonical_manifest_bytes(&manifest)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("toolchain chunk manifest exceeds the size limit");
    }
    let signature = signer.sign(&manifest_bytes);
    let manifest_file = staging.path.join("manifest.json");
    let signature_file = staging.path.join("manifest.sig");
    write_new_file(&manifest_file, &manifest_bytes)?;
    write_new_file(&signature_file, signature.as_bytes())?;
    let manifest_object = manifest_object_path(toolchain, platform);
    transport.publish_immutable(&manifest_object, &manifest_file, MAX_MANIFEST_BYTES)?;
    transport.publish_immutable(
        &signature_object_path(toolchain, platform),
        &signature_file,
        MAX_SIGNATURE_BYTES,
    )?;

    Ok(PublishReport {
        toolchain: toolchain.to_owned(),
        platform: platform.to_owned(),
        manifest: manifest_object,
        signing_key_fingerprint: signer.fingerprint(),
        entries: manifest.entries.len() as u64,
        files: state.files,
        logical_bytes: state.logical_bytes,
        unique_chunks: state.chunks.len() as u64,
        chunks_uploaded: state.chunks_uploaded,
        chunks_reused: state.chunks.len() as u64 - state.chunks_uploaded,
        compressed_bytes_uploaded: state.compressed_bytes_uploaded,
    })
}

struct PublishState<'a> {
    transport: &'a ObjectTransport,
    compressed_root: &'a Path,
    chunks: BTreeMap<String, ChunkReference>,
    chunks_uploaded: u64,
    compressed_bytes_uploaded: u64,
    entries: Vec<LayoutEntry>,
    logical_bytes: u64,
    files: u64,
}

fn scan_source_tree(source: &Path, relative: &Path, state: &mut PublishState<'_>) -> Result<()> {
    let directory = source.join(relative);
    let mut children = fs::read_dir(&directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", directory.display()))?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let path = child.path();
        let relative_path = relative.join(child.file_name());
        validate_local_relative_path(&relative_path)?;
        let manifest_path = manifest_path(&relative_path)?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            validate_symlink_target(&relative_path, &target)?;
            state.entries.push(LayoutEntry::Symlink {
                path: manifest_path,
                target: symlink_target_string(&target)?,
            });
        } else if metadata.is_dir() {
            state.entries.push(LayoutEntry::Directory {
                path: manifest_path,
            });
            scan_source_tree(source, &relative_path, state)?;
        } else if metadata.is_file() {
            let entry = publish_file(&path, &relative_path, &metadata, state)?;
            state.entries.push(entry);
        } else {
            bail!("unsupported file type in toolchain: {}", path.display());
        }
        if state.entries.len() > MAX_ENTRIES {
            bail!("toolchain contains more than {MAX_ENTRIES} entries");
        }
    }
    Ok(())
}

fn publish_file(
    source: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    state: &mut PublishState<'_>,
) -> Result<LayoutEntry> {
    let mode = platform::read_only_mode(metadata);
    if mode & 0o444 == 0 {
        bail!("toolchain file is not readable: {}", source.display());
    }
    let input =
        File::open(source).with_context(|| format!("failed to read {}", source.display()))?;
    let chunker = StreamCDC::new(input, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX);
    let mut file_digest = Sha256::new();
    let mut file_bytes = 0_u64;
    let mut references = Vec::new();

    for result in chunker {
        let chunk = result
            .with_context(|| format!("failed to chunk toolchain file {}", source.display()))?;
        let chunk_sha256 = format!("{:x}", Sha256::digest(&chunk.data));
        file_digest.update(&chunk.data);
        file_bytes = file_bytes
            .checked_add(chunk.length as u64)
            .context("toolchain file size overflow")?;
        let reference = if let Some(reference) = state.chunks.get(&chunk_sha256) {
            if reference.bytes != chunk.length as u64 {
                bail!("SHA-256 collision while chunking {}", source.display());
            }
            reference.clone()
        } else {
            let resolved = compress_chunk(
                &chunk.data,
                &state.compressed_root.join(format!("{}.zst", chunk_sha256)),
            )?;
            let object = chunk_object_path(
                &resolved.reference.sha256,
                &resolved.reference.compressed_sha256,
            )?;
            if state
                .transport
                .publish_if_absent(&object, &resolved.compressed_path)?
            {
                state.chunks_uploaded = state
                    .chunks_uploaded
                    .checked_add(1)
                    .context("uploaded chunk count overflow")?;
                state.compressed_bytes_uploaded = state
                    .compressed_bytes_uploaded
                    .checked_add(resolved.reference.compressed_bytes)
                    .context("uploaded chunk size overflow")?;
            }
            state
                .chunks
                .insert(chunk_sha256.clone(), resolved.reference.clone());
            resolved.reference
        };
        references.push(reference);
    }

    if file_bytes != metadata.len() {
        bail!(
            "{} changed while it was being chunked: expected {} bytes, read {file_bytes}",
            source.display(),
            metadata.len()
        );
    }
    state.logical_bytes = state
        .logical_bytes
        .checked_add(file_bytes)
        .context("toolchain logical size overflow")?;
    if state.logical_bytes > MAX_LOGICAL_BYTES {
        bail!("toolchain exceeds the 64 GiB logical size limit");
    }
    state.files += 1;
    Ok(LayoutEntry::File {
        path: manifest_path(relative)?,
        mode,
        sha256: format!("{:x}", file_digest.finalize()),
        bytes: file_bytes,
        chunks: references,
    })
}

fn compress_chunk(bytes: &[u8], destination: &Path) -> Result<ResolvedChunk> {
    let compressed = zstd::stream::encode_all(Cursor::new(bytes), ZSTD_LEVEL)
        .context("failed to compress chunk")?;
    if compressed.len() as u64 > MAX_COMPRESSED_CHUNK_BYTES {
        bail!("compressed toolchain chunk exceeds the size limit");
    }
    write_new_file(destination, &compressed)?;
    Ok(ResolvedChunk {
        reference: ChunkReference {
            sha256: format!("{:x}", Sha256::digest(bytes)),
            bytes: bytes.len() as u64,
            compressed_sha256: format!("{:x}", Sha256::digest(&compressed)),
            compressed_bytes: compressed.len() as u64,
        },
        compressed_path: destination.to_owned(),
    })
}

/// Install a signed toolchain manifest with local whole-file and chunk reuse.
pub fn install(
    store: &ToolchainStore,
    toolchain: &str,
    platform: &str,
    remote: &str,
    public_key: &Path,
    indexed_manifest_sha256: Option<&str>,
    allow_insecure_http: bool,
) -> Result<InstallReport> {
    validate_identity(toolchain, platform)?;
    let transport = ObjectTransport::parse(remote, allow_insecure_http)?;
    let verifier = ManifestVerifier::load(public_key)?;
    let staging = StagingDirectory::create(store, "install")?;
    let manifest_object = manifest_object_path(toolchain, platform);
    let manifest_file = staging.path.join("manifest.json");
    let signature_file = staging.path.join("manifest.sig");
    if !transport.fetch(&manifest_object, &manifest_file, MAX_MANIFEST_BYTES)? {
        bail!("toolchain chunk manifest {manifest_object} does not exist");
    }
    if !transport.fetch(
        &signature_object_path(toolchain, platform),
        &signature_file,
        MAX_SIGNATURE_BYTES,
    )? {
        bail!("toolchain chunk signature for {manifest_object} does not exist");
    }
    let manifest_bytes = fs::read(&manifest_file)
        .with_context(|| format!("failed to read {}", manifest_file.display()))?;
    let signature = fs::read(&signature_file)
        .with_context(|| format!("failed to read {}", signature_file.display()))?;
    verifier.verify(&manifest_bytes, &signature)?;
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    if let Some(expected) = indexed_manifest_sha256 {
        validate_sha256(expected)?;
        if manifest_sha256 != expected {
            bail!(
                "toolchain manifest SHA-256 does not match the authenticated index: expected {expected}, got {manifest_sha256}"
            );
        }
    }
    let manifest: ToolchainChunkManifest = serde_json::from_slice(&manifest_bytes)
        .context("failed to parse signed toolchain chunk manifest")?;
    if canonical_manifest_bytes(&manifest)? != manifest_bytes {
        bail!("signed toolchain chunk manifest is not in canonical lev JSON form");
    }
    validate_manifest(&manifest, toolchain, platform)?;

    let tree = staging.path.join("tree");
    let raw_chunks = staging.path.join("chunks");
    let compressed = staging.path.join("downloaded");
    fs::create_dir(&tree).with_context(|| format!("failed to create {}", tree.display()))?;
    fs::create_dir(&raw_chunks)
        .with_context(|| format!("failed to create {}", raw_chunks.display()))?;
    fs::create_dir(&compressed)
        .with_context(|| format!("failed to create {}", compressed.display()))?;
    create_directories(&tree, &manifest.entries)?;

    let expected_chunks = collect_chunks(&manifest.entries)?;
    let mut complete_paths = HashSet::new();
    let mut complete_files_reused = 0_u64;
    let mut complete_bytes_reused = 0_u64;
    let mut local_chunks_reused = 0_u64;
    let mut local_chunk_bytes_reused = 0_u64;

    {
        // GC and imports cannot remove candidate objects while this guard is
        // alive. Exact objects are hard-linked into staging before release;
        // recovered ranges are copied into transaction-local chunk files.
        let _store_lock = store.read_lock()?;
        for entry in &manifest.entries {
            let LayoutEntry::File {
                path,
                mode,
                sha256,
                bytes,
                ..
            } = entry
            else {
                continue;
            };
            let Some(object) = store.existing_object(*mode, sha256, *bytes)? else {
                continue;
            };
            let (actual_sha256, actual_bytes) = file_hash::sha256_with_size(&object)?;
            if actual_sha256 != *sha256 || actual_bytes != *bytes {
                bail!(
                    "existing toolchain object is corrupt: {}; run `lev toolchain store verify`",
                    object.display()
                );
            }
            let destination = local_path(&tree, path)?;
            stage_complete_object(&object, &destination, *mode)?;
            complete_paths.insert(path.clone());
            complete_files_reused += 1;
            complete_bytes_reused = complete_bytes_reused
                .checked_add(*bytes)
                .context("complete reused size overflow")?;
        }

        for entry in &manifest.entries {
            let LayoutEntry::File { path, chunks, .. } = entry else {
                continue;
            };
            if complete_paths.contains(path) || chunks.is_empty() {
                continue;
            }
            let wanted = chunks
                .iter()
                .filter(|chunk| !raw_chunks.join(&chunk.sha256).is_file())
                .map(|chunk| (chunk.sha256.clone(), chunk.bytes))
                .collect::<BTreeMap<_, _>>();
            if wanted.is_empty() {
                continue;
            }
            for source in store.file_sources(&manifest_path_to_local(path)?)? {
                let recovered = recover_local_chunks(&source, &wanted, &raw_chunks)?;
                local_chunks_reused = local_chunks_reused
                    .checked_add(recovered.0)
                    .context("local chunk count overflow")?;
                local_chunk_bytes_reused = local_chunk_bytes_reused
                    .checked_add(recovered.1)
                    .context("local chunk size overflow")?;
                if wanted
                    .keys()
                    .all(|sha256| raw_chunks.join(sha256).is_file())
                {
                    break;
                }
            }
        }
    }

    let mut chunks_downloaded = 0_u64;
    let mut compressed_bytes_downloaded = 0_u64;
    let required_chunks = manifest
        .entries
        .iter()
        .filter_map(|entry| match entry {
            LayoutEntry::File { path, chunks, .. } if !complete_paths.contains(path) => {
                Some(chunks)
            }
            _ => None,
        })
        .flatten()
        .map(|chunk| chunk.sha256.as_str())
        .collect::<HashSet<_>>();
    for (sha256, reference) in &expected_chunks {
        if !required_chunks.contains(sha256.as_str()) {
            continue;
        }
        let raw = raw_chunks.join(sha256);
        if raw.is_file() {
            continue;
        }
        let compressed_path = compressed.join(format!("{sha256}.zst"));
        let object = chunk_object_path(sha256, &reference.compressed_sha256)?;
        if !transport.fetch(&object, &compressed_path, reference.compressed_bytes)? {
            bail!("toolchain chunk {object} is missing");
        }
        decode_chunk(&compressed_path, reference, &raw)?;
        chunks_downloaded = chunks_downloaded
            .checked_add(1)
            .context("downloaded chunk count overflow")?;
        compressed_bytes_downloaded = compressed_bytes_downloaded
            .checked_add(reference.compressed_bytes)
            .context("downloaded chunk size overflow")?;
    }

    materialize_files(&tree, &raw_chunks, &manifest.entries, &complete_paths)?;
    materialize_symlinks(&tree, &manifest.entries)?;
    require_lean_root(&tree)?;

    let provenance = ChunkProvenance {
        remote: remote.to_owned(),
        manifest: manifest_object.clone(),
        manifest_sha256: manifest_sha256.clone(),
        signing_key_fingerprint: verifier.fingerprint(),
        platform: platform.to_owned(),
    };
    let imported = store.import_chunks(toolchain, &tree, provenance)?;
    Ok(InstallReport {
        toolchain: toolchain.to_owned(),
        platform: platform.to_owned(),
        manifest: manifest_object,
        trusted_key_fingerprint: verifier.fingerprint(),
        manifest_sha256,
        unique_chunks: expected_chunks.len() as u64,
        complete_files_reused,
        complete_bytes_reused,
        local_chunks_reused,
        local_chunk_bytes_reused,
        chunks_downloaded,
        compressed_bytes_downloaded,
        imported,
    })
}

fn recover_local_chunks(
    source: &crate::toolchain::store::StoredFileSource,
    wanted: &BTreeMap<String, u64>,
    raw_root: &Path,
) -> Result<(u64, u64)> {
    let input = File::open(&source.object)
        .with_context(|| format!("failed to read {}", source.object.display()))?;
    let chunker = StreamCDC::new(input, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX);
    let mut file_digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut recovered = 0_u64;
    let mut recovered_bytes = 0_u64;
    for result in chunker {
        let chunk = result.with_context(|| {
            format!(
                "failed to chunk existing object {}",
                source.object.display()
            )
        })?;
        file_digest.update(&chunk.data);
        bytes = bytes
            .checked_add(chunk.length as u64)
            .context("existing object size overflow")?;
        let chunk_sha256 = format!("{:x}", Sha256::digest(&chunk.data));
        if wanted.get(&chunk_sha256) != Some(&(chunk.length as u64)) {
            continue;
        }
        let destination = raw_root.join(&chunk_sha256);
        if destination.is_file() {
            continue;
        }
        write_new_file(&destination, &chunk.data)?;
        recovered += 1;
        recovered_bytes = recovered_bytes
            .checked_add(chunk.length as u64)
            .context("recovered chunk size overflow")?;
    }
    let actual_sha256 = format!("{:x}", file_digest.finalize());
    if actual_sha256 != source.hash || bytes != source.bytes {
        bail!(
            "existing toolchain object is corrupt: {}; run `lev toolchain store verify`",
            source.object.display()
        );
    }
    Ok((recovered, recovered_bytes))
}

fn decode_chunk(compressed: &Path, expected: &ChunkReference, destination: &Path) -> Result<()> {
    let (compressed_sha256, compressed_bytes) = file_hash::sha256_with_size(compressed)?;
    if compressed_sha256 != expected.compressed_sha256
        || compressed_bytes != expected.compressed_bytes
    {
        bail!("compressed toolchain chunk digest or size mismatch");
    }
    let input = File::open(compressed)
        .with_context(|| format!("failed to read {}", compressed.display()))?;
    let decoder =
        zstd::stream::read::Decoder::new(input).context("toolchain chunk is not valid zstd")?;
    let mut output = create_new_file(destination)?;
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut reader = decoder.take(expected.bytes.saturating_add(1));
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("failed to decompress toolchain chunk")?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .with_context(|| format!("failed to write {}", destination.display()))?;
        digest.update(&buffer[..read]);
        bytes += read as u64;
    }
    output
        .sync_all()
        .with_context(|| format!("failed to sync {}", destination.display()))?;
    let sha256 = format!("{:x}", digest.finalize());
    if bytes != expected.bytes || sha256 != expected.sha256 {
        let _ = fs::remove_file(destination);
        bail!("uncompressed toolchain chunk digest or size mismatch");
    }
    Ok(())
}

fn create_directories(root: &Path, entries: &[LayoutEntry]) -> Result<()> {
    let mut directories = entries
        .iter()
        .filter_map(|entry| match entry {
            LayoutEntry::Directory { path } => Some(path),
            _ => None,
        })
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| path.split('/').count());
    for path in directories {
        let destination = local_path(root, path)?;
        fs::create_dir(&destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
    }
    Ok(())
}

fn materialize_files(
    root: &Path,
    raw_root: &Path,
    entries: &[LayoutEntry],
    complete_paths: &HashSet<String>,
) -> Result<()> {
    for entry in entries {
        let LayoutEntry::File {
            path,
            mode,
            sha256,
            bytes,
            chunks,
        } = entry
        else {
            continue;
        };
        if complete_paths.contains(path) {
            continue;
        }
        let destination = local_path(root, path)?;
        let mut output = create_new_file(&destination)?;
        let mut digest = Sha256::new();
        let mut written = 0_u64;
        for chunk in chunks {
            let mut input = File::open(raw_root.join(&chunk.sha256))
                .with_context(|| format!("missing staged chunk {}", chunk.sha256))?;
            let copied = io::copy(&mut input, &mut output)
                .with_context(|| format!("failed to construct {}", destination.display()))?;
            written = written
                .checked_add(copied)
                .context("reconstructed file size overflow")?;
        }
        output
            .flush()
            .with_context(|| format!("failed to flush {}", destination.display()))?;
        drop(output);
        let mut input = File::open(&destination)
            .with_context(|| format!("failed to verify {}", destination.display()))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = input
                .read(&mut buffer)
                .with_context(|| format!("failed to verify {}", destination.display()))?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        if written != *bytes || format!("{:x}", digest.finalize()) != *sha256 {
            bail!(
                "reconstructed toolchain file failed verification: {}",
                destination.display()
            );
        }
        platform::set_read_only_mode(&destination, *mode)?;
    }
    Ok(())
}

fn materialize_symlinks(root: &Path, entries: &[LayoutEntry]) -> Result<()> {
    for entry in entries {
        let LayoutEntry::Symlink { path, target } = entry else {
            continue;
        };
        let destination = local_path(root, path)?;
        create_symlink(Path::new(target), &destination, root)?;
    }
    Ok(())
}

fn collect_chunks(entries: &[LayoutEntry]) -> Result<BTreeMap<String, ChunkReference>> {
    let mut chunks = BTreeMap::new();
    for entry in entries {
        let LayoutEntry::File {
            chunks: file_chunks,
            ..
        } = entry
        else {
            continue;
        };
        for chunk in file_chunks {
            match chunks.insert(chunk.sha256.clone(), chunk.clone()) {
                Some(previous) if previous != *chunk => {
                    bail!(
                        "toolchain manifest gives conflicting metadata for chunk {}",
                        chunk.sha256
                    );
                }
                _ => {}
            }
        }
    }
    Ok(chunks)
}

fn validate_manifest(
    manifest: &ToolchainChunkManifest,
    toolchain: &str,
    platform: &str,
) -> Result<()> {
    if manifest.schema != SCHEMA || manifest.kind != KIND {
        bail!("unsupported signed toolchain chunk manifest");
    }
    if manifest.toolchain != toolchain || manifest.platform != platform {
        bail!("signed toolchain manifest identity does not match the request");
    }
    if manifest.chunking != expected_chunking() {
        bail!("unsupported toolchain chunking parameters");
    }
    if manifest.entries.len() > MAX_ENTRIES {
        bail!("toolchain manifest contains too many entries");
    }
    if !manifest.entries.windows(2).all(|pair| {
        pair[0].path() < pair[1].path()
            || (pair[0].path() == pair[1].path() && pair[0].kind_order() < pair[1].kind_order())
    }) {
        bail!("toolchain manifest entries are not canonically sorted");
    }

    let mut paths = BTreeMap::<String, u8>::new();
    let mut directories = BTreeSet::<String>::new();
    let mut logical_bytes = 0_u64;
    for entry in &manifest.entries {
        let path = entry.path();
        validate_manifest_path(path)?;
        if paths.insert(path.to_owned(), entry.kind_order()).is_some() {
            bail!("duplicate toolchain manifest path {path:?}");
        }
        match entry {
            LayoutEntry::Directory { .. } => {
                directories.insert(path.to_owned());
            }
            LayoutEntry::File {
                mode,
                sha256,
                bytes,
                chunks,
                ..
            } => {
                validate_mode(*mode)?;
                validate_sha256(sha256)?;
                let chunk_bytes = chunks.iter().try_fold(0_u64, |total, chunk| {
                    validate_chunk(chunk)?;
                    total
                        .checked_add(chunk.bytes)
                        .context("toolchain chunk size overflow")
                })?;
                if chunk_bytes != *bytes || (*bytes == 0) != chunks.is_empty() {
                    bail!("invalid chunk lengths for toolchain file {path:?}");
                }
                if *bytes == 0 && *sha256 != format!("{:x}", Sha256::digest([])) {
                    bail!("invalid digest for empty toolchain file {path:?}");
                }
                logical_bytes = logical_bytes
                    .checked_add(*bytes)
                    .context("toolchain logical size overflow")?;
            }
            LayoutEntry::Symlink { target, .. } => {
                validate_manifest_symlink(path, target)?;
            }
        }
    }
    if logical_bytes != manifest.logical_bytes || logical_bytes > MAX_LOGICAL_BYTES {
        bail!("invalid toolchain manifest logical size");
    }
    for path in paths.keys() {
        match path.rsplit_once('/') {
            Some((parent, _)) if !directories.contains(parent) => {
                bail!("toolchain entry {path:?} has an undeclared parent directory");
            }
            _ => {}
        }
    }
    collect_chunks(&manifest.entries)?;
    let lean = if platform
        .split(['-', '_'])
        .next()
        .is_some_and(|os| os.eq_ignore_ascii_case("windows"))
    {
        "bin/lean.exe"
    } else {
        "bin/lean"
    };
    if paths.get(lean) != Some(&1) {
        bail!("signed toolchain manifest does not contain {lean}");
    }
    Ok(())
}

fn validate_chunk(chunk: &ChunkReference) -> Result<()> {
    validate_sha256(&chunk.sha256)?;
    validate_sha256(&chunk.compressed_sha256)?;
    if chunk.bytes == 0
        || chunk.bytes > CHUNK_MAX as u64
        || chunk.compressed_bytes == 0
        || chunk.compressed_bytes > MAX_COMPRESSED_CHUNK_BYTES
    {
        bail!("invalid toolchain chunk size");
    }
    Ok(())
}

fn expected_chunking() -> Chunking {
    Chunking {
        algorithm: ALGORITHM.to_owned(),
        min_bytes: CHUNK_MIN as u64,
        average_bytes: CHUNK_AVG as u64,
        max_bytes: CHUNK_MAX as u64,
    }
}

fn canonical_manifest_bytes(manifest: &ToolchainChunkManifest) -> Result<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec(manifest).context("failed to serialize toolchain chunk manifest")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn manifest_object_path(toolchain: &str, platform: &str) -> String {
    format!(
        "toolchains-v1/manifests/{}/{}.json",
        digest(toolchain.as_bytes()),
        platform
    )
}

fn signature_object_path(toolchain: &str, platform: &str) -> String {
    format!(
        "toolchains-v1/manifests/{}/{}.sig",
        digest(toolchain.as_bytes()),
        platform
    )
}

fn chunk_object_path(sha256: &str, compressed_sha256: &str) -> Result<String> {
    validate_sha256(sha256)?;
    validate_sha256(compressed_sha256)?;
    Ok(format!(
        "toolchains-v1/chunks/sha256/{}/{}.{}.zst",
        &sha256[..2],
        sha256,
        compressed_sha256
    ))
}

fn validate_identity(toolchain: &str, platform: &str) -> Result<()> {
    if toolchain.is_empty()
        || toolchain.len() > 512
        || toolchain.chars().any(char::is_control)
        || platform.is_empty()
        || platform.len() > 128
        || !platform
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("invalid signed toolchain identity");
    }
    Ok(())
}

fn validate_mode(mode: u32) -> Result<()> {
    if mode & !0o555 != 0 || mode & 0o444 == 0 {
        bail!("invalid toolchain file mode {mode:o}");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if !crate::core::hex::is_sha256(value) {
        bail!("invalid SHA-256 {value:?}");
    }
    Ok(())
}

fn validate_local_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.components().count() > 256
        || path.as_os_str().len() > 32 * 1024
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!("unsafe or implausible toolchain path {}", path.display());
    }
    Ok(())
}

fn validate_manifest_path(path: &str) -> Result<()> {
    manifest_path_to_local(path).map(|_| ())
}

fn manifest_path(path: &Path) -> Result<String> {
    validate_local_relative_path(path)?;
    path.components()
        .map(|component| {
            let Component::Normal(component) = component else {
                unreachable!("validated path component")
            };
            let component = component
                .to_str()
                .with_context(|| format!("toolchain path is not UTF-8: {}", path.display()))?;
            if component.contains(['/', '\\']) || matches!(component, "." | "..") {
                bail!("toolchain path cannot be represented remotely");
            }
            Ok(component)
        })
        .collect::<Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

fn manifest_path_to_local(path: &str) -> Result<PathBuf> {
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains('\\') {
        bail!("unsafe toolchain manifest path {path:?}");
    }
    let mut local = PathBuf::new();
    let mut depth = 0_usize;
    for component in path.split('/') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || component.chars().any(char::is_control)
            || !matches!(
                Path::new(component)
                    .components()
                    .collect::<Vec<_>>()
                    .as_slice(),
                [Component::Normal(_)]
            )
        {
            bail!("unsafe toolchain manifest path {path:?}");
        }
        local.push(component);
        depth += 1;
    }
    if depth > 256 || path.len() > 32 * 1024 {
        bail!("implausible toolchain manifest path");
    }
    Ok(local)
}

fn local_path(root: &Path, path: &str) -> Result<PathBuf> {
    Ok(root.join(manifest_path_to_local(path)?))
}

#[cfg(unix)]
fn stage_complete_object(source: &Path, destination: &Path, _mode: u32) -> Result<()> {
    fs::hard_link(source, destination).with_context(|| {
        format!(
            "failed to reuse complete toolchain object {}",
            source.display()
        )
    })
}

#[cfg(not(unix))]
fn stage_complete_object(source: &Path, destination: &Path, mode: u32) -> Result<()> {
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to stage complete toolchain object {}",
            source.display()
        )
    })?;
    platform::set_read_only_mode(destination, mode)
}

fn symlink_target_string(target: &Path) -> Result<String> {
    if target.is_absolute() {
        bail!("absolute toolchain symlink target {}", target.display());
    }
    let mut components = Vec::new();
    for component in target.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().with_context(|| {
                    format!(
                        "toolchain symlink target is not UTF-8: {}",
                        target.display()
                    )
                })?;
                if value.contains(['/', '\\']) {
                    bail!("invalid toolchain symlink target {}", target.display());
                }
                components.push(value.to_owned());
            }
            Component::CurDir => components.push(".".to_owned()),
            Component::ParentDir => components.push("..".to_owned()),
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute toolchain symlink target {}", target.display())
            }
        }
    }
    if components.is_empty() {
        bail!("empty toolchain symlink target");
    }
    Ok(components.join("/"))
}

fn validate_symlink_target(path: &Path, target: &Path) -> Result<()> {
    let target = symlink_target_string(target)?;
    validate_manifest_symlink(&manifest_path(path)?, &target)
}

fn validate_manifest_symlink(path: &str, target: &str) -> Result<()> {
    if target.is_empty()
        || target.starts_with('/')
        || target.ends_with('/')
        || target.contains('\\')
        || target.chars().any(char::is_control)
    {
        bail!("unsafe symlink target {target:?} for {path:?}");
    }
    let mut depth = path.split('/').count().saturating_sub(1);
    for component in target.split('/') {
        match component {
            "" => bail!("empty symlink target component"),
            "." => {}
            ".." if depth > 0 => depth -= 1,
            ".." => bail!("symlink target escapes toolchain root: {path:?} -> {target:?}"),
            _ => depth += 1,
        }
    }
    Ok(())
}

fn require_lean_root(root: &Path) -> Result<()> {
    if !root.join("bin/lean").is_file() && !root.join("bin/lean.exe").is_file() {
        bail!("{} is not a Lean toolchain root", root.display());
    }
    Ok(())
}

struct StagingDirectory {
    path: PathBuf,
}

impl StagingDirectory {
    fn create(store: &ToolchainStore, purpose: &str) -> Result<Self> {
        let root = store.root.join("chunk-staging-v1");
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        for _ in 0..32 {
            let mut random = [0_u8; 16];
            OsRng.fill_bytes(&mut random);
            let suffix = crate::cache::lowercase_hex(&random);
            let path = root.join(format!("{purpose}-{}-{suffix}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
        bail!("failed to allocate toolchain chunk staging")
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = make_tree_writable(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn make_tree_writable(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        make_writable(path, true)?;
        for entry in
            fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
        {
            make_tree_writable(&entry?.path())?;
        }
    } else if metadata.is_file() && cfg!(not(unix)) {
        make_writable(path, false)?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_writable(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = if directory { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to make {} writable", path.display()))
}

#[cfg(not(unix))]
fn make_writable(path: &Path, _directory: bool) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make {} writable", path.display()))
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path, _root: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)
        .with_context(|| format!("failed to create symlink {}", destination.display()))
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, root: &Path) -> Result<()> {
    let resolved = destination
        .parent()
        .context("symlink destination has no parent")?
        .join(target);
    if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
    .with_context(|| {
        format!(
            "failed to create symlink {} inside {}",
            destination.display(),
            root.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use crate::core::platform;
    use crate::core::signing::{ManifestSigner, generate_key_pair};
    use crate::toolchain::store::ToolchainStore;

    use super::{
        ToolchainChunkManifest, canonical_manifest_bytes, install, manifest_object_path, publish,
        signature_object_path,
    };

    fn write_toolchain(root: &Path, large: &[u8]) {
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        let lean = if cfg!(windows) {
            root.join("bin/lean.exe")
        } else {
            root.join("bin/lean")
        };
        fs::write(&lean, b"lean executable").unwrap();
        set_executable(&lean);
        fs::write(root.join("lib/common"), b"identical support file").unwrap();
        fs::write(root.join("lib/large"), large).unwrap();
    }

    fn deterministic_bytes(bytes: usize) -> Vec<u8> {
        let mut state = 0x7a31_4f29_d2c8_05b1_u64;
        let mut output = vec![0_u8; bytes];
        for byte in &mut output {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        output
    }

    fn keys(root: &Path) -> (PathBuf, PathBuf) {
        let private = root.join("signing.key");
        let public = root.join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();
        (private, public)
    }

    #[test]
    fn signed_chunk_round_trip_is_idempotent() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        write_toolchain(&source, b"small library");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        let remote = temp.path().join("remote");
        let (private, public) = keys(temp.path());
        let toolchain = "leanprover/lean4:v4.chunk-test";
        let platform = platform::host_id();

        let published = publish(
            &store,
            &source,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &private,
            false,
        )
        .unwrap();
        assert_eq!(published.files, 3);
        assert!(published.chunks_uploaded > 0);

        let installed = install(
            &store,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &public,
            None,
            false,
        )
        .unwrap();
        assert_eq!(installed.chunks_downloaded, installed.unique_chunks);
        assert_eq!(
            fs::read(installed.imported.view.join("lib/large")).unwrap(),
            b"small library"
        );
        store.verify().unwrap();

        let repeated = install(
            &store,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &public,
            None,
            false,
        )
        .unwrap();
        assert_eq!(repeated.chunks_downloaded, 0);
        assert_eq!(repeated.complete_files_reused, 3);
    }

    #[test]
    fn reuses_fastcdc_ranges_from_an_older_toolchain() {
        let temp = tempdir().unwrap();
        let mut first_bytes = deterministic_bytes(10 * 1024 * 1024);
        let first = temp.path().join("first");
        write_toolchain(&first, &first_bytes);
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        let remote = temp.path().join("remote");
        let (private, public) = keys(temp.path());
        let platform = platform::host_id();

        publish(
            &store,
            &first,
            "leanprover/lean4:v4.chunk-one",
            &platform,
            remote.to_str().unwrap(),
            &private,
            false,
        )
        .unwrap();
        install(
            &store,
            "leanprover/lean4:v4.chunk-one",
            &platform,
            remote.to_str().unwrap(),
            &public,
            None,
            false,
        )
        .unwrap();

        // Change a bounded region without changing file length. FastCDC cut
        // points outside the affected neighborhood remain stable.
        for byte in &mut first_bytes[4_500_000..4_600_000] {
            *byte ^= 0x5a;
        }
        let second = temp.path().join("second");
        write_toolchain(&second, &first_bytes);
        let second_publish = publish(
            &store,
            &second,
            "leanprover/lean4:v4.chunk-two",
            &platform,
            remote.to_str().unwrap(),
            &private,
            false,
        )
        .unwrap();
        assert!(second_publish.chunks_reused > 0);

        let second_install = install(
            &store,
            "leanprover/lean4:v4.chunk-two",
            &platform,
            remote.to_str().unwrap(),
            &public,
            None,
            false,
        )
        .unwrap();
        assert!(second_install.complete_files_reused >= 2);
        assert!(second_install.local_chunks_reused > 0);
        assert!(second_install.chunks_downloaded < second_install.unique_chunks);
        assert_eq!(
            fs::read(second_install.imported.view.join("lib/large")).unwrap(),
            first_bytes
        );
        store.verify().unwrap();
    }

    #[test]
    fn rejects_signature_tampering_and_signed_path_traversal() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        write_toolchain(&source, b"library");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        let remote = temp.path().join("remote");
        let (private, public) = keys(temp.path());
        let toolchain = "leanprover/lean4:v4.chunk-test";
        let platform = platform::host_id();
        publish(
            &store,
            &source,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &private,
            false,
        )
        .unwrap();

        let manifest_path = remote.join(manifest_object_path(toolchain, &platform));
        let original = fs::read(&manifest_path).unwrap();
        fs::write(&manifest_path, b"{}\n").unwrap();
        let error = install(
            &store,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &public,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("signature is invalid"), "{error}");

        let mut manifest: ToolchainChunkManifest = serde_json::from_slice(&original).unwrap();
        manifest.entries[0] = match manifest.entries[0].clone() {
            super::LayoutEntry::Directory { .. } => super::LayoutEntry::Directory {
                path: "../escape".to_owned(),
            },
            entry => entry,
        };
        manifest.entries.sort_by(|left, right| {
            left.path()
                .cmp(right.path())
                .then_with(|| left.kind_order().cmp(&right.kind_order()))
        });
        let malicious = canonical_manifest_bytes(&manifest).unwrap();
        let signature = ManifestSigner::load(&private).unwrap().sign(&malicious);
        fs::write(&manifest_path, malicious).unwrap();
        fs::write(
            remote.join(signature_object_path(toolchain, &platform)),
            signature,
        )
        .unwrap();
        let error = install(
            &store,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &public,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unsafe toolchain manifest path"), "{error}");
        assert!(!temp.path().join("escape").exists());
    }

    #[test]
    fn corrupted_chunk_never_publishes_a_store_view() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        write_toolchain(&source, b"library");
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        let remote = temp.path().join("remote");
        let (private, public) = keys(temp.path());
        let toolchain = "leanprover/lean4:v4.chunk-corrupt";
        let platform = platform::host_id();
        publish(
            &store,
            &source,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &private,
            false,
        )
        .unwrap();

        let chunk_root = remote.join("toolchains-v1/chunks/sha256");
        let prefix = fs::read_dir(chunk_root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let chunk = fs::read_dir(prefix)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let length = fs::metadata(&chunk).unwrap().len() as usize;
        fs::write(&chunk, vec![0_u8; length]).unwrap();

        let error = install(
            &store,
            toolchain,
            &platform,
            remote.to_str().unwrap(),
            &public,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("compressed toolchain chunk digest or size mismatch"),
            "{error}"
        );
        assert!(
            fs::read_dir(store.root.join("views"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            fs::read_dir(store.root.join("manifests"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[cfg(unix)]
    fn set_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_executable(_path: &Path) {}
}
