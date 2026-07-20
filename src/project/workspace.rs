//! Monorepo workspaces declared in `lev.toml`.
//!
//! These are user-owned collections of ordinary Lake projects, unlike the
//! cache copies in [`crate::project::local_workspace`]. Members must stay under
//! the workspace root, and glob expansion is bounded.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};

use crate::cache::digest;
use crate::core::atomic_file::replace as atomic_write;
use crate::project::config::LevConfig;
use crate::project::lockfile;
use crate::project::{Project, absolute};

const WORKSPACE_LOCK_VERSION: u32 = 1;
const MAX_WORKSPACE_MEMBERS: usize = 1024;

/// One configured Lean project and its stable path relative to the workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceMember {
    /// Slash-separated path used for ordering, diagnostics, and lockfiles.
    pub relative: String,
    /// Fully loaded member project.
    pub project: Project,
}

/// Expanded workspace rooted at the `lev.toml` that declared it.
#[derive(Debug)]
pub struct ProjectWorkspace {
    /// Canonical directory containing the workspace configuration.
    pub root: PathBuf,
    /// Configuration file whose digest is captured by the aggregate lock.
    pub config_path: PathBuf,
    /// Canonically resolved members sorted by [`WorkspaceMember::relative`].
    pub members: Vec<WorkspaceMember>,
}

/// Workspace configuration plus the exact lock for every member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceLock {
    version: u32,
    config_sha256: String,
    members: Vec<WorkspaceLockMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceLockMember {
    path: String,
    toolchain: String,
    lev_lock_sha256: String,
    lake_manifest_sha256: String,
}

impl ProjectWorkspace {
    /// Find the nearest ancestor `lev.toml` containing `[workspace]`.
    pub fn discover(start: &Path) -> Result<Self> {
        let mut current = absolute(start)?;
        if current.is_file() {
            current.pop();
        }
        loop {
            let config_path = current.join("lev.toml");
            if config_path.is_file() {
                let config = LevConfig::read(&current)?;
                if !config.workspace_members.is_empty() {
                    return Self::load(&current, config);
                }
            }
            if !current.pop() {
                break;
            }
        }
        bail!(
            "no [workspace] configuration found from {}; add workspace.members to lev.toml",
            start.display()
        )
    }

    fn load(root: &Path, config: LevConfig) -> Result<Self> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve workspace root {}", root.display()))?;
        let excludes = compile_excludes(&config.workspace_exclude)?;
        let mut projects = BTreeMap::<String, Project>::new();
        for member_pattern in &config.workspace_members {
            let matches = expand_pattern(&root, member_pattern)?;
            if matches.is_empty() {
                bail!(
                    "workspace member pattern {member_pattern:?} matched no directories under {}",
                    root.display()
                );
            }
            for candidate in matches {
                let candidate = fs::canonicalize(&candidate).with_context(|| {
                    format!("failed to resolve workspace member {}", candidate.display())
                })?;
                if !candidate.starts_with(&root) {
                    bail!(
                        "workspace member {} escapes workspace root {}",
                        candidate.display(),
                        root.display()
                    );
                }
                let relative_path = candidate
                    .strip_prefix(&root)
                    .context("workspace member escaped its root")?;
                let relative = portable_relative(relative_path)?;
                if excluded(&excludes, &relative) {
                    continue;
                }
                if !candidate.join("lean-toolchain").is_file() {
                    bail!(
                        "workspace member {} has no lean-toolchain",
                        candidate.display()
                    );
                }
                if !candidate.join("lakefile.toml").is_file()
                    && !candidate.join("lakefile.lean").is_file()
                {
                    bail!(
                        "workspace member {} has no lakefile.toml or lakefile.lean",
                        candidate.display()
                    );
                }
                let project = Project::load(candidate)?;
                projects.insert(relative, project);
                if projects.len() > MAX_WORKSPACE_MEMBERS {
                    bail!("workspace contains more than {MAX_WORKSPACE_MEMBERS} projects");
                }
            }
        }
        if projects.is_empty() {
            bail!("workspace has no members after exclusions");
        }
        let members = projects
            .into_iter()
            .map(|(relative, project)| WorkspaceMember { relative, project })
            .collect();
        Ok(Self {
            config_path: root.join("lev.toml"),
            root,
            members,
        })
    }

    /// Write a deterministic aggregate lock after member locks are current.
    pub fn refresh_lock(&self) -> Result<PathBuf> {
        let lock = self.generate_lock()?;
        let path = self.lock_path();
        let encoded = toml_edit::ser::to_string_pretty(&lock)
            .context("failed to serialize lev-workspace.lock")?;
        let contents =
            format!("# This file is generated by lev. Commit it to version control.\n{encoded}");
        if fs::read(&path).ok().as_deref() != Some(contents.as_bytes()) {
            atomic_write(&path, contents.as_bytes())?;
        }
        Ok(path)
    }

    /// Verify member lockfiles and the aggregate workspace envelope.
    pub fn verify_lock(&self) -> Result<()> {
        for member in &self.members {
            lockfile::verify(&member.project)
                .with_context(|| format!("workspace member {}", member.relative))?;
        }
        let path = self.lock_path();
        let source = fs::read_to_string(&path).with_context(|| {
            format!(
                "failed to read {}; run `lev workspace lock`",
                path.display()
            )
        })?;
        let locked: WorkspaceLock = toml_edit::de::from_str(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if locked.version != WORKSPACE_LOCK_VERSION {
            bail!(
                "{} uses unsupported workspace lock version {}",
                path.display(),
                locked.version
            );
        }
        let current = self.generate_lock()?;
        if locked != current {
            bail!(
                "workspace membership or member locks changed since {}; run `lev workspace lock`",
                path.display()
            );
        }
        Ok(())
    }

    /// Return the generated aggregate lockfile path.
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("lev-workspace.lock")
    }

    fn generate_lock(&self) -> Result<WorkspaceLock> {
        let config = fs::read(&self.config_path)
            .with_context(|| format!("failed to read {}", self.config_path.display()))?;
        let mut members = Vec::with_capacity(self.members.len());
        for member in &self.members {
            let lev_lock = fs::read(member.project.lock_path()).with_context(|| {
                format!(
                    "failed to read {}; run `lev lock` for workspace member {}",
                    member.project.lock_path().display(),
                    member.relative
                )
            })?;
            let manifest = fs::read(member.project.manifest_path()).with_context(|| {
                format!(
                    "failed to read {} for workspace member {}",
                    member.project.manifest_path().display(),
                    member.relative
                )
            })?;
            members.push(WorkspaceLockMember {
                path: member.relative.clone(),
                toolchain: member.project.toolchain.clone(),
                lev_lock_sha256: digest(&lev_lock),
                lake_manifest_sha256: digest(&manifest),
            });
        }
        Ok(WorkspaceLock {
            version: WORKSPACE_LOCK_VERSION,
            config_sha256: digest(&config),
            members,
        })
    }
}

