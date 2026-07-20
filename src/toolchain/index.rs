//! Signed catalog for a static toolchain chunk host.
//!
//! The index lists available toolchains and platforms and uses the same
//! Ed25519 trust key as the chunk manifests:
//!
//! ```text
//! toolchains-v1/
//! |-- index.json
//! |-- index.sig
//! `-- manifests/...
//! ```
//!
//! A static HTTPS server can serve the resulting directory; no lev-specific
//! service is required.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::digest;
use crate::core::atomic_file::rename_replace;
use crate::core::object_transport::ObjectTransport;
use crate::core::signing::{ManifestSigner, ManifestVerifier};
use crate::toolchain;
use crate::toolchain::chunks::{PublishedManifestSummary, inspect_signed_manifest};

pub const INDEX_SCHEMA: &str = "lev.toolchain-index/v1";
const INDEX_OBJECT: &str = "toolchains-v1/index.json";
const SIGNATURE_OBJECT: &str = "toolchains-v1/index.sig";
const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
const MAX_INDEX_ENTRIES: usize = 100_000;

/// One authenticated toolchain/platform available from the same remote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct ToolchainIndexEntry {
    pub toolchain: String,
    pub platform: String,
    pub manifest: String,
    pub manifest_sha256: String,
    pub logical_bytes: u64,
    pub entries: u64,
    pub files: u64,
    pub unique_chunks: u64,
}

impl From<PublishedManifestSummary> for ToolchainIndexEntry {
    fn from(summary: PublishedManifestSummary) -> Self {
        Self {
            toolchain: summary.toolchain,
            platform: summary.platform,
            manifest: summary.manifest,
            manifest_sha256: summary.manifest_sha256,
            logical_bytes: summary.logical_bytes,
            entries: summary.entries,
            files: summary.files,
            unique_chunks: summary.unique_chunks,
        }
    }
}

/// Canonical signed catalog served at `toolchains-v1/index.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolchainIndex {
    pub schema: String,
    pub signing_key_fingerprint: String,
    pub entries: Vec<ToolchainIndexEntry>,
}

/// Result of rebuilding a local static publication tree's catalog.
#[derive(Debug, Serialize)]
pub struct BuildReport {
    pub schema: &'static str,
    pub index: String,
    pub signature: String,
    pub signing_key_fingerprint: String,
    pub entries: u64,
    pub toolchains: u64,
    pub platforms: u64,
    pub index_sha256: String,
}

/// Verify all published manifests and rebuild the local signed index.
pub fn build(root: &Path, signing_key: &Path, public_key: &Path) -> Result<BuildReport> {
    let root = absolute(root)?;
    let signer = ManifestSigner::load(signing_key)?;
    let verifier = ManifestVerifier::load(public_key)?;
    if signer.fingerprint() != verifier.fingerprint() {
        bail!("index signing key does not match the manifest public key");
    }

    let mut entries = scan_manifests(&root, &verifier)?;
    entries.sort();
    if entries
        .windows(2)
        .any(|pair| pair[0].toolchain == pair[1].toolchain && pair[0].platform == pair[1].platform)
    {
        bail!("toolchain publication contains duplicate toolchain/platform identities");
    }
    let index = ToolchainIndex {
        schema: INDEX_SCHEMA.to_owned(),
        signing_key_fingerprint: verifier.fingerprint(),
        entries,
    };
    validate(&index, &verifier)?;
    let bytes = canonical_bytes(&index)?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        bail!("toolchain index exceeds the 32 MiB size limit");
    }
    let signature = signer.sign(&bytes);
    let index_path = root.join(INDEX_OBJECT);
    let signature_path = root.join(SIGNATURE_OBJECT);
    atomic_write(&index_path, &bytes)?;
    atomic_write(&signature_path, signature.as_bytes())?;

    let toolchains = index
        .entries
        .iter()
        .map(|entry| entry.toolchain.as_str())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let platforms = index
        .entries
        .iter()
        .map(|entry| entry.platform.as_str())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    Ok(BuildReport {
        schema: "lev.toolchain-index-build/v1",
        index: index_path.display().to_string(),
        signature: signature_path.display().to_string(),
        signing_key_fingerprint: verifier.fingerprint(),
        entries: index.entries.len() as u64,
        toolchains,
        platforms,
        index_sha256: format!("{:x}", Sha256::digest(&bytes)),
    })
}

/// Fetch, authenticate, parse, and validate a remote catalog.
pub fn load(remote: &str, public_key: &Path, allow_insecure_http: bool) -> Result<ToolchainIndex> {
    load_optional(remote, public_key, allow_insecure_http)?
        .context("toolchain remote does not publish toolchains-v1/index.json")
}

