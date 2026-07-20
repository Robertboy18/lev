//! Local integrity checks for a locked Lean project.
//!
//! This checks reproducibility and provenance from local project data. It is
//! not a vulnerability scanner.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::core::process::{checked_output, checked_status, output_text};
use crate::project::Project;
use crate::project::lockfile;
use crate::project::manifest::{
    LakeManifest, ManifestPackage, package_directory_name, validate_revision,
};

const AUDIT_SCHEMA: &str = "lev.project-audit/v1";

/// Severity of one independently actionable audit result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditLevel {
    Pass,
    Warning,
    Error,
}

/// One check and its human-readable result.
#[derive(Debug, Serialize)]
pub struct AuditFinding {
    /// Stable check category suitable for filtering JSON output.
    pub check: String,
    /// Whether the check passed, warned, or failed.
    pub level: AuditLevel,
    /// Project file, toolchain, or dependency examined by the check.
    pub subject: String,
    /// Concrete result or remediation-oriented failure detail.
    pub message: String,
}

/// Aggregated finding counts kept in sync as results are added.
#[derive(Debug, Default, Serialize)]
pub struct AuditSummary {
    /// Checks that established the expected invariant.
    pub passed: u64,
    /// Portability or policy concerns that are not integrity failures.
    pub warnings: u64,
    /// Missing, stale, corrupt, or contradictory local state.
    pub errors: u64,
}

/// Complete machine-readable project audit.
#[derive(Debug, Serialize)]
pub struct AuditReport {
    /// Versioned report shape.
    pub schema: &'static str,
    /// Project root examined by this local report.
    pub project: String,
    /// Canonical Lean toolchain selected by the project.
    pub toolchain: String,
    /// Ordered checks and their complete diagnostics.
    pub findings: Vec<AuditFinding>,
    /// Counts used to determine the command's exit status.
    pub summary: AuditSummary,
}

impl AuditReport {
    /// Audit lock metadata and, unless disabled, every Git dependency checkout.
    pub fn inspect(project: &Project, git: &OsStr, checkouts: bool) -> Self {
        let mut report = Self {
            schema: AUDIT_SCHEMA,
            project: project.root.display().to_string(),
            toolchain: project.toolchain.clone(),
            findings: Vec::new(),
            summary: AuditSummary::default(),
        };

        match lockfile::verify(project) {
            Ok(()) => report.pass(
                "lock",
                "lev.lock",
                "lock matches the toolchain, Lake configuration, and manifest",
            ),
            Err(error) => report.error("lock", "lev.lock", format!("{error:#}")),
        }

        let manifest = match LakeManifest::read(&project.manifest_path()) {
            Ok(manifest) => {
                report.pass(
                    "manifest",
                    "lake-manifest.json",
                    "manifest is readable JSON",
                );
                manifest
            }
            Err(error) => {
                report.error("manifest", "lake-manifest.json", format!("{error:#}"));
                return report;
            }
        };

        let packages_dir = match manifest.packages_path(&project.root) {
            Ok(path) => {
                report.pass(
                    "packages_dir",
                    "lake-manifest.json",
                    "packagesDir stays within the project",
                );
                Some(path)
            }
            Err(error) => {
                report.error("packages_dir", "lake-manifest.json", format!("{error:#}"));
                None
            }
        };

        let mut names = HashSet::new();
        for package in &manifest.packages {
            let directory_name = match package_directory_name(&package.name) {
                Ok(name) => name,
                Err(error) => {
                    report.error("package_name", &package.name, format!("{error:#}"));
                    continue;
                }
            };
            if !names.insert(directory_name.clone()) {
                report.error(
                    "package_name",
                    &package.name,
                    "duplicate package directory in lake-manifest.json",
                );
                continue;
            }
            validate_auxiliary_paths(&mut report, package);
            match package.kind.as_str() {
                "git" => audit_git_package(
                    &mut report,
                    package,
                    &directory_name,
                    packages_dir.as_deref(),
                    git,
                    checkouts,
                ),
                "path" => audit_path_package(&mut report, project, package),
                kind => report.warning(
                    "package_kind",
                    &package.name,
                    format!("unrecognized Lake package type {kind:?}"),
                ),
            }
        }
        report
    }

    /// Add a successful external check, such as toolchain availability.
    pub fn pass(
        &mut self,
        check: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(AuditLevel::Pass, check, subject, message);
    }

    /// Add a warning produced by an external check.
    pub fn warning(
        &mut self,
        check: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(AuditLevel::Warning, check, subject, message);
    }

    /// Add an error produced by an external check.
    pub fn error(
        &mut self,
        check: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(AuditLevel::Error, check, subject, message);
    }

