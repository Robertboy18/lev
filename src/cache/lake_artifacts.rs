//! Inspection and cleanup for Lake's native artifact cache.
//!
//! Both known on-disk layouts are detected from their structure:
//!
//! ```text
//! Legacy layout                Current layout
//! <lake-cache>/                <lake-cache>/
//! |-- artifacts/<decimal>.*    |-- artifacts/<hex>.*
//! `-- inputs/<package>.jsonl   `-- outputs/<package>/<input-hash>.json
//! ```
//!
//! Lake owns the hash and mapping semantics. lev accounts for files, verifies
//! structure, and collects old unreferenced artifacts. Bare legacy hashes may
//! match several extensions, so cleanup keeps every match.
//!
//! lev-managed builds take a shared lock for their toolchain cache. Garbage
//! collection takes exclusive locks. Direct `lake` commands do not use these
//! locks and should not run alongside lev cache GC.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;
use serde_json::Value;

use crate::cache::{CacheLayout, path_bytes};
use crate::core::clock::{modified_seconds, now_seconds};

#[derive(Debug, Default, Serialize)]
/// Aggregate storage and reachability statistics across selected toolchains.
pub struct ArtifactCacheStats {
    /// Number of toolchain-specific Lake cache roots inspected.
    pub toolchain_caches: u64,
    /// Number of regular files in the artifact stores.
    pub artifacts: u64,
    /// Logical size of all artifact files.
    pub artifact_bytes: u64,
    /// Number of artifacts that have at least one additional hard link.
    pub hardlinked_artifacts: u64,
    /// Logical size represented by hard-linked artifacts.
    pub hardlinked_bytes: u64,
    /// Number of input-to-output JSON mappings parsed.
    pub mappings: u64,
    /// Number of distinct descriptor names referenced by each cache root.
    pub referenced_artifacts: u64,
    /// Number of artifact files not referenced by any mapping.
    pub unreferenced_artifacts: u64,
    /// Logical size of unreferenced artifact files.
    pub unreferenced_bytes: u64,
    /// Number of locally-authored references whose artifact file is absent.
    pub missing_local_artifacts: u64,
}

#[derive(Debug, Serialize)]
/// One broken local input-to-output mapping.
pub struct MissingArtifact {
    /// Mapping that contains the broken reference.
    pub mapping: PathBuf,
    /// Lake artifact descriptor, for example `0123456789abcdef.olean`.
    pub artifact: String,
}

#[derive(Debug, Default, Serialize)]
/// Complete result of a structural artifact-cache inspection.
pub struct ArtifactCacheReport {
    /// Aggregate counts and sizes.
    pub stats: ArtifactCacheStats,
    /// Locally-authored references that could not be resolved.
    pub missing: Vec<MissingArtifact>,
}

#[derive(Debug, Serialize)]
/// One unreferenced artifact eligible for deletion.
pub struct ArtifactGcCandidate {
    /// Absolute path inside a toolchain's `artifacts` directory.
    pub path: PathBuf,
    /// Logical file size used for dry-run accounting.
    pub bytes: u64,
}

#[derive(Debug, Default, Serialize)]
/// Deterministic deletion plan produced by an artifact-cache scan.
pub struct ArtifactGcPlan {
    /// Candidates sorted by absolute path.
    pub candidates: Vec<ArtifactGcCandidate>,
}

impl ArtifactGcPlan {
    /// Total logical bytes represented by the candidates.
    pub fn bytes(&self) -> u64 {
        self.candidates.iter().map(|entry| entry.bytes).sum()
    }

    fn apply(&self) -> Result<()> {
        for candidate in &self.candidates {
            fs::remove_file(&candidate.path)
                .with_context(|| format!("failed to remove {}", candidate.path.display()))?;
        }
        Ok(())
    }
}

/// Advisory lock for one toolchain's Lake cache.
#[must_use = "dropping the guard releases the artifact-cache lock"]
pub struct ArtifactCacheLock(File);

/// The two file classes that can be transferred through a remote cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportEntryKind {
    /// A content-addressed file from Lake's `artifacts` directory.
    Artifact,
    /// An input-to-output record from Lake's `outputs` directory.
    Mapping,
}

/// One file in a stable, locked export snapshot.
#[derive(Debug)]
pub struct ExportEntry {
    /// File class used to preserve publication ordering on import.
    pub kind: ExportEntryKind,
    /// Path below the selected toolchain's Lake cache root.
    pub relative_path: PathBuf,
    /// Absolute source path used while preparing remote blobs.
    pub source_path: PathBuf,
}