fn expand_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    validate_pattern(pattern)?;
    if pattern == "." {
        return Ok(vec![root.to_owned()]);
    }
    let options = MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };

    // Expand one relative component at a time. Besides bounding traversal,
    // this avoids turning Windows verbatim paths (`\\?\...`) into glob syntax.
    let mut matches = vec![root.to_owned()];
    for component in pattern.split('/') {
        let matcher = Pattern::new(component)
            .with_context(|| format!("invalid workspace member pattern {pattern:?}"))?;
        let mut next = Vec::new();
        for parent in matches {
            let mut entries = match fs::read_dir(&parent) {
                Ok(entries) => entries
                    .collect::<std::io::Result<Vec<_>>>()
                    .with_context(|| format!("failed to read {}", parent.display()))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to read {}", parent.display()));
                }
            };
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if matcher.matches_with(name, options) && entry.path().is_dir() {
                    next.push(entry.path());
                    if next.len() > MAX_WORKSPACE_MEMBERS {
                        bail!(
                            "workspace member pattern {pattern:?} matched more than \
                             {MAX_WORKSPACE_MEMBERS} directories"
                        );
                    }
                }
            }
        }
        next.sort();
        next.dedup();
        matches = next;
        if matches.is_empty() {
            break;
        }
    }

    matches.sort();
    matches.dedup();
    Ok(matches)
}

fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty()
        || pattern.len() > 4096
        || pattern.contains('\\')
        || pattern
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "**")
    {
        bail!("invalid workspace pattern {pattern:?}");
    }
    if pattern == "." {
        return Ok(());
    }
    let path = Path::new(pattern);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("workspace pattern must stay below its root: {pattern:?}");
    }
    Ok(())
}

fn compile_excludes(patterns: &[String]) -> Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|pattern| {
            validate_pattern(pattern)?;
            Pattern::new(pattern)
                .with_context(|| format!("invalid workspace exclusion pattern {pattern:?}"))
        })
        .collect()
}

fn excluded(patterns: &[Pattern], relative: &str) -> bool {
    patterns.iter().any(|pattern| pattern.matches(relative))
}