    /// Return the CI status for this report.
    pub fn exit_code(&self, strict: bool) -> i32 {
        if self.summary.errors > 0 || (strict && self.summary.warnings > 0) {
            1
        } else {
            0
        }
    }

    fn push(
        &mut self,
        level: AuditLevel,
        check: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) {
        match level {
            AuditLevel::Pass => self.summary.passed += 1,
            AuditLevel::Warning => self.summary.warnings += 1,
            AuditLevel::Error => self.summary.errors += 1,
        }
        self.findings.push(AuditFinding {
            check: check.into(),
            level,
            subject: subject.into(),
            message: message.into(),
        });
    }
}

fn validate_auxiliary_paths(report: &mut AuditReport, package: &ManifestPackage) {
    for (label, path) in [
        ("subDir", package.sub_dir.as_deref()),
        ("manifestFile", package.manifest_file.as_deref()),
        ("configFile", package.config_file.as_deref()),
    ] {
        let Some(path) = path else {
            continue;
        };
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        {
            report.error(
                "package_path",
                &package.name,
                format!("unsafe {label} path {}", path.display()),
            );
        }
    }
}

fn audit_git_package(
    report: &mut AuditReport,
    package: &ManifestPackage,
    directory_name: &str,
    packages_dir: Option<&Path>,
    git: &OsStr,
    checkouts: bool,
) {
    let Some(source) = package
        .url
        .as_deref()
        .filter(|source| !source.trim().is_empty())
    else {
        report.error(
            "git_source",
            &package.name,
            "Git dependency has no source URL",
        );
        return;
    };
    let Some(revision) = package
        .rev
        .as_deref()
        .filter(|revision| !revision.trim().is_empty())
    else {
        report.error(
            "git_revision",
            &package.name,
            "Git dependency has no locked revision",
        );
        return;
    };
    if let Err(error) = validate_revision(&package.name, revision) {
        report.error("git_revision", &package.name, format!("{error:#}"));
        return;
    }
    report.pass(
        "git_revision",
        &package.name,
        format!("locked to immutable revision {revision}"),
    );

    if !checkouts {
        return;
    }
    let Some(packages_dir) = packages_dir else {
        return;
    };
    let checkout = packages_dir.join(directory_name);
    if !checkout.is_dir() {
        report.error(
            "git_checkout",
            &package.name,
            format!("checkout is missing at {}", checkout.display()),
        );
        return;
    }

    match git_text(git, &checkout, ["rev-parse", "HEAD"]) {
        Ok(head) if head == revision => report.pass(
            "git_checkout",
            &package.name,
            "checkout HEAD matches the locked revision",
        ),
        Ok(head) => report.error(
            "git_checkout",
            &package.name,
            format!("checkout HEAD is {head}, expected {revision}"),
        ),
        Err(error) => report.error("git_checkout", &package.name, format!("{error:#}")),
    }

    match git_text(git, &checkout, ["remote", "get-url", "origin"]) {
        Ok(origin) if origin == source => report.pass(
            "git_origin",
            &package.name,
            "checkout origin matches the manifest",
        ),
        Ok(origin) => report.error(
            "git_origin",
            &package.name,
            format!("checkout origin is {origin:?}, expected {source:?}"),
        ),
        Err(error) => report.error("git_origin", &package.name, format!("{error:#}")),
    }

    match git_text(
        git,
        &checkout,
        ["status", "--porcelain=v1", "--untracked-files=all"],
    ) {
        Ok(status) if status.is_empty() => report.pass(
            "git_clean",
            &package.name,
            "dependency checkout has no local changes",
        ),
        Ok(status) => report.error(
            "git_clean",
            &package.name,
            format!(
                "dependency checkout has local changes: {}",
                first_line(&status)
            ),
        ),
        Err(error) => report.error("git_clean", &package.name, format!("{error:#}")),
    }
}

fn audit_path_package(report: &mut AuditReport, project: &Project, package: &ManifestPackage) {
    let Some(directory) = package.dir.as_deref() else {
        report.error(
            "path_dependency",
            &package.name,
            "path dependency has no directory",
        );
        return;
    };
    let path = project.root.join(directory);
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => report.warning(
            "path_dependency",
            &package.name,
            format!(
                "{} exists but is not pinned to immutable content",
                path.display()
            ),
        ),
        Ok(_) => report.error(
            "path_dependency",
            &package.name,
            format!("{} is not a directory", path.display()),
        ),
        Err(error) => report.error(
            "path_dependency",
            &package.name,
            format!("failed to inspect {}: {error}", path.display()),
        ),
    }
}

fn git_text<const N: usize>(
    git: &OsStr,
    repository: &Path,
    arguments: [&str; N],
) -> Result<String> {
    let mut command = Command::new(git);
    command.arg("-C").arg(repository).args(arguments);
    Ok(output_text(checked_output(&mut command)?))
}

