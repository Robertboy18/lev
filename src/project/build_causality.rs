//! Conservative project-level rebuild causality.
//!
//! Lake traces record observed build metadata. Lean imports support a separate
//! source-level "could affect" relation. This module keeps those statements
//! apart and records where neither source can provide an explanation.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::project::build_trace::{
    OpaqueDepHashChange, TraceChange, TraceChangeStatus, TraceInput, TraceInputChange,
    TraceMetadata,
};
use crate::project::import_graph::ImportGraph;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalityEvidenceKind {
    ObservedTrace,
    SourceImportGraph,
    MissingEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedTraceEvidence {
    pub evidence: CausalityEvidenceKind,
    pub trace: String,
    pub target: Option<String>,
    pub status: TraceChangeStatus,
    pub labeled_changes: Vec<TraceInputChange>,
    pub current_inputs: Vec<TraceInput>,
    pub previous_metadata: Option<TraceMetadata>,
    pub current_metadata: Option<TraceMetadata>,
    pub opaque_dep_hash_change: Option<OpaqueDepHashChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PotentialImpact {
    pub evidence: CausalityEvidenceKind,
    pub source_module: String,
    pub affected_module: String,
    pub shortest_path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct MissingCausalityEvidence {
    pub evidence: CausalityEvidenceKind,
    pub subject: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RebuildCausality {
    pub observed_traces: Vec<ObservedTraceEvidence>,
    pub potential_affected_modules: Vec<String>,
    pub potential_impacts: Vec<PotentialImpact>,
    pub missing_evidence: Vec<MissingCausalityEvidence>,
}

impl RebuildCausality {
    /// Combine trace observations with a source-level potential blast radius.
    ///
    /// `changed_project_modules` must be supplied by a caller that has already
    /// mapped its initiating changes to project modules. Trace targets are not
    /// silently promoted to initiating causes. The result therefore makes no
    /// claim to reproduce Lake's complete job DAG.
    pub fn analyze(
        graph: &ImportGraph,
        changed_project_modules: &[String],
        trace_changes: &[TraceChange],
    ) -> Self {
        let graph_modules = graph
            .nodes
            .iter()
            .map(|node| node.module.as_str())
            .collect::<BTreeSet<_>>();
        let blast = graph.potential_blast_radius(changed_project_modules);
        let observed_traces = trace_changes
            .iter()
            .map(|change| ObservedTraceEvidence {
                evidence: CausalityEvidenceKind::ObservedTrace,
                trace: change.trace.clone(),
                target: change.target.clone(),
                status: change.status,
                labeled_changes: change.inputs.clone(),
                current_inputs: change.current_inputs().to_vec(),
                previous_metadata: change.previous_metadata().cloned(),
                current_metadata: change.current_metadata().cloned(),
                opaque_dep_hash_change: change.opaque_dep_hash_change().cloned(),
            })
            .collect();
        let potential_impacts = blast
            .paths
            .into_iter()
            .map(|path| PotentialImpact {
                evidence: CausalityEvidenceKind::SourceImportGraph,
                source_module: path.source,
                affected_module: path.affected,
                shortest_path: path.path,
            })
            .collect();
        let mut missing_evidence = blast
            .unknown_sources
            .into_iter()
            .map(|module| MissingCausalityEvidence {
                evidence: CausalityEvidenceKind::MissingEvidence,
                subject: module,
                detail: "the proposed changed module is not a source in this project graph"
                    .to_owned(),
            })
            .collect::<BTreeSet<_>>();

        for change in trace_changes {
            let subject = change
                .target
                .clone()
                .unwrap_or_else(|| change.trace.clone());
            match change.status {
                TraceChangeStatus::Created => {
                    missing_evidence.insert(MissingCausalityEvidence {
                        evidence: CausalityEvidenceKind::MissingEvidence,
                        subject: subject.clone(),
                        detail: "the trace is new, so no previous trace exists for comparison"
                            .to_owned(),
                    });
                }
                TraceChangeStatus::Removed => {
                    missing_evidence.insert(MissingCausalityEvidence {
                        evidence: CausalityEvidenceKind::MissingEvidence,
                        subject: subject.clone(),
                        detail: "the trace was removed, so no current trace exists".to_owned(),
                    });
                }
                TraceChangeStatus::Changed => {}
            }
            if change.opaque_dep_hash_change().is_some() {
                missing_evidence.insert(MissingCausalityEvidence {
                    evidence: CausalityEvidenceKind::MissingEvidence,
                    subject: subject.clone(),
                    detail: "the dependency hash changed without an exposed labeled input change"
                        .to_owned(),
                });
            }
            match change.target.as_deref() {
                None => {
                    missing_evidence.insert(MissingCausalityEvidence {
                        evidence: CausalityEvidenceKind::MissingEvidence,
                        subject,
                        detail: "the trace exposes no project module target".to_owned(),
                    });
                }
                Some(target) if !graph_modules.contains(target) => {
                    missing_evidence.insert(MissingCausalityEvidence {
                        evidence: CausalityEvidenceKind::MissingEvidence,
                        subject,
                        detail: "the trace target does not map to a project source module; it may be external, generated, or a custom Lake target".to_owned(),
                    });
                }
                Some(_) => {}
            }
        }

        Self {
            observed_traces,
            potential_affected_modules: blast.affected_modules,
            potential_impacts,
            missing_evidence: missing_evidence.into_iter().collect(),
        }
    }

    pub fn evidence_kinds(&self) -> BTreeSet<CausalityEvidenceKind> {
        let mut kinds = BTreeSet::new();
        if !self.observed_traces.is_empty() {
            kinds.insert(CausalityEvidenceKind::ObservedTrace);
        }
        if !self.potential_impacts.is_empty() {
            kinds.insert(CausalityEvidenceKind::SourceImportGraph);
        }
        if !self.missing_evidence.is_empty() {
            kinds.insert(CausalityEvidenceKind::MissingEvidence);
        }
        kinds
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use crate::project::Project;
    use crate::project::build_trace::TraceSnapshot;
    use crate::project::import_graph::ImportGraph;

    use super::{CausalityEvidenceKind, RebuildCausality};

    #[test]
    fn keeps_observations_inference_and_missing_evidence_separate() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "test\n").unwrap();
        fs::write(temp.path().join("A.lean"), "def a := 1\n").unwrap();
        fs::write(temp.path().join("B.lean"), "import A\ndef b := a\n").unwrap();
        fs::write(temp.path().join("C.lean"), "import A\ndef c := a\n").unwrap();
        fs::write(
            temp.path().join("D.lean"),
            "import B\nimport C\ndef d := b + c\n",
        )
        .unwrap();
        let trace_path = temp.path().join(".lake/build/A.trace");
        fs::create_dir_all(trace_path.parent().unwrap()).unwrap();
        fs::write(
            &trace_path,
            serde_json::to_vec(&json!({
                "depHash": "before",
                "inputs": [
                    ["A.lean", "before-source"],
                    ["Module.name: A", "module"]
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let before = TraceSnapshot::capture(temp.path()).unwrap();
        fs::write(
            &trace_path,
            serde_json::to_vec(&json!({
                "depHash": "after",
                "schemaVersion": 3,
                "synthetic": false,
                "inputs": [
                    ["A.lean", "after-source"],
                    ["Module.name: A", "module"]
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let current = TraceSnapshot::capture(temp.path()).unwrap();
        let changes = before.changes_since(&current);
        let project = Project::load(temp.path().to_owned()).unwrap();
        let graph = ImportGraph::inspect(&project, &[], false).unwrap();

        let analysis =
            RebuildCausality::analyze(&graph, &["A".to_owned(), "Unknown".to_owned()], &changes);

        assert_eq!(analysis.observed_traces.len(), 1);
        assert_eq!(
            analysis.observed_traces[0].evidence,
            CausalityEvidenceKind::ObservedTrace
        );
        assert_eq!(analysis.observed_traces[0].labeled_changes.len(), 1);
        assert_eq!(
            analysis.observed_traces[0]
                .current_metadata
                .as_ref()
                .unwrap()
                .schema_version
                .as_deref(),
            Some("3")
        );
        assert_eq!(analysis.potential_affected_modules, ["A", "B", "C", "D"]);
        let d = analysis
            .potential_impacts
            .iter()
            .find(|impact| impact.affected_module == "D")
            .unwrap();
        assert_eq!(d.evidence, CausalityEvidenceKind::SourceImportGraph);
        assert_eq!(d.shortest_path, ["A", "B", "D"]);
        assert!(analysis.missing_evidence.iter().any(|missing| {
            missing.subject == "Unknown" && missing.detail.contains("not a source")
        }));
        assert_eq!(
            analysis.evidence_kinds(),
            [
                CausalityEvidenceKind::ObservedTrace,
                CausalityEvidenceKind::SourceImportGraph,
                CausalityEvidenceKind::MissingEvidence
            ]
            .into()
        );
    }

    #[test]
    fn records_cold_opaque_and_unmapped_trace_limits() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "test\n").unwrap();
        fs::write(temp.path().join("Root.lean"), "def root := 1\n").unwrap();
        let trace_root = temp.path().join(".lake/build");
        fs::create_dir_all(&trace_root).unwrap();
        fs::write(
            trace_root.join("Root.trace"),
            br#"{"depHash":"before","inputs":[["Module.name: Root","module"]]}"#,
        )
        .unwrap();
        fs::write(
            trace_root.join("Custom.trace"),
            br#"{"depHash":"before","inputs":[["unlabeled","same"]]}"#,
        )
        .unwrap();
        let before = TraceSnapshot::capture(temp.path()).unwrap();
        fs::write(
            trace_root.join("Root.trace"),
            br#"{"depHash":"after","inputs":[["Module.name: Root","module"]]}"#,
        )
        .unwrap();
        fs::write(
            trace_root.join("Custom.trace"),
            br#"{"depHash":"after","inputs":[["unlabeled","same"]]}"#,
        )
        .unwrap();
        fs::write(
            trace_root.join("Cold.trace"),
            br#"{"depHash":"cold","inputs":[["Cold.lean","source"]]}"#,
        )
        .unwrap();
        let current = TraceSnapshot::capture(temp.path()).unwrap();
        let changes = before.changes_since(&current);
        let project = Project::load(temp.path().to_owned()).unwrap();
        let graph = ImportGraph::inspect(&project, &[], false).unwrap();

        let analysis = RebuildCausality::analyze(&graph, &[], &changes);

        assert!(analysis.observed_traces.iter().any(|trace| {
            trace.target.as_deref() == Some("Root") && trace.opaque_dep_hash_change.is_some()
        }));
        let cold = analysis
            .observed_traces
            .iter()
            .find(|trace| trace.trace.ends_with("Cold.trace"))
            .unwrap();
        assert!(cold.labeled_changes.is_empty());
        assert_eq!(cold.current_inputs.len(), 1);
        assert!(analysis.missing_evidence.iter().any(|missing| {
            missing.subject.ends_with("Cold.trace") && missing.detail.contains("no previous")
        }));
        assert!(analysis.missing_evidence.iter().any(|missing| {
            missing.subject.ends_with("Custom.trace")
                && missing.detail.contains("no project module target")
        }));
        assert!(analysis.missing_evidence.iter().any(|missing| {
            missing.subject == "Root" && missing.detail.contains("dependency hash changed")
        }));
        assert!(analysis.potential_impacts.is_empty());
    }
}
