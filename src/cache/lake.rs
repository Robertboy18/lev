//! Lake's artifact cache: where a build puts it, what it holds, and which of
//! its entries no registered project references.
//!
//! Lake 5 writes this layout (cache schema 2026-02-25):
//!
//! ```text
//! <cache>/artifacts/<16 hex>.<extension>   build outputs, named by content hash
//! <cache>/outputs/<package>/<16 hex>.json  one build's input hash -> its outputs
//! ```
//!
//! A mapping is named after the `depHash` of the trace whose build wrote it, so
//! a project references a mapping when one of its traces has that hash, and an
//! artifact when one of its traces or a kept mapping names it. A cache that
//! still has the older `inputs/` directory is reported and never pruned.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use super::GcCandidate;
use crate::core::clock::modified_seconds;
use crate::core::hex::is_lower_hex;

/// How Lake chose the cache directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `LAKE_CACHE_DIR` names it.
    Environment,
    /// `LAKE_CACHE_DIR` is empty, so each workspace keeps `.lake/cache`.
    Workspace,
    /// The default: `lake/cache` inside the elan toolchain.
    Toolchain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    pub path: PathBuf,
    pub source: Source,
}

/// Decide where Lake keeps the artifact cache for builds of `project_root`,
/// following Lake's own rules. `lake_cache_dir` is the value of
/// `LAKE_CACHE_DIR`; `toolchain_root` finds the elan toolchain directory.
pub fn locate(
    lake_cache_dir: Option<OsString>,
    project_root: &Path,
    toolchain_root: impl FnOnce() -> Result<PathBuf>,
) -> Result<Location> {
    Ok(match lake_cache_dir {
        Some(dir) if dir.is_empty() => Location {
            path: project_root.join(".lake/cache"),
            source: Source::Workspace,
        },
        Some(dir) => Location {
            path: project_root.join(dir),
            source: Source::Environment,
        },
        None => Location {
            path: toolchain_root()?.join("lake").join("cache"),
            source: Source::Toolchain,
        },
    })
}

/// What the registered projects' traces name.
#[derive(Debug, Default)]
pub struct References {
    /// Trace `depHash` values, which are also mapping file names.
    pub hashes: BTreeSet<String>,
    /// Artifact file names from trace `outputs`.
    pub artifacts: BTreeSet<String>,
}

/// The facts `lev cache status` reports about one Lake cache.
#[derive(Debug, Default, Serialize)]
pub struct Inventory {
    pub path: PathBuf,
    pub exists: bool,
    /// The cache has Lake's older `inputs/` directory, which lev does not read.
    pub legacy_layout: bool,
    pub artifacts: u64,
    pub artifact_bytes: u64,
    /// Artifacts also linked from a build directory; removing them frees nothing.
    pub hardlinked_artifacts: u64,
    pub hardlinked_bytes: u64,
    pub mappings: u64,
    /// Artifacts that no mapping names.
    pub unmapped_artifacts: u64,
    pub unmapped_bytes: u64,
    /// Local mappings that name an artifact the cache does not hold.
    pub missing: Vec<MissingArtifact>,
}

#[derive(Debug, Serialize)]
pub struct MissingArtifact {
    pub mapping: PathBuf,
    pub artifact: String,
}

#[derive(Debug)]
struct Artifact {
    name: String,
    path: PathBuf,
    bytes: u64,
    links: u64,
}

#[derive(Debug)]
struct Mapping {
    path: PathBuf,
    hash: String,
    bytes: u64,
    artifacts: BTreeSet<String>,
}

/// One read of a Lake cache directory.
#[derive(Debug)]
pub struct Scan {
    pub inventory: Inventory,
    artifacts: Vec<Artifact>,
    mappings: Vec<Mapping>,
}