fn first_line(value: &str) -> &str {
    value.lines().next().unwrap_or(value)
}

/// Provenance established before uploading a release artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseSource {
    /// Canonical Git worktree containing the project.
    pub repository: PathBuf,
    /// Commit selected by both `HEAD` and the requested tag.
    pub commit: String,
}

/// Require a valid local tag at `HEAD` and, by default, a clean worktree.
pub fn verify_release_source(
    project: &Project,
    git: &OsStr,
    tag: &str,
    allow_dirty: bool,
) -> Result<ReleaseSource> {
    if tag.is_empty() || tag.len() > 256 || tag.chars().any(char::is_control) {
        bail!("invalid release tag {tag:?}");
    }

    let mut check_ref = Command::new(git);
    check_ref
        .arg("-C")
        .arg(&project.root)
        .arg("check-ref-format")
        .arg(format!("refs/tags/{tag}"));
    checked_status(&mut check_ref).with_context(|| format!("invalid release tag {tag:?}"))?;

    let repository = PathBuf::from(git_text(
        git,
        &project.root,
        ["rev-parse", "--show-toplevel"],
    )?);
    let canonical_repository = fs::canonicalize(&repository)
        .with_context(|| format!("failed to resolve {}", repository.display()))?;
    let canonical_project = fs::canonicalize(&project.root)
        .with_context(|| format!("failed to resolve {}", project.root.display()))?;
    if !canonical_project.starts_with(&canonical_repository) {
        bail!(
            "project {} is outside Git worktree {}",
            canonical_project.display(),
            canonical_repository.display()
        );
    }

    let head = git_text(git, &project.root, ["rev-parse", "HEAD"])?;
    let tagged = git_text(
        git,
        &project.root,
        [
            "rev-parse",
            "--verify",
            &format!("refs/tags/{tag}^{{commit}}"),
        ],
    )
    .with_context(|| format!("release tag {tag:?} does not resolve to a local commit"))?;
    if tagged != head {
        bail!("release tag {tag:?} points to {tagged}, but HEAD is {head}");
    }

    if !allow_dirty {
        let status = git_text(
            git,
            &canonical_repository,
            ["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        if !status.is_empty() {
            bail!(
                "Git worktree has local changes: {}; commit or remove them, or pass --allow-dirty",
                first_line(&status)
            );
        }
    }

    Ok(ReleaseSource {
        repository: canonical_repository,
        commit: head,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    use super::{AuditReport, verify_release_source};
    use crate::project::Project;
    use crate::project::lockfile;

    fn git(root: &std::path::Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn warns_for_path_dependencies_and_strict_mode_fails() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("dep")).unwrap();
        fs::write(temp.path().join("lean-toolchain"), "v4.fixture-d\n").unwrap();
        fs::write(
            temp.path().join("lakefile.toml"),
            "[package]\nname = \"root\"\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("lake-manifest.json"),
            r#"{
              "packages": [{
                "name": "dep",
                "type": "path",
                "dir": "dep",
                "inherited": false
              }]
            }"#,
        )
        .unwrap();
        let project = Project::discover(temp.path()).unwrap();
        lockfile::refresh(&project).unwrap();

        let report = AuditReport::inspect(&project, OsStr::new("git"), false);
        assert_eq!(report.summary.errors, 0);
        assert_eq!(report.summary.warnings, 1);
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 1);
    }

    #[test]
    fn release_source_requires_a_clean_tag_at_head() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "v4.fixture-d\n").unwrap();
        fs::write(
            temp.path().join("lakefile.toml"),
            "[package]\nname = \"root\"\n",
        )
        .unwrap();
        git(temp.path(), &["init", "--initial-branch=main"]);
        git(
            temp.path(),
            &["config", "user.email", "lev@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "lev test"]);
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-m", "initial"]);
        git(temp.path(), &["tag", "v1.0.0"]);
        let project = Project::discover(temp.path()).unwrap();

        let source = verify_release_source(&project, OsStr::new("git"), "v1.0.0", false).unwrap();
        assert_eq!(source.repository, fs::canonicalize(temp.path()).unwrap());
        fs::write(temp.path().join("dirty"), "change").unwrap();
        assert!(verify_release_source(&project, OsStr::new("git"), "v1.0.0", false).is_err());
        verify_release_source(&project, OsStr::new("git"), "v1.0.0", true).unwrap();

        git(temp.path(), &["add", "dirty"]);
        git(temp.path(), &["commit", "-m", "later"]);
        assert!(verify_release_source(&project, OsStr::new("git"), "v1.0.0", true).is_err());
    }
}