fn portable_relative(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() {
        return Ok(".".to_owned());
    }
    path.components()
        .map(|component| {
            let Component::Normal(component) = component else {
                bail!("invalid workspace-relative path {}", path.display());
            };
            component
                .to_str()
                .map(str::to_owned)
                .with_context(|| format!("workspace path is not UTF-8: {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::ProjectWorkspace;
    use crate::project::lockfile;

    fn project(root: &std::path::Path, name: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        fs::write(
            root.join("lakefile.toml"),
            format!("[package]\nname = \"{name}\"\n"),
        )
        .unwrap();
        fs::write(
            root.join("lake-manifest.json"),
            format!(r#"{{"name":"{name}","packagesDir":".lake/packages","packages":[]}}"#),
        )
        .unwrap();
        lockfile::refresh(&crate::project::Project::load(root.to_owned()).unwrap()).unwrap();
    }

    #[test]
    fn expands_members_excludes_projects_and_locks_deterministically() {
        let temp = tempdir().unwrap();
        project(&temp.path().join("packages/alpha"), "alpha");
        project(&temp.path().join("packages/beta"), "beta");
        project(&temp.path().join("packages/ignored"), "ignored");
        fs::write(
            temp.path().join("lev.toml"),
            r#"[workspace]
members = ["packages/*"]
exclude = ["packages/ignored"]
"#,
        )
        .unwrap();

        let workspace = ProjectWorkspace::discover(&temp.path().join("packages/alpha")).unwrap();
        assert_eq!(workspace.members.len(), 2);
        assert_eq!(workspace.members[0].relative, "packages/alpha");
        assert_eq!(workspace.members[1].relative, "packages/beta");
        workspace.refresh_lock().unwrap();
        let first = fs::read(workspace.lock_path()).unwrap();
        workspace.refresh_lock().unwrap();
        assert_eq!(fs::read(workspace.lock_path()).unwrap(), first);
        workspace.verify_lock().unwrap();

        fs::write(
            temp.path().join("packages/beta/lakefile.toml"),
            "[package]\nname = \"changed\"\n",
        )
        .unwrap();
        assert!(workspace.verify_lock().is_err());
    }

    #[test]
    fn discovers_from_nested_paths_deduplicates_and_escapes_the_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("workspace[production]");
        project(&root.join("packages/alpha"), "alpha");
        project(&root.join("packages/beta"), "beta");
        fs::write(
            root.join("lev.toml"),
            r#"[workspace]
members = ["packages/*", "packages/alpha"]
"#,
        )
        .unwrap();

        let nested = root.join("packages/alpha/src/deeply/nested");
        fs::create_dir_all(&nested).unwrap();
        let workspace = ProjectWorkspace::discover(&nested).unwrap();

        assert_eq!(workspace.root, fs::canonicalize(root).unwrap());
        assert_eq!(
            workspace
                .members
                .iter()
                .map(|member| member.relative.as_str())
                .collect::<Vec<_>>(),
            ["packages/alpha", "packages/beta"]
        );
    }

    #[test]
    fn aggregate_lock_detects_a_refreshed_member_lock() {
        let temp = tempdir().unwrap();
        project(&temp.path().join("alpha"), "alpha");
        fs::write(
            temp.path().join("lev.toml"),
            "[workspace]\nmembers = [\"alpha\"]\n",
        )
        .unwrap();
        let workspace = ProjectWorkspace::discover(temp.path()).unwrap();
        workspace.refresh_lock().unwrap();

        fs::write(
            temp.path().join("alpha/lake-manifest.json"),
            r#"{"name":"changed","packagesDir":".lake/packages","packages":[]}"#,
        )
        .unwrap();
        lockfile::refresh(&workspace.members[0].project).unwrap();

        let error = workspace.verify_lock().unwrap_err().to_string();
        assert!(error.contains("workspace membership or member locks changed"));
    }

    #[test]
    fn rejects_escape_and_missing_project_configuration() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("lev.toml"),
            "[workspace]\nmembers = [\"../outside\"]\n",
        )
        .unwrap();
        assert!(ProjectWorkspace::discover(temp.path()).is_err());

        fs::write(
            temp.path().join("lev.toml"),
            "[workspace]\nmembers = [\"missing\"]\n",
        )
        .unwrap();
        assert!(ProjectWorkspace::discover(temp.path()).is_err());

        let incomplete = temp.path().join("incomplete");
        fs::create_dir_all(&incomplete).unwrap();
        fs::write(
            incomplete.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-d\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("lev.toml"),
            "[workspace]\nmembers = [\"incomplete\"]\n",
        )
        .unwrap();
        let error = ProjectWorkspace::discover(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no lakefile"));
    }

    #[test]
    fn rejects_recursive_globs_and_an_empty_post_exclusion_workspace() {
        let temp = tempdir().unwrap();
        project(&temp.path().join("packages/alpha"), "alpha");
        fs::write(
            temp.path().join("lev.toml"),
            "[workspace]\nmembers = [\"packages/**\"]\n",
        )
        .unwrap();
        assert!(ProjectWorkspace::discover(temp.path()).is_err());

        fs::write(
            temp.path().join("lev.toml"),
            r#"[workspace]
members = ["packages/*"]
exclude = ["packages/*"]
"#,
        )
        .unwrap();
        let error = ProjectWorkspace::discover(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("no members after exclusions"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlinked_member_outside_the_workspace() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let root = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        project(&outside, "outside");
        fs::create_dir_all(root.join("packages")).unwrap();
        symlink(&outside, root.join("packages/escaped")).unwrap();
        fs::write(
            root.join("lev.toml"),
            "[workspace]\nmembers = [\"packages/*\"]\n",
        )
        .unwrap();

        let error = ProjectWorkspace::discover(&root).unwrap_err().to_string();
        assert!(error.contains("escapes workspace root"));
    }
}