/// A locked export containing mappings and the artifacts they reference.
#[must_use = "the snapshot lock must remain held while its files are read"]
pub struct LockedArtifactExport {
    /// Mapping and artifact files sorted by relative path.
    pub entries: Vec<ExportEntry>,
    _lock: ArtifactCacheLock,
}

/// A GC plan that keeps every cache lock until deletion finishes.
#[must_use = "the transaction holds exclusive locks until it is dropped"]
pub struct LockedArtifactGc {
    /// Structural state observed while the exclusive locks were held.
    pub report: ArtifactCacheReport,
    /// Age-qualified orphan deletion plan.
    pub plan: ArtifactGcPlan,
    locks: Vec<ArtifactCacheLock>,
}

/// Exact names and bare hashes found in a legacy Lake record.
#[derive(Debug, Default)]
struct LegacyReferences {
    exact_names: BTreeSet<String>,
    bare_hashes: BTreeSet<String>,
}

impl LockedArtifactGc {
    /// Delete every candidate while the exclusive cache locks remain held.
    pub fn apply(&self) -> Result<()> {
        // Keep the guards visibly borrowed until deletion finishes.
        let _held_locks = &self.locks;
        self.plan.apply()
    }
}

/// Take a shared lock used by lev-managed commands that may read or write Lake
/// artifacts for `toolchain`.
pub fn lock_shared(cache: &CacheLayout, toolchain: &str) -> Result<ArtifactCacheLock> {
    lock(cache, cache.lake_lock_path(toolchain), false)
}

/// Lock one toolchain cache for an atomic publication.
pub fn lock_exclusive(cache: &CacheLayout, toolchain: &str) -> Result<ArtifactCacheLock> {
    lock(cache, cache.lake_lock_path(toolchain), true)
}

/// Export mappings and referenced artifacts while holding a shared lock.
///
/// Every referenced artifact must exist locally.
pub fn export(cache: &CacheLayout, toolchain: &str) -> Result<LockedArtifactExport> {
    let root = cache.lake_dir(toolchain);
    if !root.is_dir() {
        bail!(
            "no Lake artifact cache exists for toolchain {toolchain:?} at {}",
            root.display()
        );
    }
    let cache_lock = lock_shared(cache, toolchain)?;
    if root.join("inputs").is_dir() {
        // Legacy mappings need a local inventory to recover file extensions.
        bail!(
            "remote snapshots cannot export the non-self-describing legacy Lake inputs layout for toolchain {toolchain:?}"
        );
    }
    let mut entries = Vec::new();
    let mut referenced = BTreeSet::new();
    let outputs = root.join("outputs");
    if outputs.is_dir() {
        collect_export_mappings(&root, &outputs, &mut referenced, &mut entries)?;
    }

    let artifacts = root.join("artifacts");
    for artifact in referenced {
        let source_path = artifacts.join(&artifact);
        if !source_path.is_file()
            || fs::symlink_metadata(&source_path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!(
                "cannot export Lake mapping reference {artifact:?}: {} is not a regular artifact file",
                source_path.display()
            );
        }
        entries.push(ExportEntry {
            kind: ExportEntryKind::Artifact,
            relative_path: PathBuf::from("artifacts").join(&artifact),
            source_path,
        });
    }
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(LockedArtifactExport {
        entries,
        _lock: cache_lock,
    })
}

/// Inspect all Lake artifact caches, or only the cache for `toolchain`.
///
/// This validates path syntax and JSON structure in addition to gathering
/// counts. It does not recompute Lake's non-cryptographic content hash.
pub fn inspect(cache: &CacheLayout, toolchain: Option<&str>) -> Result<ArtifactCacheReport> {
    let mut report = ArtifactCacheReport::default();
    for root in selected_roots(cache, toolchain)? {
        let _lock = lock(cache, lock_path_for_root(cache, &root)?, false)?;
        inspect_root(&root, &mut report)?;
    }
    report.stats.missing_local_artifacts = report.missing.len() as u64;
    Ok(report)
}