/// Read the artifacts and mappings of the cache at `root`.
pub fn scan(root: &Path) -> Result<Scan> {
    let mut inventory = Inventory {
        path: root.to_owned(),
        exists: root.is_dir(),
        legacy_layout: root.join("inputs").is_dir(),
        ..Inventory::default()
    };
    let artifacts = read_artifacts(&root.join("artifacts"))?;
    let present = artifacts
        .iter()
        .map(|artifact| artifact.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut mappings = Vec::new();
    let mut mapped = BTreeSet::new();
    let outputs = root.join("outputs");
    if outputs.is_dir() {
        read_mappings(&outputs, &mut |path, remote, mapping| {
            if !remote {
                for artifact in &mapping.artifacts {
                    if !present.contains(artifact.as_str()) {
                        inventory.missing.push(MissingArtifact {
                            mapping: path.to_owned(),
                            artifact: artifact.clone(),
                        });
                    }
                }
            }
            mapped.extend(mapping.artifacts.iter().cloned());
            mappings.push(mapping);
        })?;
    }

    inventory.mappings = mappings.len() as u64;
    for artifact in &artifacts {
        inventory.artifacts += 1;
        inventory.artifact_bytes += artifact.bytes;
        if artifact.links > 1 {
            inventory.hardlinked_artifacts += 1;
            inventory.hardlinked_bytes += artifact.bytes;
        }
        if !mapped.contains(&artifact.name) {
            inventory.unmapped_artifacts += 1;
            inventory.unmapped_bytes += artifact.bytes;
        }
    }
    Ok(Scan {
        inventory,
        artifacts,
        mappings,
    })
}

impl Scan {
    /// Mappings that no reference names, then artifacts that neither a
    /// reference nor a kept mapping names, all older than `cutoff`. Artifacts
    /// with another hard link stay, since removing them would free nothing.
    /// `references` must be complete for this cache: whatever it misses
    /// looks unused.
    pub fn gc_candidates(&self, references: &References, cutoff: u64) -> Vec<GcCandidate> {
        let mut keep = references.artifacts.clone();
        let mut candidates = Vec::new();
        for mapping in &self.mappings {
            if references.hashes.contains(&mapping.hash) || modified_seconds(&mapping.path) > cutoff
            {
                keep.extend(mapping.artifacts.iter().cloned());
            } else {
                candidates.push(GcCandidate {
                    kind: "lake-mapping",
                    path: mapping.path.clone(),
                    bytes: mapping.bytes,
                });
            }
        }
        for artifact in &self.artifacts {
            if !keep.contains(&artifact.name)
                && artifact.links <= 1
                && modified_seconds(&artifact.path) <= cutoff
            {
                candidates.push(GcCandidate {
                    kind: "lake-artifact",
                    path: artifact.path.clone(),
                    bytes: artifact.bytes,
                });
            }
        }
        candidates
    }
}

/// Files in `artifacts/` whose names Lake could have written, sorted by name.
fn read_artifacts(directory: &Path) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    if !directory.is_dir() {
        return Ok(artifacts);
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", directory.display()))?;
        let path = entry.path();
        // Lake may remove a file between the listing and this read.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if metadata.is_file() && is_artifact_name(&name) {
            artifacts.push(Artifact {
                name,
                path,
                bytes: metadata.len(),
                links: hard_links(&metadata),
            });
        }
    }
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(artifacts)
}

fn read_mappings(directory: &Path, visit: &mut dyn FnMut(&Path, bool, Mapping)) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            read_mappings(&path, visit)?;
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        // Current mappings wrap the outputs as `{schemaVersion, service, data}`;
        // a mapping with a cache service may fetch its artifacts on first use.
        let (remote, data) = match value.as_object() {
            Some(object) if object.contains_key("schemaVersion") => (
                object
                    .get("service")
                    .is_some_and(|service| !service.is_null()),
                object
                    .get("data")
                    .with_context(|| format!("{} has no data", path.display()))?,
            ),
            _ => (false, &value),
        };
        let mut artifacts = BTreeSet::new();
        collect_descriptors(data, &mut artifacts);
        let hash = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        visit(
            &path,
            remote,
            Mapping {
                path: path.clone(),
                hash,
                bytes: bytes.len() as u64,
                artifacts,
            },
        );
    }
    Ok(())
}

fn collect_descriptors(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::String(name) => {
            output.insert(name.clone());
        }
        // Lake also accepts a bare 64-bit hash for an `.art` artifact.
        Value::Number(hash) => {
            if let Some(hash) = hash.as_u64() {
                output.insert(format!("{hash:016x}.art"));
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_descriptors(value, output)),
        Value::Object(values) => values
            .values()
            .for_each(|value| collect_descriptors(value, output)),
        Value::Null | Value::Bool(_) => {}
    }
}

/// `<16 lowercase hex>` with an optional extension, as one path component.
fn is_artifact_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return false;
    }
    let hash = name.split_once('.').map_or(name, |(hash, _)| hash);
    is_lower_hex(hash, 16)
}

