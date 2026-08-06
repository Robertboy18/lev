//! Explanations built from Lake traces and Lean declaration metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::{digest as cache_digest, registry};
use crate::cli::{
    InspectBuildArgs, InspectDeclarationArgs, InspectDependenciesArgs, InspectTrustArgs,
};
use crate::core::atomic_file::replace as atomic_write;
use crate::core::json_output::{self, schema};
use crate::core::process::{checked_output, exit_code, output_text};
use crate::dependency::environment as dependency_env;
use crate::dependency::pin_analysis::{self, PinAnalysis, PinStatus};
use crate::diagnostics::{
    Finding, FindingCategory, FindingSeverity, FindingState, FindingSummary, RunStatus,
};
use crate::project::Project;
use crate::project::build_causality::{CausalityEvidenceKind, RebuildCausality};
use crate::project::build_diagnostics::{self, BuildRootCause, BuildRootCauseKind};
use crate::project::build_trace::{TraceChange, TraceChangeStatus, TraceInputKind, TraceSnapshot};
use crate::project::config::LevConfig;
use crate::project::declaration_index::{self, DeclarationCandidate, DeclarationRange};
use crate::project::import_graph;
use crate::project::lakefile;
use crate::project::local_workspace;
use crate::project::lockfile;
use crate::project::manifest::LakeManifest;
use crate::project::trust_inspector::{self, TrustRecord};
use crate::project::workspace::ProjectWorkspace;

use super::AppContext;
use super::execution_commands::{display_os_command, prepared_project, project_command};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum DiagnosticStatus {
    Passed,
    Warning,
    Error,
}

#[derive(Debug, Serialize)]
struct DiagnosticCheck {
    name: String,
    status: DiagnosticStatus,
    detail: String,
}