/// Build an age-gated orphan plan while retaining exclusive cache locks.
///
/// The returned transaction must remain alive until the caller has either
/// applied or discarded the plan. Toolchain roots are locked in lexical order
/// to make concurrent multi-cache collectors deadlock-free.
pub fn gc_plan(
    cache: &CacheLayout,
    toolchain: Option<&str>,
    max_age_days: u64,
) -> Result<LockedArtifactGc> {
    let cutoff = now_seconds().saturating_sub(max_age_days.saturating_mul(86_400));
    let roots = selected_roots(cache, toolchain)?;
    let mut locks = Vec::with_capacity(roots.len());
    for root in &roots {
        locks.push(lock(cache, lock_path_for_root(cache, root)?, true)?);
    }

    let mut report = ArtifactCacheReport::default();
    let mut plan = ArtifactGcPlan::default();
    for root in roots {
        let referenced = inspect_root(&root, &mut report)?;
        let artifact_root = root.join("artifacts");
        if !artifact_root.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&artifact_root)
            .with_context(|| format!("failed to read {}", artifact_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", artifact_root.display()))?;
            if !entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?
                .is_file()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            validate_artifact_name(&name)?;
            if referenced.contains(&name) || modified_seconds(&entry.path()) > cutoff {
                continue;
            }
            plan.candidates.push(ArtifactGcCandidate {
                bytes: entry
                    .metadata()
                    .with_context(|| format!("failed to inspect {}", entry.path().display()))?
                    .len(),
                path: entry.path(),
            });
        }
    }
    report.stats.missing_local_artifacts = report.missing.len() as u64;
    plan.candidates
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(LockedArtifactGc {
        report,
        plan,
        locks,
    })
}