#[cfg(unix)]
fn hard_links(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(not(unix))]
fn hard_links(_metadata: &fs::Metadata) -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{References, Source, is_artifact_name, locate, scan};

    fn write(root: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn local_mapping(data: &str) -> String {
        format!(r#"{{"schemaVersion":"2026-02-25","service":null,"data":{data}}}"#)
    }

    #[test]
    fn locates_the_cache_the_way_lake_does() {
        let project = Path::new("/work/project");
        let toolchain = || Ok(PathBuf::from("/elan/toolchains/tc"));
        let default = locate(None, project, toolchain).unwrap();
        assert_eq!(default.path, Path::new("/elan/toolchains/tc/lake/cache"));
        assert_eq!(default.source, Source::Toolchain);

        let explicit = locate(Some(OsString::from("/shared/lake")), project, toolchain).unwrap();
        assert_eq!(explicit.path, Path::new("/shared/lake"));
        assert_eq!(explicit.source, Source::Environment);

        let relative = locate(Some(OsString::from("cache")), project, toolchain).unwrap();
        assert_eq!(relative.path, Path::new("/work/project/cache"));

        let disabled = locate(Some(OsString::new()), project, || {
            anyhow::bail!("elan is not consulted")
        })
        .unwrap();
        assert_eq!(disabled.path, Path::new("/work/project/.lake/cache"));
        assert_eq!(disabled.source, Source::Workspace);
    }

    #[test]
    fn inventories_artifacts_mappings_and_missing_outputs() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        write(root, "artifacts/0123456789abcdef.olean", "olean");
        write(root, "artifacts/fedcba9876543210.ilean", "orphan");
        write(root, "artifacts/not-an-artifact", "ignored");
        write(
            root,
            "outputs/pkg/1111111111111111.json",
            &local_mapping(r#"{"o":["0123456789abcdef.olean"],"m":false}"#),
        );
        write(
            root,
            "outputs/pkg/2222222222222222.json",
            &local_mapping(r#""aaaaaaaaaaaaaaaa.olean""#),
        );
        write(
            root,
            "outputs/pkg/3333333333333333.json",
            r#"{"schemaVersion":"2026-02-25","service":"remote","data":"bbbbbbbbbbbbbbbb.ltar"}"#,
        );

        let inventory = scan(root).unwrap().inventory;
        assert!(inventory.exists && !inventory.legacy_layout);
        assert_eq!(inventory.artifacts, 2);
        assert_eq!(inventory.artifact_bytes, 11);
        assert_eq!(inventory.mappings, 3);
        assert_eq!(inventory.unmapped_artifacts, 1);
        assert_eq!(inventory.missing.len(), 1);
        assert_eq!(inventory.missing[0].artifact, "aaaaaaaaaaaaaaaa.olean");
    }

    #[test]
    fn collects_unreferenced_mappings_then_their_artifacts() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        write(root, "artifacts/0123456789abcdef.olean", "kept by a trace");
        write(
            root,
            "artifacts/1111111111111111.olean",
            "kept by a mapping",
        );
        write(root, "artifacts/2222222222222222.olean", "mapping goes");
        write(root, "artifacts/3333333333333333.olean", "named by nothing");
        let referenced = write(
            root,
            "outputs/pkg/aaaaaaaaaaaaaaaa.json",
            &local_mapping(r#""1111111111111111.olean""#),
        );
        let unreferenced = write(
            root,
            "outputs/pkg/bbbbbbbbbbbbbbbb.json",
            &local_mapping(r#""2222222222222222.olean""#),
        );
        let references = References {
            hashes: ["aaaaaaaaaaaaaaaa".to_owned()].into(),
            artifacts: ["0123456789abcdef.olean".to_owned()].into(),
        };

        let scan = scan(root).unwrap();
        assert!(scan.gc_candidates(&references, 0).is_empty());
        let candidates = scan.gc_candidates(&references, u64::MAX);
        let paths = candidates
            .iter()
            .map(|candidate| (candidate.kind, candidate.path.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                ("lake-mapping", unreferenced),
                (
                    "lake-artifact",
                    root.join("artifacts/2222222222222222.olean")
                ),
                (
                    "lake-artifact",
                    root.join("artifacts/3333333333333333.olean")
                ),
            ]
        );
        assert!(referenced.exists());
    }

    #[cfg(unix)]
    #[test]
    fn keeps_artifacts_that_a_build_directory_links() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("cache");
        let artifact = write(&root, "artifacts/0123456789abcdef.olean", "linked");
        fs::hard_link(&artifact, temp.path().join("Root.olean")).unwrap();

        let scan = scan(&root).unwrap();
        assert_eq!(scan.inventory.hardlinked_artifacts, 1);
        assert!(
            scan.gc_candidates(&References::default(), u64::MAX)
                .is_empty()
        );
    }

    #[test]
    fn recognizes_lake_artifact_names() {
        for valid in [
            "0123456789abcdef.olean",
            "0000000000000000",
            "8b25b976b9b824e7.ltar",
        ] {
            assert!(is_artifact_name(valid), "{valid}");
        }
        for invalid in [
            "",
            ".olean",
            "01.art",
            "0123456789abcdeF.olean",
            "0123456789abcdeg.olean",
            "../0123456789abcdef.olean",
            "0123456789abcdef/sub.olean",
        ] {
            assert!(!is_artifact_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn rejects_mappings_it_cannot_parse() {
        let temp = tempdir().unwrap();
        write(temp.path(), "outputs/pkg/broken.json", "{");
        let error = scan(temp.path()).unwrap_err().to_string();
        assert!(error.contains("broken.json"), "{error}");
    }
}