/// Load a catalog when present, preserving compatibility with older remotes.
pub fn load_optional(
    remote: &str,
    public_key: &Path,
    allow_insecure_http: bool,
) -> Result<Option<ToolchainIndex>> {
    let transport = ObjectTransport::parse(remote, allow_insecure_http)?;
    let staging = StagingDirectory::create()?;
    let index_path = staging.path.join("index.json");
    let signature_path = staging.path.join("index.sig");
    if !transport.fetch(INDEX_OBJECT, &index_path, MAX_INDEX_BYTES)? {
        return Ok(None);
    }
    if !transport.fetch(SIGNATURE_OBJECT, &signature_path, MAX_SIGNATURE_BYTES)? {
        bail!("toolchain index signature is missing");
    }
    let bytes = fs::read(&index_path)
        .with_context(|| format!("failed to read {}", index_path.display()))?;
    let signature = fs::read(&signature_path)
        .with_context(|| format!("failed to read {}", signature_path.display()))?;
    let verifier = ManifestVerifier::load(public_key)?;
    verifier.verify(&bytes, &signature)?;
    let index: ToolchainIndex =
        serde_json::from_slice(&bytes).context("failed to parse signed toolchain index")?;
    if canonical_bytes(&index)? != bytes {
        bail!("signed toolchain index is not in canonical lev JSON form");
    }
    validate(&index, &verifier)?;
    Ok(Some(index))
}

/// Return the indexed manifest digest, or `None` for an index-free remote.
///
/// The digest binds installation to the catalog entry.
pub fn require_if_indexed(
    remote: &str,
    public_key: &Path,
    allow_insecure_http: bool,
    toolchain: &str,
    platform: &str,
) -> Result<Option<String>> {
    let Some(index) = load_optional(remote, public_key, allow_insecure_http)? else {
        return Ok(None);
    };
    index
        .entries
        .iter()
        .find(|entry| entry.toolchain == toolchain && entry.platform == platform)
        .map(|entry| Some(entry.manifest_sha256.clone()))
        .with_context(|| {
            format!("signed toolchain index has no entry for {toolchain} on {platform}")
        })
}

fn scan_manifests(root: &Path, verifier: &ManifestVerifier) -> Result<Vec<ToolchainIndexEntry>> {
    let manifests_root = root.join("toolchains-v1/manifests");
    if !manifests_root.is_dir() {
        bail!(
            "{} does not contain published toolchain manifests",
            manifests_root.display()
        );
    }
    let mut directories = read_sorted(&manifests_root)?;
    let mut entries = Vec::new();
    for directory in directories.drain(..) {
        if !directory.file_type()?.is_dir() {
            continue;
        }
        let directory_name = utf8_name(&directory.path())?;
        if !crate::core::hex::is_sha256(&directory_name) {
            bail!(
                "unexpected directory in toolchain manifest root: {}",
                directory.path().display()
            );
        }
        for manifest in read_sorted(&directory.path())? {
            let path = manifest.path();
            if !manifest.file_type()?.is_file()
                || path.extension().and_then(|extension| extension.to_str()) != Some("json")
            {
                continue;
            }
            if entries.len() >= MAX_INDEX_ENTRIES {
                bail!("toolchain index contains more than {MAX_INDEX_ENTRIES} entries");
            }
            let metadata = fs::metadata(&path)?;
            if metadata.len() > MAX_MANIFEST_BYTES {
                bail!("toolchain manifest is too large: {}", path.display());
            }
            let signature_path = path.with_extension("sig");
            let signature_metadata = fs::metadata(&signature_path).with_context(|| {
                format!(
                    "published manifest {} has no detached signature",
                    path.display()
                )
            })?;
            if !signature_metadata.is_file() || signature_metadata.len() > MAX_SIGNATURE_BYTES {
                bail!("invalid toolchain signature {}", signature_path.display());
            }
            let file_name = utf8_name(&path)?;
            let object = format!("toolchains-v1/manifests/{directory_name}/{file_name}");
            let manifest_bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let signature = fs::read(&signature_path)
                .with_context(|| format!("failed to read {}", signature_path.display()))?;
            entries.push(
                inspect_signed_manifest(&object, &manifest_bytes, &signature, verifier)?.into(),
            );
        }
    }
    Ok(entries)
}

fn validate(index: &ToolchainIndex, verifier: &ManifestVerifier) -> Result<()> {
    if index.schema != INDEX_SCHEMA {
        bail!("unsupported toolchain index schema {:?}", index.schema);
    }
    if index.signing_key_fingerprint != verifier.fingerprint() {
        bail!("toolchain index signing-key fingerprint does not match the trust anchor");
    }
    if index.entries.len() > MAX_INDEX_ENTRIES {
        bail!("toolchain index contains too many entries");
    }
    if !index.entries.windows(2).all(|pair| pair[0] < pair[1]) {
        bail!("toolchain index entries are not canonically sorted");
    }
    let mut identities = BTreeSet::new();
    for entry in &index.entries {
        if toolchain::normalize(&entry.toolchain)? != entry.toolchain {
            bail!("toolchain index contains a noncanonical toolchain name");
        }
        validate_platform(&entry.platform)?;
        if !crate::core::hex::is_sha256(&entry.manifest_sha256)
            || entry.entries < entry.files
            || entry.files == 0
            || entry.logical_bytes == 0
        {
            bail!("toolchain index contains invalid entry accounting");
        }
        let expected = format!(
            "toolchains-v1/manifests/{}/{}.json",
            digest(entry.toolchain.as_bytes()),
            entry.platform
        );
        if entry.manifest != expected {
            bail!("toolchain index manifest path does not match its identity");
        }
        if !identities.insert((&entry.toolchain, &entry.platform)) {
            bail!("toolchain index contains a duplicate identity");
        }
    }
    Ok(())
}