fn inspect_root(root: &Path, report: &mut ArtifactCacheReport) -> Result<BTreeSet<String>> {
    report.stats.toolchain_caches += 1;
    let artifacts = root.join("artifacts");
    let mut present = BTreeSet::new();
    if artifacts.is_dir() {
        for entry in fs::read_dir(&artifacts)
            .with_context(|| format!("failed to read {}", artifacts.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", artifacts.display()))?;
            if !entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?
                .is_file()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            validate_artifact_name(&name)?;
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            let bytes = metadata.len();
            report.stats.artifacts += 1;
            report.stats.artifact_bytes = report
                .stats
                .artifact_bytes
                .checked_add(bytes)
                .context("Lake artifact cache size overflow")?;
            if hard_link_count(&metadata) > 1 {
                report.stats.hardlinked_artifacts += 1;
                report.stats.hardlinked_bytes = report
                    .stats
                    .hardlinked_bytes
                    .checked_add(bytes)
                    .context("Lake hard-linked artifact size overflow")?;
            }
            present.insert(name);
        }
    }

    let mut referenced = BTreeSet::new();
    let outputs = root.join("outputs");
    if outputs.is_dir() {
        collect_mappings(&outputs, &present, &mut referenced, report)?;
    }
    let inputs = root.join("inputs");
    if inputs.is_dir() {
        collect_legacy_mappings(&inputs, &present, &mut referenced, report)?;
    }

    report.stats.referenced_artifacts += referenced.len() as u64;
    for name in present.difference(&referenced) {
        report.stats.unreferenced_artifacts += 1;
        report.stats.unreferenced_bytes = report
            .stats
            .unreferenced_bytes
            .checked_add(path_bytes(&artifacts.join(name))?)
            .context("Lake unreferenced artifact size overflow")?;
    }
    Ok(referenced)
}

fn collect_mappings(
    directory: &Path,
    present: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
    report: &mut ArtifactCacheReport,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_mappings(&entry.path(), present, referenced, report)?;
            continue;
        }
        if !file_type.is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let bytes = fs::read(entry.path())
            .with_context(|| format!("failed to read {}", entry.path().display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", entry.path().display()))?;
        let (remote, data) = unwrap_output(&value)?;
        let mut mapping_references = BTreeSet::new();
        collect_descriptors(data, &mut mapping_references)?;
        if !remote {
            // A local mapping is written only after Lake has published its
            // output artifacts. A missing file therefore means interrupted
            // mutation or external cache damage. Remote mappings are lazy:
            // Lake is allowed to download their artifacts on first use.
            for artifact in &mapping_references {
                if !present.contains(artifact) {
                    report.missing.push(MissingArtifact {
                        mapping: entry.path(),
                        artifact: artifact.clone(),
                    });
                }
            }
        }
        referenced.extend(mapping_references);
        report.stats.mappings += 1;
    }
    Ok(())
}

/// Parse legacy `[inputHash, outputData]` JSON Lines files.
///
/// GC rejects malformed lines instead of calculating reachability from a
/// partial file.
fn collect_legacy_mappings(
    directory: &Path,
    present: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
    report: &mut ArtifactCacheReport,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    let artifact_index = index_artifacts_by_hash(present);
    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if !file_type.is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            continue;
        }

        let file = File::open(entry.path())
            .with_context(|| format!("failed to open {}", entry.path().display()))?;
        for (line_index, line) in BufReader::new(file).lines().enumerate() {
            let line_number = line_index + 1;
            let line = line.with_context(|| {
                format!(
                    "failed to read {} line {line_number}",
                    entry.path().display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line).with_context(|| {
                format!(
                    "failed to parse {} line {line_number}",
                    entry.path().display()
                )
            })?;
            let data = legacy_mapping_data(&value).with_context(|| {
                format!(
                    "invalid legacy Lake mapping {} line {line_number}",
                    entry.path().display()
                )
            })?;
            let mut mapping_references = LegacyReferences::default();
            collect_legacy_descriptors(data, &mut mapping_references).with_context(|| {
                format!(
                    "invalid legacy Lake outputs in {} line {line_number}",
                    entry.path().display()
                )
            })?;
            resolve_legacy_references(
                &entry.path(),
                &mapping_references,
                present,
                &artifact_index,
                referenced,
                report,
            );
            report.stats.mappings += 1;
        }
    }
    Ok(())
}

/// Build a stable lookup for legacy mappings that identify content but omit
/// the extension selected by the typed Lake build rule.
fn index_artifacts_by_hash(present: &BTreeSet<String>) -> BTreeMap<String, BTreeSet<String>> {
    let mut index = BTreeMap::<String, BTreeSet<String>>::new();
    for name in present {
        let hash = name.split_once('.').map_or(name.as_str(), |(hash, _)| hash);
        index
            .entry(hash.to_owned())
            .or_default()
            .insert(name.clone());
    }
    index
}

fn resolve_legacy_references(
    mapping: &Path,
    references: &LegacyReferences,
    present: &BTreeSet<String>,
    artifact_index: &BTreeMap<String, BTreeSet<String>>,
    referenced: &mut BTreeSet<String>,
    report: &mut ArtifactCacheReport,
) {
    let mut missing = BTreeSet::new();
    for name in &references.exact_names {
        referenced.insert(name.clone());
        if !present.contains(name) {
            missing.insert(name.clone());
        }
    }
    for hash in &references.bare_hashes {
        if let Some(names) = artifact_index.get(hash) {
            referenced.extend(names.iter().cloned());
        } else {
            // A generic legacy descriptor does not tell us which extension is
            // missing. Reporting the canonical hash is more truthful than
            // inventing a `.art` path that Lake may never have used.
            missing.insert(hash.clone());
        }
    }
    report
        .missing
        .extend(missing.into_iter().map(|artifact| MissingArtifact {
            mapping: mapping.to_owned(),
            artifact,
        }));
}

fn collect_export_mappings(
    root: &Path,
    directory: &Path,
    referenced: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
) -> Result<()> {
    let mut children = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", directory.display()))?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let file_type = child
            .file_type()
            .with_context(|| format!("failed to inspect {}", child.path().display()))?;
        if file_type.is_dir() {
            collect_export_mappings(root, &child.path(), referenced, entries)?;
            continue;
        }
        if !file_type.is_file()
            || child.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }

        let bytes = fs::read(child.path())
            .with_context(|| format!("failed to read {}", child.path().display()))?;
        referenced.extend(mapping_artifacts(&bytes).with_context(|| {
            format!(
                "failed to validate Lake output mapping {}",
                child.path().display()
            )
        })?);
        let relative_path = child
            .path()
            .strip_prefix(root)
            .with_context(|| {
                format!(
                    "{} is outside Lake cache root {}",
                    child.path().display(),
                    root.display()
                )
            })?
            .to_owned();
        entries.push(ExportEntry {
            kind: ExportEntryKind::Mapping,
            relative_path,
            source_path: child.path(),
        });
    }
    Ok(())
}

