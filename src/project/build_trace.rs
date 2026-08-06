//! Bounded parsing and comparison of Lake build traces.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const MAX_ENTRIES: usize = 500_000;
const MAX_TRACE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_TRACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct TraceSnapshot {
    traces: BTreeMap<String, BuildTrace>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildTrace {
    dep_hash: Option<String>,
    inputs: BTreeMap<String, String>,
    target: Option<String>,
    metadata: TraceMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceChangeStatus {
    Created,
    Changed,
    Removed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceInputKind {
    Source,
    Dependency,
    Options,
    Toolchain,
    Configuration,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceInputChange {
    pub input: String,
    pub kind: TraceInputKind,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// One labeled input from the current trace.
///
/// This is deliberately distinct from [`TraceInputChange`]. A newly created
/// trace has useful current inputs, but no earlier trace against which those
/// inputs can be called changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceInput {
    pub input: String,
    pub kind: TraceInputKind,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceMetadata {
    pub schema_version: Option<String>,
    pub synthetic: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpaqueDepHashChange {
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TraceChange {
    pub trace: String,
    pub target: Option<String>,
    pub status: TraceChangeStatus,
    pub reason: String,
    pub inputs: Vec<TraceInputChange>,
    // Keep the existing build-inspection JSON contract stable. These details
    // are available to the causality layer through the accessors below.
    #[serde(skip)]
    current_inputs: Vec<TraceInput>,
    #[serde(skip)]
    previous_metadata: Option<TraceMetadata>,
    #[serde(skip)]
    current_metadata: Option<TraceMetadata>,
    #[serde(skip)]
    opaque_dep_hash_change: Option<OpaqueDepHashChange>,
}

impl TraceChange {
    pub fn current_inputs(&self) -> &[TraceInput] {
        &self.current_inputs
    }

    pub fn previous_metadata(&self) -> Option<&TraceMetadata> {
        self.previous_metadata.as_ref()
    }

    pub fn current_metadata(&self) -> Option<&TraceMetadata> {
        self.current_metadata.as_ref()
    }

    pub fn opaque_dep_hash_change(&self) -> Option<&OpaqueDepHashChange> {
        self.opaque_dep_hash_change.as_ref()
    }
}

impl TraceSnapshot {
    pub fn capture(project_root: &Path) -> Result<Self> {
        let build_root = project_root.join(".lake/build");
        if !build_root.is_dir() {
            return Ok(Self::default());
        }

        let mut snapshot = Self::default();
        let mut entries = 0_usize;
        let mut total_bytes = 0_u64;
        visit_traces(
            project_root,
            &build_root,
            &mut entries,
            &mut total_bytes,
            &mut snapshot,
        )?;
        Ok(snapshot)
    }

    pub fn changes_since(&self, current: &Self) -> Vec<TraceChange> {
        let paths = self
            .traces
            .keys()
            .chain(current.traces.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changes = Vec::new();

        for path in paths {
            match (self.traces.get(&path), current.traces.get(&path)) {
                (None, Some(trace)) => changes.push(TraceChange {
                    trace: path,
                    target: trace.target.clone(),
                    status: TraceChangeStatus::Created,
                    reason: "no previous Lake trace was available".to_owned(),
                    inputs: Vec::new(),
                    current_inputs: trace_inputs(&trace.inputs),
                    previous_metadata: None,
                    current_metadata: Some(trace.metadata.clone()),
                    opaque_dep_hash_change: None,
                }),
                (Some(trace), None) => changes.push(TraceChange {
                    trace: path,
                    target: trace.target.clone(),
                    status: TraceChangeStatus::Removed,
                    reason: "the previous Lake trace is no longer present".to_owned(),
                    inputs: Vec::new(),
                    current_inputs: Vec::new(),
                    previous_metadata: Some(trace.metadata.clone()),
                    current_metadata: None,
                    opaque_dep_hash_change: None,
                }),
                (Some(before), Some(after))
                    if before.dep_hash != after.dep_hash || before.inputs != after.inputs =>
                {
                    let inputs = compare_inputs(&before.inputs, &after.inputs);
                    let opaque_dep_hash_change = (before.dep_hash != after.dep_hash
                        && inputs.is_empty())
                    .then(|| OpaqueDepHashChange {
                        before: before.dep_hash.clone(),
                        after: after.dep_hash.clone(),
                    });
                    changes.push(TraceChange {
                        trace: path,
                        target: after.target.clone().or_else(|| before.target.clone()),
                        status: TraceChangeStatus::Changed,
                        reason: if opaque_dep_hash_change.is_some() {
                            "Lake's dependency hash changed without an exposed labeled input change"
                                .to_owned()
                        } else {
                            "one or more labeled Lake trace inputs changed".to_owned()
                        },
                        inputs,
                        current_inputs: trace_inputs(&after.inputs),
                        previous_metadata: Some(before.metadata.clone()),
                        current_metadata: Some(after.metadata.clone()),
                        opaque_dep_hash_change,
                    });
                }
                _ => {}
            }
        }
        changes
    }
}

fn visit_traces(
    project_root: &Path,
    directory: &Path,
    entries: &mut usize,
    total_bytes: &mut u64,
    snapshot: &mut TraceSnapshot,
) -> Result<()> {
    let mut children = fs::read_dir(directory)
        .with_context(|| {
            format!(
                "failed to read Lake build directory {}",
                directory.display()
            )
        })?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| {
            format!(
                "failed to read Lake build directory {}",
                directory.display()
            )
        })?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        *entries += 1;
        if *entries > MAX_ENTRIES {
            snapshot.warnings.push(format!(
                "stopped reading Lake traces after {MAX_ENTRIES} build entries"
            ));
            return Ok(());
        }
        let file_type = child
            .file_type()
            .with_context(|| format!("failed to inspect {}", child.path().display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            visit_traces(project_root, &child.path(), entries, total_bytes, snapshot)?;
            continue;
        }
        let path = child.path();
        if !file_type.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("trace")
        {
            continue;
        }
        let metadata = child
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.len() > MAX_TRACE_BYTES {
            snapshot.warnings.push(format!(
                "skipped oversized Lake trace {}",
                display_relative(project_root, &path)
            ));
            continue;
        }
        *total_bytes = total_bytes.saturating_add(metadata.len());
        if *total_bytes > MAX_TOTAL_TRACE_BYTES {
            snapshot
                .warnings
                .push("stopped after reading 2 GiB of Lake trace metadata".to_owned());
            return Ok(());
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        match parse_trace(&bytes) {
            Ok(trace) => {
                snapshot
                    .traces
                    .insert(display_relative(project_root, &path), trace);
            }
            Err(error) => snapshot.warnings.push(format!(
                "could not parse {}: {error:#}",
                display_relative(project_root, &path)
            )),
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RawTrace {
    #[serde(rename = "depHash")]
    dep_hash: Option<String>,
    #[serde(default)]
    inputs: serde_json::Value,
    #[serde(rename = "schemaVersion")]
    schema_version: Option<serde_json::Value>,
    synthetic: Option<bool>,
}

fn parse_trace(bytes: &[u8]) -> Result<BuildTrace> {
    let raw: RawTrace = serde_json::from_slice(bytes).context("invalid trace JSON")?;
    let mut inputs = BTreeMap::new();
    flatten_inputs(&raw.inputs, &mut Vec::new(), &mut inputs);
    let target = inputs.keys().find_map(|label| {
        label
            .split(" / ")
            .find_map(|part| part.strip_prefix("Module.name: "))
            .map(str::to_owned)
    });
    Ok(BuildTrace {
        dep_hash: raw.dep_hash,
        inputs,
        target,
        metadata: TraceMetadata {
            schema_version: raw.schema_version.and_then(render_schema_version),
            synthetic: raw.synthetic,
        },
    })
}

fn render_schema_version(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn flatten_inputs(
    value: &serde_json::Value,
    prefix: &mut Vec<String>,
    inputs: &mut BTreeMap<String, String>,
) {
    let Some(entries) = value.as_array() else {
        return;
    };
    for entry in entries {
        let Some(pair) = entry.as_array().filter(|pair| pair.len() == 2) else {
            continue;
        };
        let Some(label) = pair[0].as_str() else {
            continue;
        };
        prefix.push(label.to_owned());
        if let Some(hash) = pair[1].as_str() {
            inputs.insert(prefix.join(" / "), hash.to_owned());
        } else {
            flatten_inputs(&pair[1], prefix, inputs);
        }
        prefix.pop();
    }
}

fn compare_inputs(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<TraceInputChange> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|input| {
            let old = before.get(input);
            let new = after.get(input);
            (old != new).then(|| TraceInputChange {
                input: input.clone(),
                kind: classify_input(input),
                before: old.cloned(),
                after: new.cloned(),
            })
        })
        .collect()
}

fn trace_inputs(inputs: &BTreeMap<String, String>) -> Vec<TraceInput> {
    inputs
        .iter()
        .map(|(input, hash)| TraceInput {
            input: input.clone(),
            kind: classify_input(input),
            hash: hash.clone(),
        })
        .collect()
}

fn classify_input(input: &str) -> TraceInputKind {
    let lower = input.to_ascii_lowercase();
    if lower.contains("lean ") && lower.contains("commit") {
        TraceInputKind::Toolchain
    } else if lower.contains("option") || lower.contains("leanargs") {
        TraceInputKind::Options
    } else if lower.contains(".lean") {
        TraceInputKind::Source
    } else if lower.contains("import") || lower.contains("deps") || lower.contains("library") {
        TraceInputKind::Dependency
    } else if lower.contains("package.")
        || lower.contains("module.name")
        || lower.contains("ismodule")
    {
        TraceInputKind::Configuration
    } else {
        TraceInputKind::Other
    }
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::{TraceChangeStatus, TraceInputKind, TraceSnapshot, classify_input, parse_trace};

    #[test]
    fn flattens_labeled_inputs_and_recovers_the_module_target() {
        let trace = parse_trace(
            br#"{
              "depHash": "new",
              "schemaVersion": 3,
              "synthetic": true,
              "inputs": [
                ["Root:deps", [["imports", [["Mathlib:importAllArts", "abc"]]]]],
                ["/project/Root.lean", "source"],
                ["Module.name: Root", "module"]
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(trace.target.as_deref(), Some("Root"));
        assert_eq!(trace.inputs["/project/Root.lean"], "source");
        assert_eq!(
            trace.inputs["Root:deps / imports / Mathlib:importAllArts"],
            "abc"
        );
        assert_eq!(trace.metadata.schema_version.as_deref(), Some("3"));
        assert_eq!(trace.metadata.synthetic, Some(true));
    }

    #[test]
    fn compares_only_changed_trace_inputs() {
        let before = TraceSnapshot {
            traces: [(
                ".lake/build/Root.trace".to_owned(),
                parse_trace(br#"{"depHash":"a","inputs":[["Root.lean","one"]]}"#).unwrap(),
            )]
            .into(),
            warnings: Vec::new(),
        };
        let after = TraceSnapshot {
            traces: [(
                ".lake/build/Root.trace".to_owned(),
                parse_trace(br#"{"depHash":"b","inputs":[["Root.lean","two"]]}"#).unwrap(),
            )]
            .into(),
            warnings: Vec::new(),
        };
        let changes = before.changes_since(&after);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].inputs.len(), 1);
        assert_eq!(changes[0].inputs[0].kind, TraceInputKind::Source);
        assert!(changes[0].opaque_dep_hash_change().is_none());
        assert_eq!(changes[0].current_inputs().len(), 1);
    }

    #[test]
    fn exposes_cold_trace_inputs_without_reporting_them_as_changes() {
        let before = TraceSnapshot::default();
        let after = TraceSnapshot {
            traces: [(
                ".lake/build/Root.trace".to_owned(),
                parse_trace(
                    br#"{
                      "depHash":"new",
                      "schemaVersion":"1.2.0",
                      "synthetic":false,
                      "inputs":[
                        ["Root.lean","source"],
                        ["Module.name: Root","module"]
                      ]
                    }"#,
                )
                .unwrap(),
            )]
            .into(),
            warnings: Vec::new(),
        };

        let changes = before.changes_since(&after);
        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert_eq!(change.status, TraceChangeStatus::Created);
        assert!(change.inputs.is_empty());
        assert_eq!(
            change
                .current_inputs()
                .iter()
                .map(|input| input.input.as_str())
                .collect::<Vec<_>>(),
            ["Module.name: Root", "Root.lean"]
        );
        assert_eq!(
            change.current_metadata().unwrap().schema_version.as_deref(),
            Some("1.2.0")
        );
        assert_eq!(change.current_metadata().unwrap().synthetic, Some(false));
        assert!(change.previous_metadata().is_none());

        let serialized = serde_json::to_value(change).unwrap();
        assert_eq!(
            serialized.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["inputs", "reason", "status", "target", "trace"]
        );
        assert!(serialized["inputs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn represents_an_unexplained_dependency_hash_change_as_opaque() {
        let before = TraceSnapshot {
            traces: [(
                ".lake/build/Root.trace".to_owned(),
                parse_trace(br#"{"depHash":"before","inputs":[["Root.lean","same"]]}"#).unwrap(),
            )]
            .into(),
            warnings: Vec::new(),
        };
        let after = TraceSnapshot {
            traces: [(
                ".lake/build/Root.trace".to_owned(),
                parse_trace(br#"{"depHash":"after","inputs":[["Root.lean","same"]]}"#).unwrap(),
            )]
            .into(),
            warnings: Vec::new(),
        };

        let changes = before.changes_since(&after);
        let opaque = changes[0].opaque_dep_hash_change().unwrap();
        assert!(changes[0].inputs.is_empty());
        assert_eq!(opaque.before.as_deref(), Some("before"));
        assert_eq!(opaque.after.as_deref(), Some("after"));
    }

    #[test]
    fn preserves_modern_metadata_on_removed_traces() {
        let before = TraceSnapshot {
            traces: [(
                ".lake/build/Root.trace".to_owned(),
                parse_trace(
                    br#"{
                      "depHash":"before",
                      "schemaVersion":4,
                      "synthetic":true,
                      "inputs":[]
                    }"#,
                )
                .unwrap(),
            )]
            .into(),
            warnings: Vec::new(),
        };

        let changes = before.changes_since(&TraceSnapshot::default());
        assert_eq!(changes[0].status, TraceChangeStatus::Removed);
        assert_eq!(
            changes[0]
                .previous_metadata()
                .unwrap()
                .schema_version
                .as_deref(),
            Some("4")
        );
        assert_eq!(
            changes[0].previous_metadata().unwrap().synthetic,
            Some(true)
        );
        assert!(changes[0].current_metadata().is_none());
    }

    #[test]
    fn classifies_lake_owned_inputs_without_guessing_unknown_labels() {
        assert_eq!(
            classify_input("Lean 4.x, commit abc"),
            TraceInputKind::Toolchain
        );
        assert_eq!(classify_input("Module.leanArgs"), TraceInputKind::Options);
        assert_eq!(classify_input("opaque key"), TraceInputKind::Other);
    }
}