fn canonical_bytes(index: &ToolchainIndex) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(index).context("failed to serialize toolchain index")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn validate_platform(platform: &str) -> Result<()> {
    if platform.is_empty()
        || platform.len() > 128
        || !platform
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("invalid toolchain index platform");
    }
    Ok(())
}

fn read_sorted(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", path.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn utf8_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .with_context(|| format!("publication path is not UTF-8: {}", path.display()))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()
            .context("failed to determine current directory")?
            .join(path))
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("index path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temporary = unique_sibling(path);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temporary.display()))?;
    drop(file);
    let result = rename_replace(&temporary, path)
        .with_context(|| format!("failed to publish {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn unique_sibling(path: &Path) -> PathBuf {
    let mut random = [0_u8; 16];
    OsRng.fill_bytes(&mut random);
    let suffix = crate::cache::lowercase_hex(&random);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.lev-index-{suffix}"))
}

struct StagingDirectory {
    path: PathBuf,
}

impl StagingDirectory {
    fn create() -> Result<Self> {
        let root = std::env::temp_dir();
        for _ in 0..32 {
            let path = unique_sibling(&root.join("lev-toolchain-index"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
        bail!("failed to allocate toolchain-index staging directory")
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

    use tempfile::tempdir;

    use super::{INDEX_SCHEMA, MAX_INDEX_ENTRIES, build, load, require_if_indexed};
    use crate::core::signing::generate_key_pair;
    use crate::toolchain::chunks as toolchain_chunks;
    use crate::toolchain::store::ToolchainStore;

    #[test]
    fn builds_loads_and_detects_tampered_static_indexes() {
        let temp = tempdir().unwrap();
        let private = temp.path().join("signing.key");
        let public = temp.path().join("signing.pub");
        generate_key_pair(&private, &public, false).unwrap();

        let source = temp.path().join("lean");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/lean"), b"lean executable").unwrap();
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        let remote = temp.path().join("cdn");
        toolchain_chunks::publish(
            &store,
            &source,
            "leanprover/lean4:v4.test",
            "linux-x86_64",
            remote.to_str().unwrap(),
            &private,
            false,
        )
        .unwrap();

        let report = build(&remote, &private, &public).unwrap();
        assert_eq!(report.entries, 1);
        let index = load(remote.to_str().unwrap(), &public, false).unwrap();
        assert_eq!(index.schema, INDEX_SCHEMA);
        assert_eq!(index.entries[0].toolchain, "leanprover/lean4:v4.test");

        // Both manifests are valid under the same key, but the catalog binds
        // one exact digest. This models a CDN exposing metadata from two
        // different atomic deployments at once.
        let indexed_digest = require_if_indexed(
            remote.to_str().unwrap(),
            &public,
            false,
            "leanprover/lean4:v4.test",
            "linux-x86_64",
        )
        .unwrap()
        .unwrap();
        fs::write(source.join("bin/lean"), b"different lean executable").unwrap();
        let mixed_remote = temp.path().join("mixed-cdn");
        toolchain_chunks::publish(
            &store,
            &source,
            "leanprover/lean4:v4.test",
            "linux-x86_64",
            mixed_remote.to_str().unwrap(),
            &private,
            false,
        )
        .unwrap();
        for name in ["index.json", "index.sig"] {
            fs::copy(
                remote.join("toolchains-v1").join(name),
                mixed_remote.join("toolchains-v1").join(name),
            )
            .unwrap();
        }
        let install_store = ToolchainStore {
            root: temp.path().join("install-store"),
        };
        let error = toolchain_chunks::install(
            &install_store,
            "leanprover/lean4:v4.test",
            "linux-x86_64",
            mixed_remote.to_str().unwrap(),
            &public,
            Some(&indexed_digest),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not match the authenticated index"));

        let index_path = remote.join("toolchains-v1/index.json");
        let mut bytes = fs::read(&index_path).unwrap();
        bytes[0] ^= 1;
        fs::write(index_path, bytes).unwrap();
        assert!(
            load(remote.to_str().unwrap(), &public, false)
                .unwrap_err()
                .to_string()
                .contains("signature")
        );
    }

    #[test]
    fn published_json_schema_tracks_the_signed_index_identifier() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../schemas/toolchain-index-v1.schema.json"))
                .unwrap();
        assert_eq!(schema["properties"]["schema"]["const"], INDEX_SCHEMA);
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["entries"]["maxItems"],
            MAX_INDEX_ENTRIES
        );
    }
}
