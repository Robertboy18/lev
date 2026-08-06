//! Shared diagnostic findings kept separate from command-specific JSON DTOs.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// Overall result of a command or policy run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunStatus {
    Passed,
    Failed,
}

/// Broad owner of one diagnostic finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingCategory {
    Build,
    Cache,
    Configuration,
    Dependency,
    Environment,
    Project,
    Trust,
    Workspace,
}

/// Whether the check passed, raised a finding, or did not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingState {
    Passed,
    Raised,
    NotApplicable,
}

/// Importance of the observed condition, independent of policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingSeverity {
    Info,
    Warning,
    Error,
}

/// How a policy command should treat the observed condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PolicyDisposition {
    Observe,
    Pass,
    Fail,
}

/// One independently actionable diagnostic result.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Finding {
    pub id: String,
    pub category: FindingCategory,
    pub state: FindingState,
    pub severity: FindingSeverity,
    pub title: String,
    pub detail: String,
    pub evidence: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<f64>,
    pub policy: PolicyDisposition,
}

/// Counts shared by human and machine-readable diagnostic reports.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct FindingSummary {
    pub passed: usize,
    pub warnings: usize,
    pub errors: usize,
    pub not_applicable: usize,
    pub policy_failures: usize,
}

impl Finding {
    pub(crate) fn passed(
        id: impl Into<String>,
        category: FindingCategory,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            category,
            FindingState::Passed,
            FindingSeverity::Info,
            title,
            detail,
        )
    }

    pub(crate) fn warning(
        id: impl Into<String>,
        category: FindingCategory,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            category,
            FindingState::Raised,
            FindingSeverity::Warning,
            title,
            detail,
        )
    }

    pub(crate) fn error(
        id: impl Into<String>,
        category: FindingCategory,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            category,
            FindingState::Raised,
            FindingSeverity::Error,
            title,
            detail,
        )
    }

    pub(crate) fn not_applicable(
        id: impl Into<String>,
        category: FindingCategory,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            category,
            FindingState::NotApplicable,
            FindingSeverity::Info,
            title,
            detail,
        )
    }

    fn new(
        id: impl Into<String>,
        category: FindingCategory,
        state: FindingState,
        severity: FindingSeverity,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let id = id.into();
        debug_assert!(valid_finding_id(&id), "invalid finding ID {id:?}");
        Self {
            id,
            category,
            state,
            severity,
            title: title.into(),
            detail: detail.into(),
            evidence: BTreeMap::new(),
            remediation: None,
            elapsed_ms: None,
            policy: PolicyDisposition::Observe,
        }
    }

    pub(crate) fn with_evidence(mut self, key: impl Into<String>, value: Value) -> Self {
        self.evidence.insert(key.into(), value);
        self
    }

    pub(crate) fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }

    pub(crate) fn with_elapsed_ms(mut self, elapsed_ms: f64) -> Self {
        self.elapsed_ms = Some(elapsed_ms);
        self
    }

    pub(crate) fn with_policy(mut self, policy: PolicyDisposition) -> Self {
        self.policy = policy;
        self
    }
}

impl FindingSummary {
    pub(crate) fn from_findings(findings: &[Finding]) -> Self {
        let mut summary = Self::default();
        for finding in findings {
            match (finding.state, finding.severity) {
                (FindingState::Passed, _) => summary.passed += 1,
                (FindingState::NotApplicable, _) => summary.not_applicable += 1,
                (FindingState::Raised, FindingSeverity::Warning) => summary.warnings += 1,
                (FindingState::Raised, FindingSeverity::Error) => summary.errors += 1,
                (FindingState::Raised, FindingSeverity::Info) => {}
            }
            if finding.policy == PolicyDisposition::Fail {
                summary.policy_failures += 1;
            }
        }
        summary
    }
}

fn valid_finding_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.ends_with('.')
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::{Finding, FindingCategory, FindingSummary, PolicyDisposition, valid_finding_id};

    #[test]
    fn validates_stable_finding_ids() {
        assert!(valid_finding_id("environment.toolchain.runtime"));
        assert!(valid_finding_id("cache.artifact-path"));
        assert!(!valid_finding_id(""));
        assert!(!valid_finding_id(".cache"));
        assert!(!valid_finding_id("Cache.Path"));
        assert!(!valid_finding_id("cache path"));
    }

    #[test]
    fn summarizes_observation_and_policy_separately() {
        let findings = vec![
            Finding::passed(
                "project.discovery",
                FindingCategory::Project,
                "project",
                "found project",
            ),
            Finding::warning(
                "storage.capacity",
                FindingCategory::Cache,
                "storage",
                "space is low",
            ),
            Finding::error(
                "configuration.lock",
                FindingCategory::Configuration,
                "lock",
                "lock is stale",
            )
            .with_policy(PolicyDisposition::Fail),
            Finding::not_applicable(
                "workspace.lock",
                FindingCategory::Workspace,
                "workspace",
                "no workspace is configured",
            ),
        ];

        assert_eq!(
            FindingSummary::from_findings(&findings),
            FindingSummary {
                passed: 1,
                warnings: 1,
                errors: 1,
                not_applicable: 1,
                policy_failures: 1,
            }
        );
    }
}