/// Parse one Lake output mapping into normalized artifact names.
pub(crate) fn mapping_artifacts(bytes: &[u8]) -> Result<BTreeSet<String>> {
    let value: Value =
        serde_json::from_slice(bytes).context("failed to parse Lake mapping JSON")?;
    let (_, data) = unwrap_output(&value)?;
    let mut referenced = BTreeSet::new();
    collect_descriptors(data, &mut referenced)?;
    Ok(referenced)
}

fn unwrap_output(value: &Value) -> Result<(bool, &Value)> {
    let Some(object) = value.as_object() else {
        return Ok((false, value));
    };
    if !object.contains_key("schemaVersion") {
        return Ok((false, value));
    }
    let data = object
        .get("data")
        .context("Lake cache output has schemaVersion but no data")?;
    let remote = object
        .get("service")
        .is_some_and(|service| !service.is_null());
    Ok((remote, data))
}

fn legacy_mapping_data(value: &Value) -> Result<&Value> {
    let Value::Array(fields) = value else {
        bail!("expected a two-element JSON array");
    };
    if fields.len() != 2 {
        bail!(
            "expected a two-element JSON array, found {} elements",
            fields.len()
        );
    }
    // Reject records Lake would not recognize before using their output data.
    legacy_hash(&fields[0]).context("invalid input hash")?;
    Ok(&fields[1])
}

fn collect_legacy_descriptors(value: &Value, output: &mut LegacyReferences) -> Result<()> {
    match value {
        Value::Null | Value::Bool(_) => {}
        Value::Number(_) | Value::String(_) => {
            output.bare_hashes.insert(legacy_hash(value)?);
        }
        Value::Array(values) => {
            for value in values {
                collect_legacy_descriptors(value, output)?;
            }
        }
        Value::Object(values)
            if values.contains_key("o") && values.contains_key("i") && values.contains_key("c") =>
        {
            collect_legacy_module_descriptors(values, output)?;
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_legacy_descriptors(value, output)?;
            }
        }
    }
    Ok(())
}

/// Reconstruct the extensions encoded by Lake's historical
/// `ModuleOutputHashes` JSON object.
fn collect_legacy_module_descriptors(
    values: &serde_json::Map<String, Value>,
    output: &mut LegacyReferences,
) -> Result<()> {
    let oleans = values
        .get("o")
        .context("module outputs have no 'o' field")?;
    let Value::Array(oleans) = oleans else {
        bail!("module output field 'o' must be an array");
    };
    if oleans.is_empty() {
        bail!("module output field 'o' must contain an OLean hash");
    }
    for (index, value) in oleans.iter().enumerate() {
        match index {
            0 => insert_legacy_exact(value, "olean", output)?,
            1 => insert_legacy_exact(value, "olean.server", output)?,
            2 => insert_legacy_exact(value, "olean.private", output)?,
            // The legacy reader uses only the first three entries. Retaining
            // all matching files for a future extra entry is safer than
            // guessing an extension that was not represented in this format.
            _ => {
                output.bare_hashes.insert(legacy_hash(value)?);
            }
        }
    }
    insert_legacy_exact(
        values
            .get("i")
            .context("module outputs have no 'i' field")?,
        "ilean",
        output,
    )?;
    insert_legacy_exact(
        values
            .get("c")
            .context("module outputs have no 'c' field")?,
        "c",
        output,
    )?;
    for (key, extension) in [("r", "ir"), ("b", "bc")] {
        if let Some(value) = values.get(key) {
            if !value.is_null() {
                insert_legacy_exact(value, extension, output)?;
            }
        }
    }

    // Preserve forward compatibility with metadata or output fields added to
    // the legacy object without weakening validation of the known fields.
    for (key, value) in values {
        if !matches!(key.as_str(), "o" | "i" | "r" | "c" | "b") {
            collect_legacy_descriptors(value, output)?;
        }
    }
    Ok(())
}

fn insert_legacy_exact(
    value: &Value,
    extension: &str,
    output: &mut LegacyReferences,
) -> Result<()> {
    let name = format!("{}.{}", legacy_hash(value)?, extension);
    // Use the same filename checks as modern mappings and remote imports.
    validate_artifact_name(&name)?;
    output.exact_names.insert(name);
    Ok(())
}