impl DiagnosticCheck {
    /// Adapt shared findings to the published bare-inspect DTO.
    fn from_finding(name: impl Into<String>, finding: Finding) -> Self {
        let status = match (finding.state, finding.severity) {
            (FindingState::Passed, _) => DiagnosticStatus::Passed,
            (FindingState::NotApplicable, _) => DiagnosticStatus::Warning,
            (FindingState::Raised, FindingSeverity::Error) => DiagnosticStatus::Error,
            (FindingState::Raised, FindingSeverity::Info | FindingSeverity::Warning) => {
                DiagnosticStatus::Warning
            }
        };
        Self {
            name: name.into(),
            status,
            detail: finding.detail,
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct DiagnosticSummary {
    passed: usize,
    warnings: usize,
    errors: usize,
}

#[derive(Debug, Serialize)]
struct ProjectDiagnosis {
    root: String,
    toolchain: String,
    lakefile_kind: Option<String>,
    lakefile_path: Option<String>,
    manifest_path: String,
    manifest_packages: Option<usize>,
    lev_config_path: Option<String>,
    configured_tasks: Option<usize>,
    configured_checks: Option<usize>,
    build_metadata_present: bool,
    runtime_source: String,
}

#[derive(Debug, Serialize)]
struct ProjectInspectionReport {
    lev_version: String,
    host: String,
    start_path: String,
    cache_path: String,
    toolchain_data_path: String,
    project: Option<ProjectDiagnosis>,
    checks: Vec<DiagnosticCheck>,
    summary: DiagnosticSummary,
    commands: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EnvironmentLocations {
    start_path: String,
    cache_path: String,
    toolchain_data_path: String,
    registry_path: String,
    workspace_cache_path: String,
}

#[derive(Debug, Serialize)]
struct EnvironmentProject {
    root: String,
    toolchain: String,
    toolchain_file: String,
    lakefile_path: Option<String>,
    manifest_path: String,
    lock_path: String,
    config_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct EnvironmentRuntime {
    toolchain: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct EnvironmentVersions {
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lean: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lake: Option<String>,
}

#[derive(Debug, Serialize)]
struct EnvironmentRegistry {
    path: String,
    parsed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    records: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale_records: Option<usize>,
}

#[derive(Debug, Serialize)]
struct EnvironmentArtifactCache {
    path: String,
    present: bool,
}

#[derive(Debug, Serialize)]
struct EnvironmentWorkspace {
    configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    members: Option<usize>,
}

#[derive(Debug, Serialize)]
struct EnvironmentInspectionReport {
    lev_version: String,
    host: String,
    locations: EnvironmentLocations,
    versions: EnvironmentVersions,
    project: Option<EnvironmentProject>,
    runtime: Option<EnvironmentRuntime>,
    artifact_cache: Option<EnvironmentArtifactCache>,
    registry: EnvironmentRegistry,
    workspace: EnvironmentWorkspace,
    managed_local_workspace: Option<bool>,
    findings: Vec<Finding>,
    summary: FindingSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuildEventAction {
    Built,
    Fetched,
    Replayed,
    Reused,
    Ran,
    Unpacked,
}

#[derive(Debug, Serialize)]
struct BuildEvent {
    action: BuildEventAction,
    target: String,
    elapsed_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
struct BuildInspectionReport {
    project: String,
    toolchain: String,
    command: String,
    status: RunStatus,
    exit_code: i32,
    elapsed_ms: f64,
    built: usize,
    replayed: usize,
    ran: usize,
    reused: usize,
    fetched: usize,
    unpacked: usize,
    events: Vec<BuildEvent>,
    root_causes: Vec<BuildRootCause>,
    failed_targets: Vec<String>,
    trace_changes: Vec<TraceChange>,
    unattributed_work: Vec<String>,
    causality: BuildCausalityReport,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BuildCausalityReport {
    source_graph_available: bool,
    changed_project_modules: Vec<String>,
    observed_rebuilt_modules: Vec<String>,
    evidence_kinds: BTreeSet<CausalityEvidenceKind>,
    #[serde(flatten)]
    analysis: RebuildCausality,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DeclarationInspectionReport {
    name: String,
    #[serde(rename = "type")]
    type_: String,
    module: String,
    #[serde(rename = "import")]
    import_statement: String,
    package: Option<String>,
    source: Option<String>,
    range: Option<DeclarationRange>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TrustDeclarationReport {
    name: String,
    module: String,
    package: String,
    source: Option<String>,
    range: Option<DeclarationRange>,
    axioms: Vec<String>,
    #[serde(rename = "unsafe")]
    is_unsafe: bool,
    denied_axioms: Vec<String>,
    unsafe_violation: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct TrustInspectionReport {
    project: String,
    toolchain: String,
    status: RunStatus,
    declarations_indexed: usize,
    declarations_scanned: usize,
    scan_failures: usize,
    declarations_with_axioms: usize,
    unsafe_declarations: usize,
    denied_axioms: Vec<String>,
    deny_unsafe: bool,
    policy_violations: usize,
    findings: Vec<TrustDeclarationReport>,
    warnings: Vec<String>,
}

struct TrustBatch {
    modules: Vec<String>,
    owners: BTreeMap<String, String>,
}

struct ScopedTrustRecord {
    module: String,
    record: TrustRecord,
}

/// Summarize enough local state to start a useful bug report.
pub(super) fn project(context: &AppContext, json: bool, output: Option<&Path>) -> Result<i32> {
    let report = inspect_project(context);
    let bytes = if json {
        json_output::render(schema::INSPECT, &report)?
    } else {
        render_project_report(&report).into_bytes()
    };
    if let Some(path) = output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_write(path, &bytes)?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write project diagnosis")?;
    }
    Ok(0)
}

/// Inspect the selected environment without creating cache state or walking build trees.
pub(super) fn environment(context: &AppContext, json: bool, output: Option<&Path>) -> Result<i32> {
    let report = inspect_environment(context);
    let bytes = if json {
        json_output::render(schema::INSPECT_ENVIRONMENT, &report)?
    } else {
        render_environment_report(&report).into_bytes()
    };
    if let Some(path) = output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_write(path, &bytes)?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write environment diagnosis")?;
    }
    Ok(0)
}

fn inspect_environment(context: &AppContext) -> EnvironmentInspectionReport {
    let mut findings = Vec::new();
    let locations = EnvironmentLocations {
        start_path: context.project_start.display().to_string(),
        cache_path: context.cache.root.display().to_string(),
        toolchain_data_path: context.store.root.display().to_string(),
        registry_path: context.cache.registry_root().display().to_string(),
        workspace_cache_path: context.cache.workspace_root().display().to_string(),
    };
    findings.push(
        Finding::passed(
            "environment.locations",
            FindingCategory::Environment,
            "Lev locations",
            "resolved the project start, cache, and toolchain data paths",
        )
        .with_evidence("start_path", json!(locations.start_path))
        .with_evidence("cache_path", json!(locations.cache_path))
        .with_evidence("toolchain_data_path", json!(locations.toolchain_data_path)),
    );

    let mut versions = EnvironmentVersions::default();
    let mut git = Command::new(&context.git);
    git.arg("--version");
    versions.git = record_version(&mut findings, git, "environment.git.version", "Git", false);

    let mut elan = Command::new(&context.elan);
    elan.arg("--version");
    versions.elan = record_version(
        &mut findings,
        elan,
        "environment.elan.version",
        "elan",
        true,
    );

    let discovered = match Project::discover(&context.project_start) {
        Ok(project) => Some(project),
        Err(error) => {
            if nearest_toolchain_file(&context.project_start).is_some() {
                findings.push(
                    Finding::error(
                        "project.discovery",
                        FindingCategory::Project,
                        "Project discovery",
                        format!("{error:#}"),
                    )
                    .with_remediation("fix the nearest lean-toolchain file"),
                );
            } else {
                findings.push(Finding::not_applicable(
                    "project.discovery",
                    FindingCategory::Project,
                    "Project discovery",
                    format!(
                        "no Lean project was found from {}",
                        context.project_start.display()
                    ),
                ));
            }
            None
        }
    };

    let project = discovered
        .as_ref()
        .map(|project| inspect_environment_project(project, &mut findings));
    let runtime = discovered.as_ref().and_then(|project| {
        inspect_environment_runtime(context, project, &mut versions, &mut findings)
    });
    if discovered.is_none() {
        for (id, title) in [
            ("environment.runtime.selected", "Selected runtime"),
            ("environment.lean.version", "Lean"),
            ("environment.lake.version", "Lake"),
            ("cache.artifacts.path", "Lake artifact cache"),
            ("workspace.managed_local", "Managed local workspace"),
        ] {
            findings.push(Finding::not_applicable(
                id,
                if id.starts_with("cache.") {
                    FindingCategory::Cache
                } else if id.starts_with("workspace.") {
                    FindingCategory::Workspace
                } else {
                    FindingCategory::Environment
                },
                title,
                "no project selected a Lean toolchain",
            ));
        }
    }

    let artifact_cache = discovered.as_ref().map(|project| {
        let path = context.cache.lake_dir(&project.toolchain);
        let present = path.is_dir();
        findings.push(
            Finding::passed(
                "cache.artifacts.path",
                FindingCategory::Cache,
                "Lake artifact cache",
                if present {
                    format!("selected cache directory exists at {}", path.display())
                } else {
                    format!(
                        "selected cache directory has not been created at {}",
                        path.display()
                    )
                },
            )
            .with_evidence("path", json!(path.display().to_string()))
            .with_evidence("present", json!(present)),
        );
        EnvironmentArtifactCache {
            path: path.display().to_string(),
            present,
        }
    });

    let registry = inspect_environment_registry(context, &mut findings);
    let workspace = inspect_environment_workspace(context, &mut findings);
    let managed_local_workspace = discovered.as_ref().map(|project| {
        let managed = local_workspace::is_managed(&context.cache, project);
        findings.push(
            Finding::passed(
                "workspace.managed_local",
                FindingCategory::Workspace,
                "Managed local workspace",
                if managed {
                    format!("{} is managed by Lev", project.root.display())
                } else {
                    format!("{} is a user-owned project", project.root.display())
                },
            )
            .with_evidence("managed", json!(managed))
            .with_evidence("project", json!(project.root.display().to_string())),
        );
        managed
    });

    let summary = FindingSummary::from_findings(&findings);
    EnvironmentInspectionReport {
        lev_version: env!("CARGO_PKG_VERSION").to_owned(),
        host: crate::core::platform::host_id(),
        locations,
        versions,
        project,
        runtime,
        artifact_cache,
        registry,
        workspace,
        managed_local_workspace,
        findings,
        summary,
    }
}

fn record_version(
    findings: &mut Vec<Finding>,
    mut command: Command,
    id: &str,
    title: &str,
    optional: bool,
) -> Option<String> {
    match command_version(&mut command) {
        Ok(version) => {
            findings.push(
                Finding::passed(
                    id,
                    FindingCategory::Environment,
                    format!("{title} version"),
                    version.clone(),
                )
                .with_evidence("version", json!(version)),
            );
            Some(version)
        }
        Err(error) if optional => {
            findings.push(Finding::not_applicable(
                id,
                FindingCategory::Environment,
                format!("{title} version"),
                format!(
                    "{title} is not installed; it is optional for Lev-managed toolchains: {error:#}"
                ),
            ));
            None
        }
        Err(error) => {
            findings.push(
                Finding::error(
                    id,
                    FindingCategory::Environment,
                    format!("{title} version"),
                    format!("{error:#}"),
                )
                .with_remediation(format!(
                    "install {title} or correct its configured executable"
                )),
            );
            None
        }
    }
}

fn command_version(command: &mut Command) -> Result<String> {
    let version = output_text(checked_output(command)?);
    if version.is_empty() {
        bail!("version command returned no text");
    }
    Ok(version)
}

fn inspect_environment_project(
    project: &Project,
    findings: &mut Vec<Finding>,
) -> EnvironmentProject {
    findings.push(
        Finding::passed(
            "project.discovery",
            FindingCategory::Project,
            "Project discovery",
            format!("found {}", project.root.display()),
        )
        .with_evidence("root", json!(project.root.display().to_string())),
    );
    findings.push(
        Finding::passed(
            "project.toolchain_file",
            FindingCategory::Configuration,
            "lean-toolchain",
            format!("selected {}", project.toolchain),
        )
        .with_evidence(
            "path",
            json!(project.root.join("lean-toolchain").display().to_string()),
        )
        .with_evidence("toolchain", json!(project.toolchain)),
    );

    let toml = project.root.join("lakefile.toml");
    let lean = project.root.join("lakefile.lean");
    let lakefile_path = match (toml.is_file(), lean.is_file()) {
        (true, false) => {
            match lakefile::dependency_names(&toml) {
                Ok(dependencies) => findings.push(
                    Finding::passed(
                        "project.lakefile",
                        FindingCategory::Configuration,
                        "Lakefile",
                        format!(
                            "parsed {} with {} direct dependencies",
                            toml.display(),
                            dependencies.len()
                        ),
                    )
                    .with_evidence("path", json!(toml.display().to_string()))
                    .with_evidence("direct_dependencies", json!(dependencies.len())),
                ),
                Err(error) => findings.push(
                    Finding::error(
                        "project.lakefile",
                        FindingCategory::Configuration,
                        "Lakefile",
                        format!("{error:#}"),
                    )
                    .with_evidence("path", json!(toml.display().to_string())),
                ),
            }
            Some(toml.display().to_string())
        }
        (false, true) => {
            findings.push(
                Finding::passed(
                    "project.lakefile",
                    FindingCategory::Configuration,
                    "Lakefile",
                    format!("found executable {}", lean.display()),
                )
                .with_evidence("path", json!(lean.display().to_string())),
            );
            Some(lean.display().to_string())
        }
        (true, true) => {
            findings.push(
                Finding::error(
                    "project.lakefile",
                    FindingCategory::Configuration,
                    "Lakefile",
                    format!("both {} and {} exist", toml.display(), lean.display()),
                )
                .with_remediation("keep only the Lakefile the project uses"),
            );
            Some(toml.display().to_string())
        }
        (false, false) => {
            findings.push(
                Finding::error(
                    "project.lakefile",
                    FindingCategory::Configuration,
                    "Lakefile",
                    "project has neither lakefile.toml nor lakefile.lean",
                )
                .with_remediation("add the Lakefile used by this project"),
            );
            None
        }
    };

    let manifest_path = project.manifest_path();
    if manifest_path.is_file() {
        match LakeManifest::read(&manifest_path) {
            Ok(manifest) => findings.push(
                Finding::passed(
                    "project.manifest",
                    FindingCategory::Dependency,
                    "Lake manifest",
                    format!(
                        "parsed {} with {} packages",
                        manifest_path.display(),
                        manifest.packages.len()
                    ),
                )
                .with_evidence("path", json!(manifest_path.display().to_string()))
                .with_evidence("packages", json!(manifest.packages.len())),
            ),
            Err(error) => findings.push(
                Finding::error(
                    "project.manifest",
                    FindingCategory::Dependency,
                    "Lake manifest",
                    format!("{error:#}"),
                )
                .with_evidence("path", json!(manifest_path.display().to_string())),
            ),
        }
    } else {
        findings.push(
            Finding::warning(
                "project.manifest",
                FindingCategory::Dependency,
                "Lake manifest",
                format!("{} is missing", manifest_path.display()),
            )
            .with_remediation("run `lev sync` to create the Lake manifest"),
        );
    }

    let lock_path = project.lock_path();
    if lock_path.is_file() {
        match lockfile::verify(project) {
            Ok(()) => findings.push(
                Finding::passed(
                    "project.lock",
                    FindingCategory::Dependency,
                    "Lev lock",
                    format!("{} matches the project", lock_path.display()),
                )
                .with_evidence("path", json!(lock_path.display().to_string())),
            ),
            Err(error) => findings.push(
                Finding::error(
                    "project.lock",
                    FindingCategory::Dependency,
                    "Lev lock",
                    format!("{error:#}"),
                )
                .with_remediation("run `lev lock` after reviewing project changes"),
            ),
        }
    } else {
        findings.push(
            Finding::warning(
                "project.lock",
                FindingCategory::Dependency,
                "Lev lock",
                format!("{} is missing", lock_path.display()),
            )
            .with_remediation("run `lev lock`"),
        );
    }

    let config_path = project.root.join("lev.toml");
    match LevConfig::read(&project.root) {
        Ok(config) => findings.push(
            Finding::passed(
                "project.lev_config",
                FindingCategory::Configuration,
                "Lev configuration",
                config.path.as_ref().map_or_else(
                    || "no lev.toml; Lake defaults are active".to_owned(),
                    |path| format!("parsed {}", path.display()),
                ),
            )
            .with_evidence("tasks", json!(config.tasks.len()))
            .with_evidence("matrix_toolchains", json!(config.matrix_toolchains.len())),
        ),
        Err(error) => findings.push(
            Finding::error(
                "project.lev_config",
                FindingCategory::Configuration,
                "Lev configuration",
                format!("{error:#}"),
            )
            .with_evidence("path", json!(config_path.display().to_string())),
        ),
    }

    EnvironmentProject {
        root: project.root.display().to_string(),
        toolchain: project.toolchain.clone(),
        toolchain_file: project.root.join("lean-toolchain").display().to_string(),
        lakefile_path,
        manifest_path: manifest_path.display().to_string(),
        lock_path: lock_path.display().to_string(),
        config_path: config_path
            .is_file()
            .then(|| config_path.display().to_string()),
    }
}

fn inspect_environment_runtime(
    context: &AppContext,
    project: &Project,
    versions: &mut EnvironmentVersions,
    findings: &mut Vec<Finding>,
) -> Option<EnvironmentRuntime> {
    let (source, path) = match context.store.find(&project.toolchain) {
        Ok(Some(stored)) => ("lev_store", Some(stored.view)),
        Ok(None) => ("elan", None),
        Err(error) => {
            findings.push(Finding::error(
                "environment.runtime.selected",
                FindingCategory::Environment,
                "Selected runtime",
                format!("could not inspect Lev's toolchain store: {error:#}"),
            ));
            findings.push(Finding::not_applicable(
                "environment.lean.version",
                FindingCategory::Environment,
                "Lean version",
                "the selected runtime could not be resolved",
            ));
            findings.push(Finding::not_applicable(
                "environment.lake.version",
                FindingCategory::Environment,
                "Lake version",
                "the selected runtime could not be resolved",
            ));
            return None;
        }
    };
    let detail = path.as_ref().map_or_else(
        || format!("{} will run through elan", project.toolchain),
        |path| {
            format!(
                "{} will run from Lev's store at {}",
                project.toolchain,
                path.display()
            )
        },
    );
    let mut selected = Finding::passed(
        "environment.runtime.selected",
        FindingCategory::Environment,
        "Selected runtime",
        detail,
    )
    .with_evidence("source", json!(source))
    .with_evidence("toolchain", json!(project.toolchain));
    if let Some(path) = &path {
        selected = selected.with_evidence("path", json!(path.display().to_string()));
    }
    findings.push(selected);

    versions.lean = record_runtime_version(
        context,
        project,
        OsStr::new("lean"),
        "environment.lean.version",
        "Lean",
        findings,
    );
    versions.lake = record_runtime_version(
        context,
        project,
        OsStr::new("lake"),
        "environment.lake.version",
        "Lake",
        findings,
    );

    Some(EnvironmentRuntime {
        toolchain: project.toolchain.clone(),
        source: source.to_owned(),
        path: path.map(|path| path.display().to_string()),
    })
}

fn record_runtime_version(
    context: &AppContext,
    project: &Project,
    program: &OsStr,
    id: &str,
    title: &str,
    findings: &mut Vec<Finding>,
) -> Option<String> {
    let mut command = match context.runtime_command(&project.toolchain, program, false) {
        Ok(command) => command,
        Err(error) => {
            findings.push(Finding::error(
                id,
                FindingCategory::Environment,
                format!("{title} version"),
                format!("{error:#}"),
            ));
            return None;
        }
    };
    command.arg("--version");
    record_version(findings, command, id, title, false)
}

fn inspect_environment_registry(
    context: &AppContext,
    findings: &mut Vec<Finding>,
) -> EnvironmentRegistry {
    let path = context.cache.registry_root();
    match registry::load(&context.cache) {
        Ok(records) => {
            let stale = records
                .iter()
                .filter(|(_, record)| registry_record_is_stale(record))
                .count();
            findings.push(
                Finding::passed(
                    "cache.registry.records",
                    FindingCategory::Cache,
                    "Project registry",
                    format!("parsed {} project records", records.len()),
                )
                .with_evidence("path", json!(path.display().to_string()))
                .with_evidence("records", json!(records.len())),
            );
            let stale_finding = if stale == 0 {
                Finding::passed(
                    "cache.registry.staleness",
                    FindingCategory::Cache,
                    "Project registry staleness",
                    "all parsed project records still match their project",
                )
            } else {
                Finding::warning(
                    "cache.registry.staleness",
                    FindingCategory::Cache,
                    "Project registry staleness",
                    format!("{stale} project records no longer match their project"),
                )
                .with_remediation("run `lev cache gc` to review stale cache records")
            };
            findings.push(
                stale_finding
                    .with_evidence("records", json!(records.len()))
                    .with_evidence("stale_records", json!(stale)),
            );
            EnvironmentRegistry {
                path: path.display().to_string(),
                parsed: true,
                records: Some(records.len()),
                stale_records: Some(stale),
            }
        }
        Err(error) => {
            findings.push(
                Finding::error(
                    "cache.registry.records",
                    FindingCategory::Cache,
                    "Project registry",
                    format!("{error:#}"),
                )
                .with_evidence("path", json!(path.display().to_string())),
            );
            findings.push(Finding::not_applicable(
                "cache.registry.staleness",
                FindingCategory::Cache,
                "Project registry staleness",
                "staleness cannot be checked until every registry record parses",
            ));
            EnvironmentRegistry {
                path: path.display().to_string(),
                parsed: false,
                records: None,
                stale_records: None,
            }
        }
    }
}

fn registry_record_is_stale(record: &registry::ProjectRecord) -> bool {
    let project = match Project::load(record.root.clone()) {
        Ok(project) if project.toolchain == record.toolchain => project,
        _ => return true,
    };
    match &record.manifest_digest {
        Some(expected) => fs::read(project.manifest_path())
            .map(|bytes| cache_digest(&bytes) != *expected)
            .unwrap_or(true),
        None => false,
    }
}

fn inspect_environment_workspace(
    context: &AppContext,
    findings: &mut Vec<Finding>,
) -> EnvironmentWorkspace {
    let config_path = match configured_workspace_path(&context.project_start) {
        Ok(Some(path)) => path,
        Ok(None) => {
            for (id, title) in [
                ("workspace.configuration", "Workspace configuration"),
                ("workspace.validity", "Workspace validity"),
                ("workspace.lock", "Workspace lock"),
            ] {
                findings.push(Finding::not_applicable(
                    id,
                    FindingCategory::Workspace,
                    title,
                    "no ancestor lev.toml declares [workspace]",
                ));
            }
            return EnvironmentWorkspace {
                configured: false,
                config_path: None,
                root: None,
                valid: None,
                locked: None,
                members: None,
            };
        }
        Err((path, error)) => {
            findings.push(
                Finding::error(
                    "workspace.configuration",
                    FindingCategory::Workspace,
                    "Workspace configuration",
                    format!("{error:#}"),
                )
                .with_evidence("path", json!(path.display().to_string())),
            );
            findings.push(Finding::not_applicable(
                "workspace.validity",
                FindingCategory::Workspace,
                "Workspace validity",
                "the candidate workspace configuration did not parse",
            ));
            findings.push(Finding::not_applicable(
                "workspace.lock",
                FindingCategory::Workspace,
                "Workspace lock",
                "the candidate workspace configuration did not parse",
            ));
            return EnvironmentWorkspace {
                configured: true,
                config_path: Some(path.display().to_string()),
                root: path.parent().map(|root| root.display().to_string()),
                valid: Some(false),
                locked: None,
                members: None,
            };
        }
    };

    findings.push(
        Finding::passed(
            "workspace.configuration",
            FindingCategory::Workspace,
            "Workspace configuration",
            format!("found {}", config_path.display()),
        )
        .with_evidence("path", json!(config_path.display().to_string())),
    );
    match ProjectWorkspace::discover(&context.project_start) {
        Ok(workspace) => {
            findings.push(
                Finding::passed(
                    "workspace.validity",
                    FindingCategory::Workspace,
                    "Workspace validity",
                    format!(
                        "{} expands to {} projects",
                        workspace.root.display(),
                        workspace.members.len()
                    ),
                )
                .with_evidence("members", json!(workspace.members.len()))
                .with_evidence("root", json!(workspace.root.display().to_string())),
            );
            let lock_path = workspace.lock_path();
            let locked = if !lock_path.is_file() {
                findings.push(
                    Finding::warning(
                        "workspace.lock",
                        FindingCategory::Workspace,
                        "Workspace lock",
                        format!("{} is missing", lock_path.display()),
                    )
                    .with_remediation("run `lev workspace lock`"),
                );
                false
            } else {
                match workspace.verify_lock() {
                    Ok(()) => {
                        findings.push(
                            Finding::passed(
                                "workspace.lock",
                                FindingCategory::Workspace,
                                "Workspace lock",
                                format!("{} matches every member", lock_path.display()),
                            )
                            .with_evidence("path", json!(lock_path.display().to_string())),
                        );
                        true
                    }
                    Err(error) => {
                        findings.push(
                            Finding::error(
                                "workspace.lock",
                                FindingCategory::Workspace,
                                "Workspace lock",
                                format!("{error:#}"),
                            )
                            .with_remediation("run `lev workspace lock` after reviewing changes"),
                        );
                        false
                    }
                }
            };
            EnvironmentWorkspace {
                configured: true,
                config_path: Some(config_path.display().to_string()),
                root: Some(workspace.root.display().to_string()),
                valid: Some(true),
                locked: Some(locked),
                members: Some(workspace.members.len()),
            }
        }
        Err(error) => {
            findings.push(
                Finding::error(
                    "workspace.validity",
                    FindingCategory::Workspace,
                    "Workspace validity",
                    format!("{error:#}"),
                )
                .with_remediation("fix workspace.members and workspace.exclude in lev.toml"),
            );
            findings.push(Finding::not_applicable(
                "workspace.lock",
                FindingCategory::Workspace,
                "Workspace lock",
                "workspace membership is invalid",
            ));
            EnvironmentWorkspace {
                configured: true,
                config_path: Some(config_path.display().to_string()),
                root: config_path.parent().map(|root| root.display().to_string()),
                valid: Some(false),
                locked: None,
                members: None,
            }
        }
    }
}

fn configured_workspace_path(
    start: &Path,
) -> std::result::Result<Option<PathBuf>, (PathBuf, anyhow::Error)> {
    let mut current = start.to_owned();
    if current.is_file() {
        current.pop();
    }
    loop {
        let path = current.join("lev.toml");
        if path.is_file() {
            let config = LevConfig::read(&current).map_err(|error| (path.clone(), error))?;
            if !config.workspace_members.is_empty() {
                return Ok(Some(path));
            }
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

fn render_environment_report(report: &EnvironmentInspectionReport) -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "Lev {} environment\nHost: {}\n\n",
        report.lev_version, report.host
    ));
    text.push_str("Locations\n");
    text.push_str(&format!(
        "  project start: {}\n",
        report.locations.start_path
    ));
    text.push_str(&format!("  lev cache: {}\n", report.locations.cache_path));
    text.push_str(&format!(
        "  toolchain data: {}\n\n",
        report.locations.toolchain_data_path
    ));

    text.push_str("Tools\n");
    text.push_str(&format!(
        "  Git: {}\n",
        report.versions.git.as_deref().unwrap_or("unavailable")
    ));
    text.push_str(&format!(
        "  elan: {}\n",
        report
            .versions
            .elan
            .as_deref()
            .unwrap_or("not installed (optional for lev-managed toolchains)")
    ));
    match &report.project {
        Some(project) => {
            text.push_str(&format!("  project: {}\n", project.root));
            text.push_str(&format!("  toolchain: {}\n", project.toolchain));
        }
        None => text.push_str("  project: none selected\n"),
    }
    if let Some(runtime) = &report.runtime {
        match (&*runtime.source, runtime.path.as_deref()) {
            ("lev_store", Some(path)) => {
                text.push_str(&format!("  runtime: lev store ({path})\n"));
            }
            _ => text.push_str(&format!("  runtime: {}\n", runtime.source)),
        }
    } else {
        text.push_str("  runtime: unavailable\n");
    }
    text.push_str(&format!(
        "  Lean: {}\n",
        report.versions.lean.as_deref().unwrap_or("unavailable")
    ));
    text.push_str(&format!(
        "  Lake: {}\n\n",
        report.versions.lake.as_deref().unwrap_or("unavailable")
    ));

    text.push_str("Local state\n");
    text.push_str(&format!(
        "  registry: {}{}",
        report.registry.path,
        if report.registry.parsed {
            format!(
                " ({} records, {} stale)",
                report.registry.records.unwrap_or(0),
                report.registry.stale_records.unwrap_or(0)
            )
        } else {
            " (could not parse)".to_owned()
        }
    ));
    text.push('\n');
    if let Some(cache) = &report.artifact_cache {
        text.push_str(&format!(
            "  Lake artifact cache: {} ({})\n",
            cache.path,
            if cache.present {
                "present"
            } else {
                "not created"
            }
        ));
    } else {
        text.push_str("  Lake artifact cache: not selected\n");
    }
    if report.workspace.configured {
        text.push_str(&format!(
            "  workspace: {} (valid: {}, locked: {})\n",
            report.workspace.root.as_deref().unwrap_or("invalid"),
            report
                .workspace
                .valid
                .map_or("unknown", |valid| if valid { "yes" } else { "no" }),
            report
                .workspace
                .locked
                .map_or("unknown", |locked| if locked { "yes" } else { "no" })
        ));
    } else {
        text.push_str("  workspace: not configured\n");
    }
    text.push_str(&format!(
        "  managed local workspace: {}\n\n",
        report
            .managed_local_workspace
            .map_or("not applicable", |managed| if managed {
                "yes"
            } else {
                "no"
            })
    ));

    text.push_str("Findings\n");
    for finding in &report.findings {
        let marker = match (finding.state, finding.severity) {
            (FindingState::Passed, _) => "PASS",
            (FindingState::NotApplicable, _) => "N/A ",
            (FindingState::Raised, FindingSeverity::Warning) => "WARN",
            (FindingState::Raised, FindingSeverity::Error) => "FAIL",
            (FindingState::Raised, FindingSeverity::Info) => "INFO",
        };
        text.push_str(&format!(
            "  {marker}  {:<32} {}\n",
            finding.id, finding.detail
        ));
    }
    text.push_str(&format!(
        "\nSummary: {} passed, {} warnings, {} errors, {} not applicable\n",
        report.summary.passed,
        report.summary.warnings,
        report.summary.errors,
        report.summary.not_applicable
    ));
    text
}

fn inspect_project(context: &AppContext) -> ProjectInspectionReport {
    let mut checks = Vec::new();
    let project = match Project::discover(&context.project_start) {
        Ok(project) => {
            push_check(
                &mut checks,
                "project",
                DiagnosticStatus::Passed,
                format!("found {}", project.root.display()),
            );
            Some(inspect_discovered_project(context, &project, &mut checks))
        }
        Err(error) => {
            let status = if nearest_toolchain_file(&context.project_start).is_some() {
                DiagnosticStatus::Error
            } else {
                DiagnosticStatus::Warning
            };
            push_check(&mut checks, "project", status, format!("{error:#}"));
            None
        }
    };
    let summary = summarize_checks(&checks);
    let commands = if project.is_some() {
        vec![
            "lev inspect build".to_owned(),
            "lev inspect dependencies".to_owned(),
            "lev inspect performance --files".to_owned(),
            "lev inspect imports --coverage".to_owned(),
            "lev inspect trust".to_owned(),
            "lev check".to_owned(),
        ]
    } else {
        vec!["lev init PATH --lean TOOLCHAIN".to_owned()]
    };
    ProjectInspectionReport {
        lev_version: env!("CARGO_PKG_VERSION").to_owned(),
        host: crate::core::platform::host_id(),
        start_path: context.project_start.display().to_string(),
        cache_path: context.cache.root.display().to_string(),
        toolchain_data_path: context.store.root.display().to_string(),
        project,
        checks,
        summary,
        commands,
    }
}

fn inspect_discovered_project(
    context: &AppContext,
    project: &Project,
    checks: &mut Vec<DiagnosticCheck>,
) -> ProjectDiagnosis {
    let toml = project.root.join("lakefile.toml");
    let lean = project.root.join("lakefile.lean");
    let (lakefile_kind, lakefile_path) = match (toml.is_file(), lean.is_file()) {
        (true, false) => {
            match lakefile::dependency_names(&toml) {
                Ok(dependencies) => push_check(
                    checks,
                    "lakefile",
                    DiagnosticStatus::Passed,
                    format!(
                        "parsed {} ({} direct dependencies)",
                        toml.display(),
                        dependencies.len()
                    ),
                ),
                Err(error) => push_check(
                    checks,
                    "lakefile",
                    DiagnosticStatus::Error,
                    format!("{error:#}"),
                ),
            }
            (Some("toml".to_owned()), Some(toml.display().to_string()))
        }
        (false, true) => {
            push_check(
                checks,
                "lakefile",
                DiagnosticStatus::Passed,
                format!("found executable {}", lean.display()),
            );
            (Some("lean".to_owned()), Some(lean.display().to_string()))
        }
        (true, true) => {
            push_check(
                checks,
                "lakefile",
                DiagnosticStatus::Error,
                format!(
                    "both {} and {} exist; keep the Lakefile the project uses",
                    toml.display(),
                    lean.display()
                ),
            );
            (Some("toml".to_owned()), Some(toml.display().to_string()))
        }
        (false, false) => {
            push_check(
                checks,
                "lakefile",
                DiagnosticStatus::Error,
                "project has neither lakefile.toml nor lakefile.lean",
            );
            (None, None)
        }
    };

    let manifest_path = project.manifest_path();
    let manifest_packages = if manifest_path.is_file() {
        match LakeManifest::read(&manifest_path) {
            Ok(manifest) => {
                let count = manifest.packages.len();
                push_check(
                    checks,
                    "manifest",
                    DiagnosticStatus::Passed,
                    format!("parsed {} ({count} packages)", manifest_path.display()),
                );
                Some(count)
            }
            Err(error) => {
                push_check(
                    checks,
                    "manifest",
                    DiagnosticStatus::Error,
                    format!("{error:#}"),
                );
                None
            }
        }
    } else {
        push_check(
            checks,
            "manifest",
            DiagnosticStatus::Warning,
            format!(
                "{} is missing; run `lev sync` to create it",
                manifest_path.display()
            ),
        );
        None
    };

    let lock_path = project.lock_path();
    if lock_path.is_file() {
        match lockfile::verify(project) {
            Ok(()) => push_check(
                checks,
                "lock",
                DiagnosticStatus::Passed,
                format!("{} matches the project", lock_path.display()),
            ),
            Err(error) => push_check(
                checks,
                "lock",
                DiagnosticStatus::Error,
                format!("{error:#}"),
            ),
        }
    } else {
        push_check(
            checks,
            "lock",
            DiagnosticStatus::Warning,
            format!("{} is missing; run `lev lock`", lock_path.display()),
        );
    }

    let config_path = project.root.join("lev.toml");
    let (lev_config_path, configured_tasks, configured_checks) =
        match LevConfig::read(&project.root) {
            Ok(config) => {
                let check_count = config.check.tasks.len()
                    + usize::from(config.check.imports)
                    + config.check.denied_axioms.len()
                    + usize::from(config.check.deny_unsafe);
                let detail = if config.path.is_some() {
                    format!(
                        "parsed {} ({} tasks, {check_count} checks)",
                        config_path.display(),
                        config.tasks.len()
                    )
                } else {
                    "no lev.toml; ordinary Lake defaults are active".to_owned()
                };
                push_check(checks, "lev.toml", DiagnosticStatus::Passed, detail);
                (
                    config.path.map(|path| path.display().to_string()),
                    Some(config.tasks.len()),
                    Some(check_count),
                )
            }
            Err(error) => {
                push_check(
                    checks,
                    "lev.toml",
                    DiagnosticStatus::Error,
                    format!("{error:#}"),
                );
                (
                    config_path
                        .is_file()
                        .then(|| config_path.display().to_string()),
                    None,
                    None,
                )
            }
        };

    let runtime_source = match context.store.find(&project.toolchain) {
        Ok(Some(stored)) => {
            push_check(
                checks,
                "toolchain",
                DiagnosticStatus::Passed,
                format!(
                    "{} is available from lev's store at {}",
                    project.toolchain,
                    stored.view.display()
                ),
            );
            "lev_store".to_owned()
        }
        Ok(None) => match super::toolchain_commands::ensure_toolchain_name(
            context,
            &project.toolchain,
            false,
        ) {
            Ok(()) => {
                push_check(
                    checks,
                    "toolchain",
                    DiagnosticStatus::Passed,
                    format!("{} is available through elan", project.toolchain),
                );
                "elan".to_owned()
            }
            Err(error) => {
                push_check(
                    checks,
                    "toolchain",
                    DiagnosticStatus::Error,
                    format!("{} is unavailable: {error:#}", project.toolchain),
                );
                "unavailable".to_owned()
            }
        },
        Err(error) => {
            push_check(
                checks,
                "toolchain",
                DiagnosticStatus::Error,
                format!("could not inspect lev's toolchain store: {error:#}"),
            );
            "unavailable".to_owned()
        }
    };

    ProjectDiagnosis {
        root: project.root.display().to_string(),
        toolchain: project.toolchain.clone(),
        lakefile_kind,
        lakefile_path,
        manifest_path: manifest_path.display().to_string(),
        manifest_packages,
        lev_config_path,
        configured_tasks,
        configured_checks,
        build_metadata_present: project.root.join(".lake/build").is_dir(),
        runtime_source,
    }
}

fn nearest_toolchain_file(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_owned();
    if current.is_file() {
        current.pop();
    }
    loop {
        let candidate = current.join("lean-toolchain");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn push_check(
    checks: &mut Vec<DiagnosticCheck>,
    name: impl Into<String>,
    status: DiagnosticStatus,
    detail: impl Into<String>,
) {
    let name = name.into();
    let (id, category) = match name.as_str() {
        "project" => ("project.discovery", FindingCategory::Project),
        "lakefile" => ("project.lakefile", FindingCategory::Configuration),
        "manifest" => ("project.manifest", FindingCategory::Dependency),
        "lock" => ("project.lock", FindingCategory::Dependency),
        "lev.toml" => ("project.lev_config", FindingCategory::Configuration),
        "toolchain" => ("environment.runtime.selected", FindingCategory::Environment),
        _ => ("project.inspection", FindingCategory::Project),
    };
    let detail = detail.into();
    let finding = match status {
        DiagnosticStatus::Passed => Finding::passed(id, category, name.clone(), detail),
        DiagnosticStatus::Warning => Finding::warning(id, category, name.clone(), detail),
        DiagnosticStatus::Error => Finding::error(id, category, name.clone(), detail),
    };
    checks.push(DiagnosticCheck::from_finding(name, finding));
}

fn summarize_checks(checks: &[DiagnosticCheck]) -> DiagnosticSummary {
    let mut summary = DiagnosticSummary::default();
    for check in checks {
        match check.status {
            DiagnosticStatus::Passed => summary.passed += 1,
            DiagnosticStatus::Warning => summary.warnings += 1,
            DiagnosticStatus::Error => summary.errors += 1,
        }
    }
    summary
}

fn render_project_report(report: &ProjectInspectionReport) -> String {
    let mut text = format!(
        "Lev {} diagnosis\nhost: {}\nstart: {}\ncache: {}\ntoolchains: {}\n",
        report.lev_version,
        report.host,
        report.start_path,
        report.cache_path,
        report.toolchain_data_path
    );
    if let Some(project) = &report.project {
        text.push_str(&format!(
            "\nProject\n  root: {}\n  toolchain: {}\n",
            project.root, project.toolchain
        ));
        match (&project.lakefile_kind, &project.lakefile_path) {
            (Some(kind), Some(path)) => {
                text.push_str(&format!("  Lakefile: {path} ({kind})\n"));
            }
            _ => text.push_str("  Lakefile: missing\n"),
        }
        match project.manifest_packages {
            Some(packages) => text.push_str(&format!(
                "  manifest: {} ({packages} packages)\n",
                project.manifest_path
            )),
            None => text.push_str(&format!("  manifest: {}\n", project.manifest_path)),
        }
        text.push_str(&format!(
            "  lev.toml: {}\n  configured tasks/checks: {}/{}\n  build metadata: {}\n  runtime: {}\n",
            project.lev_config_path.as_deref().unwrap_or("absent"),
            project
                .configured_tasks
                .map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
            project
                .configured_checks
                .map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
            if project.build_metadata_present {
                "present"
            } else {
                "absent"
            },
            project.runtime_source
        ));
    } else {
        text.push_str("\nProject\n  none found\n");
    }
    text.push_str("\nChecks\n");
    for check in &report.checks {
        let status = match check.status {
            DiagnosticStatus::Passed => "PASS",
            DiagnosticStatus::Warning => "WARN",
            DiagnosticStatus::Error => "ERROR",
        };
        text.push_str(&format!(
            "  {status:<5} {:<10} {}\n",
            check.name, check.detail
        ));
    }
    text.push_str(&format!(
        "\nSummary: {} passed, {} warnings, {} errors\n\nNext\n",
        report.summary.passed, report.summary.warnings, report.summary.errors
    ));
    for command in &report.commands {
        text.push_str(&format!("  {command}\n"));
    }
    text
}

/// Run Lake verbosely and compare its structured traces before and after.
pub(super) fn build(context: &AppContext, args: InspectBuildArgs) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;
    let before = TraceSnapshot::capture(&project.root)?;
    let mut command = vec![
        OsString::from("lake"),
        OsString::from("--verbose"),
        OsString::from("build"),
    ];
    command.extend(args.args.clone());

    let started = Instant::now();
    let output = captured_in_project(context, &project, &command, !args.execution.offline)?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let (trace_changes, current_warnings, trace_capture_warning) = match TraceSnapshot::capture(
        &project.root,
    ) {
        Ok(current) => (before.changes_since(&current), current.warnings, None),
        Err(error) => (
            Vec::new(),
            Vec::new(),
            Some(format!(
                "could not capture Lake traces after the build; trace-based causality evidence is unavailable: {error:#}"
            )),
        ),
    };
    let trace_capture_succeeded = trace_capture_warning.is_none();
    let events = build_events(&output);
    let diagnostics = build_diagnostics::parse(&output.stdout, &output.stderr);
    let mut causality = build_causality_report(&project, &events, &trace_changes);
    if let Some(warning) = trace_capture_warning {
        causality.warnings.push(warning);
    }
    let attributed = trace_changes
        .iter()
        .filter_map(|change| change.target.as_deref())
        .collect::<BTreeSet<_>>();
    let unattributed_work = if trace_capture_succeeded {
        events
            .iter()
            .filter(|event| event.action == BuildEventAction::Built)
            .filter(|event| !attributed.contains(event.target.as_str()))
            .map(|event| event.target.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let code = exit_code(output.status);
    let warnings = before
        .warnings
        .into_iter()
        .chain(current_warnings)
        .chain(causality.warnings.iter().cloned())
        .collect();
    let report = BuildInspectionReport {
        project: source.root.display().to_string(),
        toolchain: project.toolchain.clone(),
        command: display_os_command(&command),
        status: if code == 0 {
            RunStatus::Passed
        } else {
            RunStatus::Failed
        },
        exit_code: code,
        elapsed_ms,
        built: count_events(&events, BuildEventAction::Built),
        replayed: count_events(&events, BuildEventAction::Replayed),
        ran: count_events(&events, BuildEventAction::Ran),
        reused: count_events(&events, BuildEventAction::Reused),
        fetched: count_events(&events, BuildEventAction::Fetched),
        unpacked: count_events(&events, BuildEventAction::Unpacked),
        events,
        root_causes: diagnostics.root_causes,
        failed_targets: diagnostics.failed_targets,
        trace_changes,
        unattributed_work,
        causality,
        warnings,
    };

    relay_output(&output, true)?;
    emit_build_report(&args, &report)?;
    Ok(code)
}

/// Explain selected revisions using every available materialized parent manifest.
pub(super) fn dependencies(context: &AppContext, args: InspectDependenciesArgs) -> Result<i32> {
    let project = context.project()?;
    let report = pin_analysis::analyze(&project)?;
    let bytes = if args.json {
        json_output::render(schema::INSPECT_DEPENDENCIES, &report)?
    } else {
        render_dependency_report(&report).into_bytes()
    };
    if let Some(path) = &args.output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_write(path, &bytes)?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write dependency inspection")?;
    }
    Ok(0)
}

fn render_dependency_report(report: &PinAnalysis) -> String {
    let aligned = report.count(PinStatus::Aligned);
    let conflict = report.count(PinStatus::Conflict);
    let root_override = report.count(PinStatus::RootOverride);
    let stale = report.count(PinStatus::Stale);
    let unknown = report.count(PinStatus::Unknown);
    let mut text = format!(
        "dependency pins for {}: {aligned} aligned, {conflict} conflict{}, {root_override} root override{}, {stale} stale, {unknown} unknown\n",
        report.root,
        if conflict == 1 { "" } else { "s" },
        if root_override == 1 { "" } else { "s" },
    );
    for package in &report.packages {
        let status = match package.status {
            PinStatus::Aligned => "ALIGNED",
            PinStatus::Conflict => "CONFLICT",
            PinStatus::RootOverride => "OVERRIDE",
            PinStatus::Stale => "STALE",
            PinStatus::Unknown => "UNKNOWN",
        };
        let revision = package
            .selected_revision
            .as_deref()
            .map(short_revision)
            .unwrap_or("-");
        text.push_str(&format!("{status:<8} {revision:<12} {}\n", package.name));
        for expectation in &package.expectations {
            let expected = expectation
                .revision
                .as_deref()
                .map(short_revision)
                .unwrap_or("-");
            let requested = expectation
                .requested
                .as_deref()
                .map(|request| format!(" ({request})"))
                .unwrap_or_default();
            text.push_str(&format!(
                "  requested by {}: {expected}{requested}\n",
                expectation.parent
            ));
        }
    }
    for warning in &report.warnings {
        text.push_str(&format!("warning: {warning}\n"));
    }
    text
}

fn short_revision(revision: &str) -> &str {
    &revision[..revision.len().min(12)]
}

/// Locate a declaration in `.ilean` indexes and ask Lean for its exact type.
pub(super) fn declaration(context: &AppContext, args: InspectDeclarationArgs) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(context, &source, None, false, args.offline)?;

    if args.build {
        let command = vec![OsString::from("lake"), OsString::from("build")];
        let output = captured_in_project(context, &project, &command, !args.offline)?;
        if !output.status.success() {
            relay_output(&output, args.json)?;
            return Ok(exit_code(output.status));
        }
        relay_output(&output, args.json)?;
    }

    let name = import_graph::canonical_lean_name(&args.name)?;
    let (module, candidate, warnings) = match args.from {
        Some(module) => (
            import_graph::canonical_lean_name(&module)?,
            None,
            Vec::new(),
        ),
        None => {
            let search = declaration_index::find(&project, &name)?;
            let candidate = choose_declaration(&name, search.candidates)?;
            (candidate.module.clone(), Some(candidate), search.warnings)
        }
    };
    let checked = check_declaration_type(context, &project, &module, &name, args.offline)?;
    let source_path = candidate
        .as_ref()
        .and_then(|candidate| candidate.source.as_deref())
        .map(|path| source_path(&source, &project, path));
    let report = DeclarationInspectionReport {
        name,
        type_: checked,
        import_statement: format!("import {module}"),
        module,
        package: candidate
            .as_ref()
            .map(|candidate| candidate.package.clone()),
        source: source_path.map(|path| path.display().to_string()),
        range: candidate.and_then(|candidate| candidate.range),
        warnings,
    };
    if args.json {
        json_output::print(schema::INSPECT_DECLARATION, &report)?;
    } else {
        print_declaration(&report);
    }
    Ok(0)
}

/// Ask Lean which axioms and unsafe declarations occur in project metadata.
pub(super) fn trust(context: &AppContext, args: InspectTrustArgs) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;

    if !args.no_build {
        let command = vec![OsString::from("lake"), OsString::from("build")];
        let output = captured_in_project(context, &project, &command, !args.execution.offline)?;
        relay_output(&output, args.json)?;
        if !output.status.success() {
            return Ok(exit_code(output.status));
        }
    }

    let report = scan_trust(
        context,
        &source,
        &project,
        &args.denied_axioms,
        args.deny_unsafe,
        args.no_build,
        args.execution.offline,
    )?;
    let failed = report.failed();

    if args.json {
        json_output::print(schema::INSPECT_TRUST, &report)?;
    } else {
        print_trust(&report);
    }
    Ok(i32::from(failed))
}

/// Inspect already-built declaration metadata and apply one trust policy.
///
/// Callers own the build step. This keeps `lev check` and
/// `lev inspect trust` on the same scanner without rebuilding a project.
pub(super) fn scan_trust(
    context: &AppContext,
    source: &Project,
    project: &Project,
    denied_axioms: &[String],
    deny_unsafe: bool,
    existing_metadata_only: bool,
    offline: bool,
) -> Result<TrustInspectionReport> {
    let inventory = declaration_index::project_declarations(project)?;
    if inventory.declarations.is_empty() {
        let mut message =
            "no readable project declarations were found in built .ilean indexes".to_owned();
        if existing_metadata_only {
            message.push_str("; rerun without `--no-build` to rebuild the indexes");
        } else {
            message.push_str(
                "; Lake completed the build, so check the project targets or run \
                 `lev project clean` and retry",
            );
        }
        if !inventory.warnings.is_empty() {
            message.push_str("; index warnings: ");
            message.push_str(
                &inventory
                    .warnings
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
            if inventory.warnings.len() > 3 {
                message.push_str(&format!(
                    " | {} more warning(s)",
                    inventory.warnings.len() - 3
                ));
            }
        }
        bail!("{message}");
    }
    let denied_axioms = denied_axioms
        .iter()
        .map(|name| import_graph::canonical_lean_name(name))
        .collect::<Result<BTreeSet<_>>>()?;
    let mut records = Vec::new();
    for batch in trust_batches(&inventory.declarations) {
        let declarations = batch.owners.keys().cloned().collect::<Vec<_>>();
        let probe = trust_inspector::render(&batch.modules, &declarations)?;
        for record in run_trust_probe(context, project, &probe, offline)? {
            let module = batch.owners.get(&record.name).with_context(|| {
                format!(
                    "Lean returned a trust record for unrequested declaration {}",
                    record.name
                )
            })?;
            records.push(ScopedTrustRecord {
                module: module.clone(),
                record,
            });
        }
    }
    let report = trust_report(
        source,
        project,
        inventory,
        records,
        denied_axioms,
        deny_unsafe,
    )?;
    Ok(report)
}

impl TrustInspectionReport {
    pub(super) fn failed(&self) -> bool {
        matches!(self.status, RunStatus::Failed)
    }

    pub(super) fn check_detail(&self) -> String {
        format!(
            "{} of {} declarations scanned, {} policy violation(s), {} scan failure(s)",
            self.declarations_scanned,
            self.declarations_indexed,
            self.policy_violations,
            self.scan_failures
        )
    }
}

fn trust_batches(declarations: &[DeclarationCandidate]) -> Vec<TrustBatch> {
    let mut declarations_by_module = BTreeMap::<String, BTreeSet<String>>::new();
    for declaration in declarations {
        declarations_by_module
            .entry(declaration.module.clone())
            .or_default()
            .insert(declaration.name.clone());
    }

    let mut batches = Vec::<TrustBatch>::new();
    for (module, names) in declarations_by_module {
        let batch = batches
            .iter_mut()
            .find(|batch| names.iter().all(|name| !batch.owners.contains_key(name)));
        if let Some(batch) = batch {
            for name in names {
                batch.owners.insert(name, module.clone());
            }
            batch.modules.push(module);
        } else {
            batches.push(TrustBatch {
                modules: vec![module.clone()],
                owners: names
                    .into_iter()
                    .map(|name| (name, module.clone()))
                    .collect(),
            });
        }
    }
    batches
}

fn run_trust_probe(
    context: &AppContext,
    project: &Project,
    source: &str,
    offline: bool,
) -> Result<Vec<TrustRecord>> {
    let mut scratch = tempfile::Builder::new()
        .prefix(".lev-trust-")
        .suffix(".lean")
        .tempfile_in(&project.root)
        .with_context(|| {
            format!(
                "failed to create a temporary Lean file in {}",
                project.root.display()
            )
        })?;
    scratch
        .write_all(source.as_bytes())
        .context("failed to write temporary Lean trust inspection")?;
    scratch
        .as_file_mut()
        .sync_all()
        .context("failed to flush temporary Lean trust inspection")?;
    let command = vec![
        OsString::from("lake"),
        OsString::from("env"),
        OsString::from("lean"),
        scratch.path().as_os_str().to_owned(),
    ];
    let output = captured_in_project(context, project, &command, !offline)?;
    if !output.status.success() {
        relay_output(&output, true)?;
        bail!(
            "Lean could not inspect project trust (exit code {})",
            exit_code(output.status)
        );
    }
    let (records, other_stdout) = trust_inspector::parse_output(&output.stdout)?;
    let mut diagnostics = io::stderr().lock();
    for line in other_stdout {
        writeln!(diagnostics, "{line}").context("failed to relay Lean output")?;
    }
    diagnostics
        .write_all(&output.stderr)
        .context("failed to relay Lean diagnostics")?;
    Ok(records)
}

fn trust_report(
    source: &Project,
    execution: &Project,
    inventory: declaration_index::DeclarationInventory,
    records: Vec<ScopedTrustRecord>,
    denied_axioms: BTreeSet<String>,
    deny_unsafe: bool,
) -> Result<TrustInspectionReport> {
    let mut records_by_name = BTreeMap::new();
    for scoped in records {
        let mut record = scoped.record;
        record.axioms.sort();
        record.axioms.dedup();
        let key = (scoped.module, record.name.clone());
        if records_by_name.insert(key, record).is_some() {
            bail!("Lean returned more than one trust record for the same module declaration");
        }
    }
    let metadata = inventory
        .declarations
        .iter()
        .map(|declaration| {
            (
                (declaration.module.clone(), declaration.name.clone()),
                declaration,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut warnings = inventory.warnings;
    let mut findings = Vec::new();
    let mut declarations_scanned = 0_usize;
    let mut scan_failures = 0_usize;
    let mut declarations_with_axioms = 0_usize;
    let mut unsafe_declarations = 0_usize;

    for (key, declaration) in &metadata {
        let (module, name) = key;
        let Some(record) = records_by_name.remove(key) else {
            scan_failures += 1;
            warnings.push(format!(
                "Lean returned no trust record for {name} from {module}"
            ));
            continue;
        };
        if !record.found {
            scan_failures += 1;
            warnings.push(format!(
                "{name} from {module} was indexed but was not present in Lean's imported environment"
            ));
            continue;
        }
        declarations_scanned += 1;
        declarations_with_axioms += usize::from(!record.axioms.is_empty());
        unsafe_declarations += usize::from(record.is_unsafe);
        if record.axioms.is_empty() && !record.is_unsafe {
            continue;
        }
        let denied = record
            .axioms
            .iter()
            .filter(|axiom| denied_axioms.contains(*axiom))
            .cloned()
            .collect::<Vec<_>>();
        findings.push(TrustDeclarationReport {
            name: record.name,
            module: declaration.module.clone(),
            package: declaration.package.clone(),
            source: declaration
                .source
                .as_deref()
                .map(|path| source_path(source, execution, path).display().to_string()),
            range: declaration.range.clone(),
            axioms: record.axioms,
            is_unsafe: record.is_unsafe,
            denied_axioms: denied,
            unsafe_violation: deny_unsafe && record.is_unsafe,
        });
    }
    for ((module, name), _) in records_by_name {
        warnings.push(format!(
            "Lean returned a trust record for unindexed declaration {name} from {module}"
        ));
    }
    let policy_violations = findings
        .iter()
        .filter(|finding| !finding.denied_axioms.is_empty() || finding.unsafe_violation)
        .count();
    Ok(TrustInspectionReport {
        project: source.root.display().to_string(),
        toolchain: execution.toolchain.clone(),
        status: if scan_failures == 0 && policy_violations == 0 {
            RunStatus::Passed
        } else {
            RunStatus::Failed
        },
        declarations_indexed: metadata.len(),
        declarations_scanned,
        scan_failures,
        declarations_with_axioms,
        unsafe_declarations,
        denied_axioms: denied_axioms.into_iter().collect(),
        deny_unsafe,
        policy_violations,
        findings,
        warnings,
    })
}

fn captured_in_project(
    context: &AppContext,
    project: &Project,
    args: &[OsString],
    install_toolchain: bool,
) -> Result<Output> {
    let _dependency_environment = dependency_env::lock_attached(&context.cache, project)?;
    let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
    registry::record(&context.cache, project)?;
    let mut command = project_command(context, project, args, install_toolchain)?;
    command
        .stdin(Stdio::inherit())
        .output()
        .with_context(|| format!("failed to start {}", display_os_command(args)))
}

fn build_events(output: &Output) -> Vec<BuildEvent> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .filter_map(parse_build_event)
        .collect()
}

fn parse_build_event(line: &str) -> Option<BuildEvent> {
    let (_, detail) = line.trim().split_once("] ")?;
    let (action, detail) = detail.split_once(' ')?;
    let action = match action {
        "Built" => BuildEventAction::Built,
        "Fetched" => BuildEventAction::Fetched,
        "Replayed" => BuildEventAction::Replayed,
        "Reused" => BuildEventAction::Reused,
        "Ran" => BuildEventAction::Ran,
        "Unpacked" => BuildEventAction::Unpacked,
        _ => return None,
    };
    let (target, elapsed_ms) = match detail.rsplit_once(" (") {
        Some((target, duration)) if duration.ends_with(')') => {
            let duration = &duration[..duration.len() - 1];
            match parse_duration_ms(duration) {
                Some(elapsed_ms) => (target, Some(elapsed_ms)),
                None => (detail, None),
            }
        }
        _ => (detail, None),
    };
    Some(BuildEvent {
        action,
        target: target.to_owned(),
        elapsed_ms,
    })
}

fn parse_duration_ms(value: &str) -> Option<f64> {
    for (suffix, multiplier) in [
        ("ms", 1.0),
        ("us", 0.001),
        ("μs", 0.001),
        ("ns", 0.000_001),
        ("s", 1000.0),
    ] {
        if let Some(number) = value.strip_suffix(suffix) {
            return number
                .trim()
                .parse::<f64>()
                .ok()
                .map(|value| value * multiplier);
        }
    }
    None
}

fn count_events(events: &[BuildEvent], action: BuildEventAction) -> usize {
    events.iter().filter(|event| event.action == action).count()
}

fn build_causality_report(
    project: &Project,
    events: &[BuildEvent],
    trace_changes: &[TraceChange],
) -> BuildCausalityReport {
    let (graph, source_graph_available, warnings) =
        match import_graph::ImportGraph::inspect(project, &[], false) {
            Ok(graph) => (graph, true, Vec::new()),
            Err(error) => (
                import_graph::ImportGraph {
                    project: project.root.display().to_string(),
                    nodes: Vec::new(),
                    edges: Vec::new(),
                    cycles: Vec::new(),
                },
                false,
                vec![format!(
                    "could not infer source-level build impact: {error:#}"
                )],
            ),
        };
    let changed_project_modules = changed_source_modules(project, &graph, trace_changes);
    let graph_modules = graph
        .nodes
        .iter()
        .map(|node| node.module.as_str())
        .collect::<BTreeSet<_>>();
    let observed_rebuilt_modules = events
        .iter()
        .filter(|event| event.action == BuildEventAction::Built)
        .filter(|event| graph_modules.contains(event.target.as_str()))
        .map(|event| event.target.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let analysis = RebuildCausality::analyze(&graph, &changed_project_modules, trace_changes);
    let evidence_kinds = analysis.evidence_kinds();

    BuildCausalityReport {
        source_graph_available,
        changed_project_modules,
        observed_rebuilt_modules,
        evidence_kinds,
        analysis,
        warnings,
    }
}

fn changed_source_modules(
    project: &Project,
    graph: &import_graph::ImportGraph,
    trace_changes: &[TraceChange],
) -> Vec<String> {
    let modules_by_path = graph
        .nodes
        .iter()
        .map(|node| (node.path.as_str(), node.module.as_str()))
        .collect::<BTreeMap<_, _>>();
    trace_changes
        .iter()
        .flat_map(|change| &change.inputs)
        .filter(|input| input.kind == TraceInputKind::Source)
        .flat_map(|input| input.input.split(" / "))
        .filter_map(|input| normalized_project_path(&project.root, input))
        .filter_map(|path| modules_by_path.get(path.as_str()).copied())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalized_project_path(project_root: &Path, value: &str) -> Option<String> {
    let path = Path::new(value);
    let relative = if path.is_absolute() {
        path.strip_prefix(project_root).ok()?
    } else {
        path
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn emit_build_report(args: &InspectBuildArgs, report: &BuildInspectionReport) -> Result<()> {
    let bytes = if args.json {
        json_output::render(schema::INSPECT_BUILD, report)?
    } else {
        render_build_report(report).into_bytes()
    };
    if let Some(path) = &args.output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_write(path, &bytes)
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write build inspection")
    }
}

fn render_build_report(report: &BuildInspectionReport) -> String {
    let status = match report.status {
        RunStatus::Passed => "passed",
        RunStatus::Failed => "failed",
    };
    let mut text = format!(
        "build {status} in {:.3} ms: {} built, {} replayed, {} ran, {} reused, {} fetched, {} unpacked\n",
        report.elapsed_ms,
        report.built,
        report.replayed,
        report.ran,
        report.reused,
        report.fetched,
        report.unpacked
    );
    for event in &report.events {
        let action = match event.action {
            BuildEventAction::Built => "BUILT",
            BuildEventAction::Fetched => "FETCHED",
            BuildEventAction::Replayed => "REPLAYED",
            BuildEventAction::Reused => "REUSED",
            BuildEventAction::Ran => "RAN",
            BuildEventAction::Unpacked => "UNPACKED",
        };
        if let Some(elapsed_ms) = event.elapsed_ms {
            text.push_str(&format!(
                "{action:<8} {:>10.3} ms  {}\n",
                elapsed_ms, event.target
            ));
        } else {
            text.push_str(&format!("{action:<8}               {}\n", event.target));
        }
    }
    if !report.root_causes.is_empty() {
        text.push_str("root causes:\n");
        for cause in &report.root_causes {
            match cause.kind {
                BuildRootCauseKind::LeanDiagnostic => {
                    text.push_str(&format!(
                        "  {}:{}:{}: {}\n",
                        cause.file.as_deref().unwrap_or("<unknown>"),
                        cause.line.unwrap_or(0),
                        cause.column.unwrap_or(0),
                        cause.message
                    ));
                }
                BuildRootCauseKind::LakeError => {
                    text.push_str(&format!("  Lake: {}\n", cause.message));
                }
                BuildRootCauseKind::FailedJob => {
                    text.push_str(&format!("  failed job: {}\n", cause.message));
                }
            }
        }
    }
    if !report.failed_targets.is_empty() {
        text.push_str("failed targets:\n");
        for target in &report.failed_targets {
            text.push_str(&format!("  {target}\n"));
        }
    }
    if report.trace_changes.is_empty() {
        text.push_str("trace inputs: no changes recorded\n");
    } else {
        text.push_str("trace inputs:\n");
        for change in &report.trace_changes {
            let status = match change.status {
                TraceChangeStatus::Created => "created",
                TraceChangeStatus::Changed => "changed",
                TraceChangeStatus::Removed => "removed",
            };
            text.push_str(&format!(
                "  {} [{}]: {}\n",
                change.target.as_deref().unwrap_or(&change.trace),
                status,
                change.reason
            ));
            for input in &change.inputs {
                text.push_str(&format!(
                    "    {}: {}\n",
                    input_kind(input.kind),
                    input.input
                ));
            }
        }
    }
    for target in &report.unattributed_work {
        text.push_str(&format!(
            "unattributed: Lake reported work for {target}, but its trace exposed no changed labeled input\n"
        ));
    }
    text.push_str("causality:\n");
    text.push_str(&format!(
        "  changed source modules: {}\n",
        list_or_none(&report.causality.changed_project_modules)
    ));
    text.push_str(&format!(
        "  Lake-reported built modules: {}\n",
        list_or_none(&report.causality.observed_rebuilt_modules)
    ));
    text.push_str(&format!(
        "  potential project impact: {}\n",
        list_or_none(&report.causality.analysis.potential_affected_modules)
    ));
    for impact in &report.causality.analysis.potential_impacts {
        text.push_str(&format!(
            "    could affect: {}\n",
            impact.shortest_path.join(" -> ")
        ));
    }
    for missing in &report.causality.analysis.missing_evidence {
        text.push_str(&format!(
            "  missing evidence for {}: {}\n",
            missing.subject, missing.detail
        ));
    }
    for warning in &report.warnings {
        text.push_str(&format!("warning: {warning}\n"));
    }
    text
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

fn input_kind(kind: TraceInputKind) -> &'static str {
    match kind {
        TraceInputKind::Source => "source",
        TraceInputKind::Dependency => "dependency",
        TraceInputKind::Options => "options",
        TraceInputKind::Toolchain => "toolchain",
        TraceInputKind::Configuration => "configuration",
        TraceInputKind::Other => "other",
    }
}

fn choose_declaration(
    name: &str,
    mut candidates: Vec<DeclarationCandidate>,
) -> Result<DeclarationCandidate> {
    if candidates.is_empty() {
        bail!(
            "declaration {name} was not found in built .ilean indexes; run `lev inspect declaration {name} --build` or pass `--from MODULE`"
        );
    }
    let modules = candidates
        .iter()
        .map(|candidate| candidate.module.as_str())
        .collect::<BTreeSet<_>>();
    if modules.len() > 1 {
        bail!(
            "declaration {name} appears in several modules ({}); select one with `--from MODULE`",
            modules.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    candidates.sort_by_key(|candidate| {
        (
            candidate.source.is_none(),
            candidate.range.is_none(),
            candidate.package.clone(),
        )
    });
    Ok(candidates.remove(0))
}

fn check_declaration_type(
    context: &AppContext,
    project: &Project,
    module: &str,
    name: &str,
    offline: bool,
) -> Result<String> {
    let mut scratch = tempfile::Builder::new()
        .prefix(".lev-inspect-")
        .suffix(".lean")
        .tempfile_in(&project.root)
        .with_context(|| {
            format!(
                "failed to create a temporary Lean file in {}",
                project.root.display()
            )
        })?;
    writeln!(scratch, "import {module}\n#check {name}")
        .context("failed to write temporary Lean inspection file")?;
    scratch
        .as_file_mut()
        .sync_all()
        .context("failed to flush temporary Lean inspection file")?;
    let command = vec![
        OsString::from("lake"),
        OsString::from("env"),
        OsString::from("lean"),
        scratch.path().as_os_str().to_owned(),
        OsString::from("--json"),
    ];
    let output = captured_in_project(context, project, &command, !offline)?;
    if !output.status.success() {
        relay_output(&output, true)?;
        bail!(
            "Lean could not inspect {name} from {module} (exit code {})",
            exit_code(output.status)
        );
    }
    parse_checked_type(&output.stdout).with_context(|| {
        format!(
            "Lean checked {name}, but its structured output did not contain the declaration type"
        )
    })
}

#[derive(Debug, Deserialize)]
struct LeanMessage {
    severity: Option<String>,
    data: serde_json::Value,
}

fn parse_checked_type(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<LeanMessage>(line).ok())
        .filter(|message| message.severity.as_deref() == Some("information"))
        .filter_map(|message| message.data.as_str().map(str::to_owned))
        .filter_map(|message| {
            message
                .split_once(" : ")
                .map(|(_, type_)| type_.trim().to_owned())
        })
        .next_back()
}

fn source_path(source: &Project, execution: &Project, path: &Path) -> PathBuf {
    if execution.root != source.root && path.starts_with(&execution.root) {
        match path.strip_prefix(&execution.root) {
            Ok(relative) => source.root.join(relative),
            Err(_) => path.to_owned(),
        }
    } else {
        path.to_owned()
    }
}

fn print_declaration(report: &DeclarationInspectionReport) {
    println!("{}", report.name);
    println!("  type:    {}", report.type_);
    println!("  module:  {}", report.module);
    println!("  import:  {}", report.import_statement);
    if let Some(package) = &report.package {
        println!("  package: {package}");
    }
    if let Some(source) = &report.source {
        if let Some(range) = &report.range {
            println!(
                "  source:  {source}:{}:{}",
                range.start_line, range.start_column
            );
        } else {
            println!("  source:  {source}");
        }
    }
    for warning in &report.warnings {
        eprintln!("lev: warning: {warning}");
    }
}

pub(super) fn print_trust(report: &TrustInspectionReport) {
    println!(
        "trust: scanned {} of {} indexed declarations; {} with axioms, {} unsafe",
        report.declarations_scanned,
        report.declarations_indexed,
        report.declarations_with_axioms,
        report.unsafe_declarations
    );
    if report.scan_failures > 0 {
        println!(
            "inspection: failed ({} declaration(s) could not be inspected)",
            report.scan_failures
        );
    }
    for finding in &report.findings {
        let label = match (!finding.axioms.is_empty(), finding.is_unsafe) {
            (true, true) => "AXIOMS+UNSAFE",
            (true, false) => "AXIOMS",
            (false, true) => "UNSAFE",
            (false, false) => continue,
        };
        println!("{label:<13} {}", finding.name);
        println!("  module: {}", finding.module);
        if let Some(source) = &finding.source {
            if let Some(range) = &finding.range {
                println!(
                    "  source: {source}:{}:{}",
                    range.start_line, range.start_column
                );
            } else {
                println!("  source: {source}");
            }
        }
        if !finding.axioms.is_empty() {
            println!("  axioms: {}", finding.axioms.join(", "));
        }
        if !finding.denied_axioms.is_empty() {
            println!("  denied: {}", finding.denied_axioms.join(", "));
        }
    }
    if report.policy_violations == 0 {
        println!("policy: passed");
    } else {
        println!(
            "policy: failed ({} declaration(s) violated the requested policy)",
            report.policy_violations
        );
    }
    for warning in &report.warnings {
        eprintln!("lev: warning: {warning}");
    }
}

fn relay_output(output: &Output, diagnostics_only: bool) -> Result<()> {
    if diagnostics_only {
        let mut diagnostics = io::stderr().lock();
        diagnostics
            .write_all(&output.stdout)
            .context("failed to relay command stdout")?;
        diagnostics
            .write_all(&output.stderr)
            .context("failed to relay command stderr")?;
    } else {
        io::stdout()
            .lock()
            .write_all(&output.stdout)
            .context("failed to relay command stdout")?;
        io::stderr()
            .lock()
            .write_all(&output.stderr)
            .context("failed to relay command stderr")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BuildEventAction, parse_build_event, parse_checked_type};

    #[test]
    fn parses_lake_verbose_build_events_and_durations() {
        let built = parse_build_event("✔ [1/3] Built Root (12ms)").unwrap();
        assert_eq!(built.action, BuildEventAction::Built);
        assert_eq!(built.target, "Root");
        assert_eq!(built.elapsed_ms, Some(12.0));

        let replayed = parse_build_event("ℹ [2/3] Replayed Mathlib").unwrap();
        assert_eq!(replayed.action, BuildEventAction::Replayed);
        assert_eq!(replayed.elapsed_ms, None);

        let ran = parse_build_event("✔ [3/3] Ran job computation").unwrap();
        assert_eq!(ran.action, BuildEventAction::Ran);
        assert_eq!(ran.target, "job computation");
        assert_eq!(ran.elapsed_ms, None);

        let reused = parse_build_event("ℹ [4/6] Reused Mathlib (1.5s)").unwrap();
        assert_eq!(reused.action, BuildEventAction::Reused);
        assert_eq!(reused.elapsed_ms, Some(1500.0));

        let fetched = parse_build_event("↓ [5/6] Fetched archive.tar.zst").unwrap();
        assert_eq!(fetched.action, BuildEventAction::Fetched);
        assert_eq!(fetched.target, "archive.tar.zst");

        let unpacked = parse_build_event("✔ [6/6] Unpacked Mathlib").unwrap();
        assert_eq!(unpacked.action, BuildEventAction::Unpacked);
    }

    #[test]
    fn extracts_the_last_structured_check_result() {
        let output = br#"{"data":"first : Nat","severity":"information"}
{"data":"Nat.add_comm : \u2200 (n m : Nat), n + m = m + n","severity":"information"}
"#;
        assert_eq!(
            parse_checked_type(output).as_deref(),
            Some("∀ (n m : Nat), n + m = m + n")
        );
    }
}