/// Normalize the JSON representation used by Lake's old `UInt64` hashes.
///
/// Lean serializes large `UInt64` values as JSON strings, but its decoder also
/// accepts JSON numbers. Canonical decimal form avoids aliases such as `001`
/// referring to the same artifact and guarantees that the resulting text is a
/// valid legacy file-name stem.
fn legacy_hash(value: &Value) -> Result<String> {
    let (hash, source) = match value {
        Value::Number(number) => (
            number
                .as_u64()
                .context("legacy Lake hash must be a nonnegative 64-bit integer")?,
            None,
        ),
        Value::String(value) => {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                bail!("legacy Lake hash must contain only decimal digits");
            }
            (
                value
                    .parse::<u64>()
                    .context("legacy Lake hash exceeds 64 bits")?,
                Some(value.as_str()),
            )
        }
        _ => bail!("legacy Lake hash must be a JSON string or number"),
    };
    let canonical = hash.to_string();
    if source.is_some_and(|source| source != canonical) {
        bail!("legacy Lake hash is not in canonical decimal form");
    }
    Ok(canonical)
}

fn collect_descriptors(value: &Value, output: &mut BTreeSet<String>) -> Result<()> {
    match value {
        Value::Null | Value::Bool(_) => {}
        Value::Number(number) => {
            let hash = number
                .as_u64()
                .context("Lake artifact hash must be a nonnegative 64-bit integer")?;
            // Modern Lake accepts a numeric compatibility representation but
            // names its artifacts with the hash's 16-digit hexadecimal form.
            // Legacy decimal mappings are parsed separately above.
            output.insert(format!("{hash:016x}.art"));
        }
        Value::String(value) => {
            validate_artifact_name(value)?;
            output.insert(value.clone());
        }
        Value::Array(values) => {
            for value in values {
                collect_descriptors(value, output)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_descriptors(value, output)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_artifact_name(name: &str) -> Result<()> {
    // Artifact descriptions come from executable project configuration.
    // Restrict them to one normal path component before joining them beneath
    // the cache root, even though ordinary Lake-generated names are benign.
    let path = Path::new(name);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        bail!("unsafe Lake artifact path {name:?}");
    }
    let hash = name.split_once('.').map_or(name, |(hash, _)| hash);
    let modern_hex = crate::core::hex::is_lower_hex(hash, 16);
    let legacy_decimal = !hash.is_empty()
        && hash.bytes().all(|byte| byte.is_ascii_digit())
        && hash
            .parse::<u64>()
            .is_ok_and(|value| value.to_string() == hash);
    if !modern_hex && !legacy_decimal {
        bail!("invalid Lake artifact name {name:?}");
    }
    Ok(())
}

fn selected_roots(cache: &CacheLayout, toolchain: Option<&str>) -> Result<Vec<PathBuf>> {
    if let Some(toolchain) = toolchain {
        let root = cache.lake_dir(toolchain);
        return Ok(root.is_dir().then_some(root).into_iter().collect());
    }
    let lake_root = cache.lake_root();
    if !lake_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut roots = Vec::new();
    for entry in fs::read_dir(&lake_root)
        .with_context(|| format!("failed to read {}", lake_root.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", lake_root.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            roots.push(entry.path());
        }
    }
    roots.sort();
    Ok(roots)
}

fn lock(cache: &CacheLayout, path: PathBuf, exclusive: bool) -> Result<ArtifactCacheLock> {
    cache.ensure()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    if exclusive {
        FileExt::lock_exclusive(&file)
    } else {
        FileExt::lock_shared(&file)
    }
    .with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(ArtifactCacheLock(file))
}

fn lock_path_for_root(cache: &CacheLayout, root: &Path) -> Result<PathBuf> {
    let key = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .context("Lake cache root has no key")?;
    Ok(cache.lock_root().join(format!("lake-{key}.lock")))
}

#[cfg(unix)]
fn hard_link_count(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(not(unix))]
fn hard_link_count(_metadata: &fs::Metadata) -> u64 {
    1
}

impl Drop for ArtifactCacheLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::OpenOptions;
    use std::time::Duration;

    use fs2::FileExt;
    use tempfile::tempdir;

    use crate::cache::CacheLayout;

    use super::{export, gc_plan, inspect, lock_shared, mapping_artifacts, validate_artifact_name};

    #[test]
    fn accepts_versioned_artifact_names_and_rejects_ambiguous_stems() {
        for valid in [
            "0123456789abcdef.olean",
            "0000000000000000",
            "0.art",
            "14550171264454906567.olean",
            "18446744073709551615.olean.server",
        ] {
            validate_artifact_name(valid).unwrap_or_else(|error| {
                panic!("expected {valid:?} to be valid: {error}");
            });
        }

        for invalid in [
            "",
            ".olean",
            "01.art",
            "00000000000000001.art",
            "18446744073709551616.art",
            "0123456789abcdeF.olean",
            "0123456789abcdeg.olean",
            "../0123456789abcdef.olean",
            "0123456789abcdef/sub.olean",
        ] {
            assert!(
                validate_artifact_name(invalid).is_err(),
                "expected {invalid:?} to be rejected"
            );
        }
    }

    #[test]
    fn normalizes_modern_numeric_descriptors_to_hex_artifacts() {
        let referenced =
            mapping_artifacts(br#"{"schemaVersion":"2026-02-25","service":null,"data":42}"#)
                .unwrap();
        assert_eq!(
            referenced,
            ["000000000000002a.art".to_owned()].into_iter().collect()
        );
    }

    #[test]
    fn legacy_json_lines_preserve_module_extensions_and_generic_hashes() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = cache.lake_dir("test/toolchain:legacy-layout");
        fs::create_dir_all(root.join("artifacts")).unwrap();
        fs::create_dir_all(root.join("inputs")).unwrap();
        for name in [
            "14550171264454906567.olean",
            "3453551376365123793.ilean",
            "940372859017955812.c",
            "42.a",
            "42.o",
            "999.art",
        ] {
            fs::write(root.join("artifacts").join(name), name).unwrap();
        }
        fs::write(
            root.join("inputs/root.jsonl"),
            concat!(
                "[\"6147500214486957215\",",
                "{\"o\":[\"14550171264454906567\"],",
                "\"i\":\"3453551376365123793\",",
                "\"c\":\"940372859017955812\"}]\n",
                "[7,42]\n"
            ),
        )
        .unwrap();

        let report = inspect(&cache, None).unwrap();
        assert_eq!(report.stats.artifacts, 6);
        assert_eq!(report.stats.mappings, 2);
        assert_eq!(report.stats.referenced_artifacts, 5);
        assert_eq!(report.stats.unreferenced_artifacts, 1);
        assert!(report.missing.is_empty());

        let transaction = gc_plan(&cache, None, 0).unwrap();
        assert_eq!(transaction.plan.candidates.len(), 1);
        assert!(transaction.plan.candidates[0].path.ends_with("999.art"));
        transaction.apply().unwrap();
        for name in [
            "14550171264454906567.olean",
            "3453551376365123793.ilean",
            "940372859017955812.c",
            "42.a",
            "42.o",
        ] {
            assert!(root.join("artifacts").join(name).is_file(), "{name}");
        }
    }

    #[test]
    fn legacy_json_lines_report_exact_and_extensionless_missing_outputs() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = cache.lake_dir("test/toolchain:legacy-layout");
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::write(
            root.join("inputs/root.jsonl"),
            concat!(
                "[\"1\",{\"o\":[\"2\"],\"i\":\"3\",\"c\":\"4\"}]\n",
                "[5,\"6\"]\n"
            ),
        )
        .unwrap();

        let report = inspect(&cache, None).unwrap();
        assert_eq!(
            report
                .missing
                .iter()
                .map(|entry| entry.artifact.as_str())
                .collect::<Vec<_>>(),
            ["2.olean", "3.ilean", "4.c", "6"]
        );
    }

    #[test]
    fn rejects_malformed_legacy_mapping_records() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = cache.lake_dir("test/toolchain:legacy-layout");
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::write(root.join("inputs/root.jsonl"), "[\"01\",42]\n").unwrap();

        let error = inspect(&cache, None).unwrap_err().to_string();
        assert!(error.contains("invalid legacy Lake mapping"), "{error}");
    }

    #[test]
    fn remote_export_rejects_legacy_extensionless_mappings() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = cache.lake_dir("test/toolchain:legacy-layout");
        fs::create_dir_all(root.join("inputs")).unwrap();

        let error = export(&cache, "test/toolchain:legacy-layout")
            .err()
            .expect("legacy export should fail")
            .to_string();
        assert!(error.contains("legacy Lake inputs layout"), "{error}");
    }

    #[test]
    fn verifies_references_and_collects_only_old_orphans() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = cache.lake_dir("leanprover/lean4:v4.test");
        fs::create_dir_all(root.join("artifacts")).unwrap();
        fs::create_dir_all(root.join("outputs/pkg")).unwrap();
        fs::write(root.join("artifacts/0123456789abcdef.olean"), "olean").unwrap();
        fs::write(root.join("artifacts/fedcba9876543210.ilean"), "orphan").unwrap();
        fs::write(
            root.join("outputs/pkg/1111111111111111.json"),
            r#"{
                "service": null,
                "schemaVersion": "2026-02-25",
                "data": {"o": ["0123456789abcdef.olean"], "m": false}
            }"#,
        )
        .unwrap();

        let report = inspect(&cache, None).unwrap();
        assert_eq!(report.stats.artifacts, 2);
        assert_eq!(report.stats.mappings, 1);
        assert_eq!(report.stats.unreferenced_artifacts, 1);
        assert!(report.missing.is_empty());

        std::thread::sleep(Duration::from_millis(10));
        let transaction = gc_plan(&cache, None, 0).unwrap();
        assert_eq!(transaction.plan.candidates.len(), 1);
        assert!(
            transaction.plan.candidates[0]
                .path
                .ends_with("fedcba9876543210.ilean")
        );
        transaction.apply().unwrap();
        assert!(!root.join("artifacts/fedcba9876543210.ilean").exists());
        assert!(root.join("artifacts/0123456789abcdef.olean").exists());
    }

    #[test]
    fn reports_missing_local_artifacts_but_allows_remote_references() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = cache.lake_dir("leanprover/lean4:v4.test");
        fs::create_dir_all(root.join("outputs/pkg")).unwrap();
        fs::write(
            root.join("outputs/pkg/local.json"),
            r#"{"schemaVersion":"2026-02-25","service":null,
                "data":"0123456789abcdef.olean"}"#,
        )
        .unwrap();
        fs::write(
            root.join("outputs/pkg/remote.json"),
            r#"{"schemaVersion":"2026-02-25","service":"remote",
                "scope":"pkg","data":"fedcba9876543210.olean"}"#,
        )
        .unwrap();

        let report = inspect(&cache, None).unwrap();
        assert_eq!(report.stats.missing_local_artifacts, 1);
        assert_eq!(report.missing[0].artifact, "0123456789abcdef.olean");
    }

    #[test]
    fn rejects_malformed_json_and_artifact_path_traversal() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let root = cache.lake_dir("leanprover/lean4:v4.test");
        fs::create_dir_all(root.join("outputs/pkg")).unwrap();
        fs::write(root.join("outputs/pkg/broken.json"), "{").unwrap();
        let error = inspect(&cache, None).unwrap_err().to_string();
        assert!(error.contains("failed to parse"), "{error}");

        fs::write(
            root.join("outputs/pkg/broken.json"),
            r#"{"schemaVersion":"2026-02-25","service":null,
                "data":"../0123456789abcdef.olean"}"#,
        )
        .unwrap();
        let error = inspect(&cache, None).unwrap_err().to_string();
        assert!(error.contains("unsafe Lake artifact path"), "{error}");
    }

    #[test]
    fn preserves_recent_orphans_and_filters_by_toolchain() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let first = cache.lake_dir("leanprover/lean4:v4.first");
        let second = cache.lake_dir("leanprover/lean4:v4.second");
        fs::create_dir_all(first.join("artifacts")).unwrap();
        fs::create_dir_all(second.join("artifacts")).unwrap();
        fs::write(first.join("artifacts/0123456789abcdef.olean"), "first").unwrap();
        fs::write(second.join("artifacts/fedcba9876543210.olean"), "second").unwrap();

        let report = inspect(&cache, Some("leanprover/lean4:v4.first")).unwrap();
        assert_eq!(report.stats.toolchain_caches, 1);
        assert_eq!(report.stats.artifacts, 1);

        let transaction = gc_plan(&cache, None, 1).unwrap();
        assert!(transaction.plan.candidates.is_empty());
        assert!(first.join("artifacts/0123456789abcdef.olean").exists());
        assert!(second.join("artifacts/fedcba9876543210.olean").exists());
    }

    #[test]
    fn shared_build_guard_excludes_garbage_collection() {
        let temp = tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let toolchain = "leanprover/lean4:v4.test";
        let _shared = lock_shared(&cache, toolchain).unwrap();
        let competing = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(cache.lake_lock_path(toolchain))
            .unwrap();
        assert!(FileExt::try_lock_exclusive(&competing).is_err());
    }
}
