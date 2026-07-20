//! Fast resolution for complete, registry-backed Lake graphs.
//!
//! The fast path runs only when committed manifests prove one exact graph.
//! Everything else goes through Lake.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::SyncArgs;
use crate::core::atomic_file::replace as atomic_write;
use crate::core::process::{checked_output, checked_status};
use crate::dependency::git::GitCache;
use crate::dependency::reservoir::{ReservoirDependency, ReservoirVersion};
use crate::project::Project;
use crate::project::local_workspace as workspace;
use crate::project::manifest::{GitPackage, LakeManifest, ManifestPackage, package_directory_name};
use crate::toolchain;

use super::AppContext;
use super::sync_commands::sync;

#[derive(Debug, Default)]
pub(super) struct Plan {
    declared_packages: BTreeSet<String>,
    overrides: BTreeMap<String, String>,
    unresolved_explicit_overrides: BTreeSet<String>,
    expected_commits: BTreeMap<String, String>,
    prefetch_packages: BTreeMap<String, PrefetchPackage>,
    prefetch_conflicts: BTreeSet<String>,
    release_snapshots: BTreeMap<String, Vec<ReservoirDependency>>,
}

impl Plan {
    pub(super) fn new(
        declared_packages: BTreeSet<String>,
        overrides: BTreeMap<String, String>,
    ) -> Self {
        let unresolved_explicit_overrides = overrides.keys().cloned().collect();
        Self {
            declared_packages,
            overrides,
            unresolved_explicit_overrides,
            ..Self::default()
        }
    }

    pub(super) fn is_declared(&self, name: &str) -> bool {
        self.declared_packages.contains(name)
    }

    pub(super) fn overrides(&self) -> &BTreeMap<String, String> {
        &self.overrides
    }

    pub(super) fn override_count(&self) -> usize {
        self.overrides.len()
    }

    pub(super) fn resolution_overrides(&self) -> BTreeMap<String, String> {
        let mut overrides = self.overrides.clone();
        overrides.extend(self.expected_commits.clone());
        overrides
    }

    pub(super) fn set_override(&mut self, name: String, revision: String) {
        self.overrides.insert(name, revision);
    }

    pub(super) fn select_release(
        &mut self,
        package: &ManifestPackage,
        release: &ReservoirVersion,
        tag: String,
    ) -> Result<()> {
        self.expected_commits
            .insert(package.name.clone(), release.revision.clone());
        self.add_release_snapshot(package, release)?;
        self.set_override(package.name.clone(), tag);
        Ok(())
    }

    pub(super) fn prefetch(&self, context: &AppContext, project: &Project) -> Result<bool> {
        prefetch_resolution_graph(context, project, self)
    }

    pub(super) fn resolve_from_committed_manifest(
        &self,
        context: &AppContext,
        project: &Project,
    ) -> Result<bool> {
        resolve_from_committed_manifest(context, project, self)
    }

    /// Add one release and its transitive snapshot to the fetch plan.
    fn add_release_snapshot(
        &mut self,
        package: &ManifestPackage,
        release: &ReservoirVersion,
    ) -> Result<()> {
        let Some(dependencies) = &release.dependencies else {
            return Ok(());
        };
        let Some(url) = package.url.as_deref() else {
            return Ok(());
        };
        if dependencies
            .iter()
            .all(|dependency| dependency.kind == "git")
        {
            self.release_snapshots
                .insert(package.name.clone(), dependencies.clone());
        }
        self.add_prefetch(PrefetchPackage::new(&package.name, url, &release.revision)?);
        for dependency in dependencies {
            if let Some(package) = PrefetchPackage::from_reservoir(dependency)? {
                self.add_prefetch(package);
            }
        }
        Ok(())
    }

    /// Add a direct root at its current locked commit.
    pub(super) fn add_fixed_root(&mut self, package: &ManifestPackage) -> Result<()> {
        if package.kind != "git" {
            return Ok(());
        }
        let (Some(url), Some(revision)) = (package.url.as_deref(), package.rev.as_deref()) else {
            return Ok(());
        };
        self.add_prefetch(PrefetchPackage::new(&package.name, url, revision)?);
        Ok(())
    }

    /// Add an override that already names an immutable commit.
    ///
    /// Lake handles moving tags and branches.
    pub(super) fn add_exact_override(
        &mut self,
        package: &ManifestPackage,
        revision: &str,
    ) -> Result<bool> {
        if package.kind != "git" {
            return Ok(false);
        }
        let Some(url) = package.url.as_deref() else {
            return Ok(false);
        };
        self.add_prefetch(PrefetchPackage::new(&package.name, url, revision)?);
        self.expected_commits
            .insert(package.name.clone(), revision.to_owned());
        self.unresolved_explicit_overrides.remove(&package.name);
        Ok(true)
    }

    fn add_prefetch(&mut self, package: PrefetchPackage) {
        if self.prefetch_conflicts.contains(&package.dir_name) {
            return;
        }
        if let Some(existing) = self.prefetch_packages.get(&package.dir_name) {
            if existing.revision == package.revision
                && equivalent_git_urls(&existing.url, &package.url)
            {
                return;
            }
            self.prefetch_packages.remove(&package.dir_name);
            self.prefetch_conflicts.insert(package.dir_name);
            return;
        }
        self.prefetch_packages
            .insert(package.dir_name.clone(), package);
    }

    pub(super) fn verify(&self, project: &Project) -> Result<()> {
        if self.expected_commits.is_empty() {
            return Ok(());
        }
        let manifest = LakeManifest::read(&project.manifest_path())?;
        for (name, expected) in &self.expected_commits {
            let package = manifest
                .packages
                .iter()
                .find(|package| package.name == *name)
                .with_context(|| format!("Lake did not resolve dependency {name}"))?;
            if package.rev.as_deref() != Some(expected) {
                bail!(
                    "Lake resolved {name} to {}, but Reservoir requires {expected}",
                    package.rev.as_deref().unwrap_or("<missing>")
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct PrefetchPackage {
    name: String,
    dir_name: String,
    url: String,
    revision: String,
}

impl PrefetchPackage {
    fn new(name: &str, url: &str, revision: &str) -> Result<Self> {
        Ok(Self {
            name: name.to_owned(),
            dir_name: package_directory_name(name)?,
            url: url.to_owned(),
            revision: revision.to_owned(),
        })
    }

    fn from_reservoir(dependency: &ReservoirDependency) -> Result<Option<Self>> {
        if dependency.kind != "git" {
            return Ok(None);
        }
        Self::new(&dependency.name, &dependency.url, &dependency.rev).map(Some)
    }

    fn borrowed(&self) -> GitPackage<'_> {
        GitPackage {
            name: &self.name,
            dir_name: self.dir_name.clone(),
            url: &self.url,
            rev: &self.revision,
            input_rev: None,
        }
    }
}

fn equivalent_git_urls(left: &str, right: &str) -> bool {
    fn normalized(value: &str) -> &str {
        value
            .trim_end_matches('/')
            .strip_suffix(".git")
            .unwrap_or_else(|| value.trim_end_matches('/'))
    }
    normalized(left) == normalized(right)
}

/// Fetch exact target revisions before invoking Lake.
///
/// Failed attempts leave checkouts only in the cache-local workspace.
fn prefetch_resolution_graph(
    context: &AppContext,
    project: &Project,
    revisions: &Plan,
) -> Result<bool> {
    if revisions.prefetch_packages.is_empty() || !native_resolution_candidate(project, revisions)? {
        return Ok(false);
    }
    let manifest = LakeManifest::read(&project.manifest_path())?;
    let packages_dir = manifest.packages_path(&project.root)?;
    let packages = revisions
        .prefetch_packages
        .values()
        .map(PrefetchPackage::borrowed)
        .collect::<Vec<_>>();
    let source_packages = workspace::source_packages_dir(project)?;

    context.info(format!(
        "prefetching {} exact dependenc{} before Lake resolution",
        packages.len(),
        if packages.len() == 1 { "y" } else { "ies" }
    ));
    let stats = GitCache::new(&context.cache, &context.git, false)
        .with_seed_packages_dir(source_packages.as_deref())
        .sync(&packages_dir, &packages)?;
    context.detail(format!(
        "dependency prefetch: {} created, {} reused, {} updated",
        stats.packages_created, stats.packages_reused, stats.packages_updated
    ));
    Ok(true)
}

fn native_resolution_candidate(project: &Project, revisions: &Plan) -> Result<bool> {
    if !revisions.prefetch_conflicts.is_empty()
        || !revisions.unresolved_explicit_overrides.is_empty()
    {
        return Ok(false);
    }
    let manifest = LakeManifest::read(&project.manifest_path())?;
    if manifest.packages_dir != Path::new(".lake/packages") {
        return Ok(false);
    }
    let direct = manifest
        .packages
        .iter()
        .filter(|package| !package.inherited)
        .collect::<Vec<_>>();
    let direct_names = direct
        .iter()
        .map(|package| package.name.clone())
        .collect::<BTreeSet<_>>();
    if direct_names != revisions.declared_packages {
        return Ok(false);
    }
    Ok(!direct.is_empty()
        && direct.iter().all(|package| {
            let Ok(directory) = package_directory_name(&package.name) else {
                return false;
            };
            let expected_revision = revisions
                .expected_commits
                .get(&package.name)
                .or(package.rev.as_ref());
            package.kind == "git"
                && package.dir.is_none()
                && package.url.as_deref().is_some_and(|url| {
                    revisions
                        .prefetch_packages
                        .get(&directory)
                        .is_some_and(|planned| {
                            equivalent_git_urls(&planned.url, url)
                                && Some(&planned.revision) == expected_revision
                        })
                })
        }))
}

/// Resolve complete, mutually compatible registry roots from Lake manifests.
///
/// Reservoir supplies each release commit and flattened graph. Every selected
/// package's committed `lake-manifest.json` is then treated as the stronger
/// immutable witness. Repeated transitive packages must agree on all
/// resolution-relevant metadata before the roots are merged. Any incomplete or
/// conflicting graph returns to Lake's general resolver.
fn resolve_from_committed_manifest(
    context: &AppContext,
    project: &Project,
    revisions: &Plan,
) -> Result<bool> {
    if !native_resolution_candidate(project, revisions)? {
        return Ok(false);
    }
    let base = LakeManifest::read(&project.manifest_path())?;
    if base.packages_dir != Path::new(".lake/packages") {
        return Ok(false);
    }
    let direct = base
        .packages
        .iter()
        .filter(|package| !package.inherited)
        .collect::<Vec<_>>();
    let packages_dir = base.packages_path(&project.root)?;
    let mut roots = Vec::with_capacity(direct.len());
    for root in direct {
        let indexed = revisions.release_snapshots.get(&root.name);
        let expected_revision = revisions
            .expected_commits
            .get(&root.name)
            .or(root.rev.as_ref())
            .with_context(|| format!("dependency {} has no locked revision", root.name))?;
        let requested_revision = revisions
            .overrides
            .get(&root.name)
            .or(root.input_rev.as_ref())
            .unwrap_or(expected_revision);

        let checkout = fs::canonicalize(packages_dir.join(package_directory_name(&root.name)?))
            .with_context(|| format!("failed to resolve checkout for {}", root.name))?;
        let package_root = package_root(&root.name, &checkout, root.sub_dir.as_deref())?;
        let manifest_file = root
            .manifest_file
            .clone()
            .unwrap_or_else(|| PathBuf::from("lake-manifest.json"));
        validate_package_file(&root.name, &package_root, &manifest_file, "manifestFile")?;
        let package_manifest_path = package_root.join(&manifest_file);
        let package_manifest = LakeManifest::read(&package_manifest_path)?;
        package_manifest.git_packages()?;
        if package_manifest.packages_dir != Path::new(".lake/packages")
            || package_manifest
                .packages
                .iter()
                .any(|package| package.kind != "git" || package.dir.is_some())
        {
            return Ok(false);
        }
        if let Some(snapshot) = indexed {
            validate_release_snapshot(snapshot, &package_manifest)?;
            verify_remote_tag(
                context,
                &root.name,
                root.url
                    .as_deref()
                    .context("registry snapshot root has no Git URL")?,
                requested_revision,
                expected_revision,
            )?;
        }

        let package_toolchain_path = [
            package_root.join("lean-toolchain"),
            checkout.join("lean-toolchain"),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| format!("{} has no lean-toolchain", root.name))?;
        let package_toolchain = fs::read_to_string(&package_toolchain_path)
            .with_context(|| format!("failed to read {}", package_toolchain_path.display()))?;
        let package_toolchain = toolchain::normalize(package_toolchain.trim())?;
        if package_toolchain != project.toolchain {
            bail!(
                "{} declares toolchain {}, not {}",
                root.name,
                package_toolchain,
                project.toolchain
            );
        }

        let configured = root
            .config_file
            .clone()
            .filter(|relative| package_root.join(relative).is_file());
        let config_file = if let Some(configured) = configured {
            configured
        } else if package_root.join("lakefile.toml").is_file() {
            PathBuf::from("lakefile.toml")
        } else if package_root.join("lakefile.lean").is_file() {
            PathBuf::from("lakefile.lean")
        } else {
            return Ok(false);
        };
        validate_package_file(&root.name, &package_root, &config_file, "configFile")?;
        let mut direct_package = root.clone();
        direct_package.rev = Some(expected_revision.clone());
        if revisions.overrides.contains_key(&root.name) {
            direct_package.input_rev = Some(requested_revision.clone());
        }
        direct_package.inherited = false;
        direct_package.config_file = Some(config_file);
        direct_package.manifest_file = Some(manifest_file);
        roots.push(CommittedRoot {
            name: root.name.clone(),
            package: direct_package,
            dependencies: package_manifest.packages,
        });
    }

    let packages = merge_committed_roots(roots)?;
    write_manifest_packages(&project.manifest_path(), &packages)?;

    // The graph has already been cross-validated, so invoking Lake would only
    // reread it. Locked sync performs attachment, and the checks below verify
    // package-owned paths.
    sync(
        context,
        project,
        SyncArgs {
            offline: false,
            update: false,
            locked: true,
            frozen: false,
        },
    )?;
    validate_materialized_package_files(project)?;
    Ok(true)
}

#[derive(Debug)]
struct CommittedRoot {
    name: String,
    package: ManifestPackage,
    dependencies: Vec<ManifestPackage>,
}

#[derive(Debug)]
struct MergedPackage {
    package: ManifestPackage,
    source_root: String,
    direct: bool,
}

/// Merge root manifests without inventing package precedence rules.
///
/// Direct roots retain their declaration order. New transitive packages are
/// appended by normalized checkout name so output is deterministic regardless
/// of Reservoir response order.
fn merge_committed_roots(roots: Vec<CommittedRoot>) -> Result<Vec<ManifestPackage>> {
    let mut direct_order = Vec::with_capacity(roots.len());
    let mut merged = BTreeMap::<String, MergedPackage>::new();

    for root in &roots {
        let key = package_directory_name(&root.package.name)?;
        if merged
            .insert(
                key.clone(),
                MergedPackage {
                    package: root.package.clone(),
                    source_root: root.name.clone(),
                    direct: true,
                },
            )
            .is_some()
        {
            bail!("direct dependencies resolve to the same package directory {key:?}");
        }
        direct_order.push(key);
    }

    for root in roots {
        for mut package in root.dependencies {
            package.inherited = true;
            let key = package_directory_name(&package.name)?;
            if let Some(existing) = merged.get(&key) {
                if !manifest_packages_agree(&existing.package, &package) {
                    bail!(
                        "package {key} disagrees between registry roots {} and {}",
                        existing.source_root,
                        root.name
                    );
                }
                continue;
            }
            merged.insert(
                key,
                MergedPackage {
                    package,
                    source_root: root.name.clone(),
                    direct: false,
                },
            );
        }
    }

    let mut packages = Vec::with_capacity(merged.len());
    for key in direct_order {
        packages.push(
            merged
                .remove(&key)
                .context("merged dependency graph lost a direct root")?
                .package,
        );
    }
    packages.extend(
        merged
            .into_values()
            .filter(|package| !package.direct)
            .map(|package| package.package),
    );
    Ok(packages)
}

fn manifest_packages_agree(left: &ManifestPackage, right: &ManifestPackage) -> bool {
    left.kind == right.kind
        && left.url.as_deref().map(normalized_git_url)
            == right.url.as_deref().map(normalized_git_url)
        && left.rev == right.rev
        && left.scope == right.scope
        && left.input_rev == right.input_rev
        && left.sub_dir == right.sub_dir
        && left.dir == right.dir
        && left.manifest_file == right.manifest_file
        && left.config_file == right.config_file
}

/// Prove that the human-readable release selector names the advertised commit.
///
/// Reservoir supplies the candidate mapping, but a standard Lake update would
/// independently resolve the tag. Native resolution preserves that check with
/// a ref-only query before publishing a lock. Git is kept noninteractive and
/// HTTP transfers use a low-speed deadline so a stalled registry endpoint
/// reaches the ordinary Lake fallback promptly.
fn verify_remote_tag(
    context: &AppContext,
    package: &str,
    url: &str,
    tag: &str,
    expected_revision: &str,
) -> Result<()> {
    let reference = format!("refs/tags/{tag}");
    let mut check = Command::new(&context.git);
    check.arg("check-ref-format").arg(&reference);
    checked_status(&mut check).with_context(|| {
        format!("Reservoir returned an invalid release tag {tag:?} for {package}")
    })?;

    let peeled = format!("{reference}^{{}}");
    let mut query = Command::new(&context.git);
    query
        .arg("-c")
        .arg("credential.interactive=false")
        .arg("-c")
        .arg("http.lowSpeedLimit=1")
        .arg("-c")
        .arg("http.lowSpeedTime=15")
        .arg("ls-remote")
        .arg("--exit-code")
        .arg("--tags")
        .arg("--")
        .arg(url)
        .arg(&reference)
        .arg(&peeled)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = checked_output(&mut query)
        .with_context(|| format!("failed to verify release tag {tag} for {package}"))?;
    let output = String::from_utf8(output.stdout)
        .with_context(|| format!("Git returned non-UTF-8 refs for {package}"))?;
    let observed = advertised_tag_commit(&output, &reference)?;
    context.detail(format!(
        "verified release tag {tag} for {package} at {observed}"
    ));
    if observed != expected_revision {
        bail!(
            "release tag {tag} for {package} resolves to {observed}, \
             but Reservoir requires {expected_revision}"
        );
    }
    Ok(())
}

fn advertised_tag_commit(output: &str, reference: &str) -> Result<String> {
    let peeled_reference = format!("{reference}^{{}}");
    let mut direct = None;
    let mut peeled = None;
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let revision = fields.next().context("Git advertised an empty ref row")?;
        let name = fields
            .next()
            .context("Git advertised a ref without a name")?;
        if fields.next().is_some() || !crate::core::hex::is_git_object_id(revision) {
            bail!("Git advertised an invalid release ref row: {line:?}");
        }
        if name == reference {
            direct = Some(revision);
        } else if name == peeled_reference {
            peeled = Some(revision);
        }
    }
    peeled
        .or(direct)
        .map(str::to_owned)
        .with_context(|| format!("Git did not advertise {reference}"))
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SnapshotPackage {
    url: String,
    revision: String,
    input_revision: Option<String>,
    scope: String,
}

fn validate_release_snapshot(
    expected: &[ReservoirDependency],
    manifest: &LakeManifest,
) -> Result<()> {
    let expected = expected
        .iter()
        .map(|package| {
            Ok((
                package_directory_name(&package.name)?,
                SnapshotPackage {
                    url: normalized_git_url(&package.url),
                    revision: package.rev.clone(),
                    input_revision: package.input_rev.clone(),
                    scope: package.scope.clone().unwrap_or_default(),
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let observed = manifest
        .packages
        .iter()
        .map(|package| {
            let url = package
                .url
                .as_deref()
                .with_context(|| format!("package {} has no Git URL", package.name))?;
            let revision = package
                .rev
                .clone()
                .with_context(|| format!("package {} has no Git revision", package.name))?;
            Ok((
                package_directory_name(&package.name)?,
                SnapshotPackage {
                    url: normalized_git_url(url),
                    revision,
                    input_revision: package.input_rev.clone(),
                    scope: package.scope.clone().unwrap_or_default(),
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    if expected != observed {
        bail!("Reservoir dependency snapshot disagrees with the release's committed Lake manifest");
    }
    Ok(())
}

fn normalized_git_url(value: &str) -> String {
    value
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| value.trim_end_matches('/'))
        .to_owned()
}

fn write_manifest_packages(path: &Path, packages: &[ManifestPackage]) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut document: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let object = document
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let mut encoded = packages
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    for package in &mut encoded {
        if let Some(object) = package.as_object_mut() {
            if object.get("dir").is_some_and(serde_json::Value::is_null) {
                object.remove("dir");
            }
        }
    }
    object.insert("packages".to_owned(), serde_json::Value::Array(encoded));
    let mut rendered = serde_json::to_vec_pretty(&document)?;
    rendered.push(b'\n');
    atomic_write(path, &rendered)?;
    LakeManifest::read(path)?.git_packages()?;
    Ok(())
}

fn validate_materialized_package_files(project: &Project) -> Result<()> {
    let manifest = LakeManifest::read(&project.manifest_path())?;
    let packages_dir = manifest.packages_path(&project.root)?;
    for package in &manifest.packages {
        let checkout = packages_dir.join(package_directory_name(&package.name)?);
        let checkout = fs::canonicalize(&checkout)
            .with_context(|| format!("failed to resolve checkout for {}", package.name))?;
        let root = if let Some(sub_dir) = &package.sub_dir {
            fs::canonicalize(checkout.join(sub_dir)).with_context(|| {
                format!(
                    "failed to resolve package subdirectory for {}",
                    package.name
                )
            })?
        } else {
            checkout.clone()
        };
        if !root.starts_with(&checkout) {
            bail!(
                "package {} has a subdirectory outside its checkout",
                package.name
            );
        }
        let config = package
            .config_file
            .as_ref()
            .with_context(|| format!("package {} has no configFile", package.name))?;
        validate_package_file(&package.name, &root, config, "configFile")?;
        if let Some(manifest_file) = &package.manifest_file {
            validate_package_file(&package.name, &root, manifest_file, "manifestFile")?;
        }
    }
    Ok(())
}

fn package_root(name: &str, checkout: &Path, sub_dir: Option<&Path>) -> Result<PathBuf> {
    let Some(sub_dir) = sub_dir else {
        return Ok(checkout.to_owned());
    };
    let root = fs::canonicalize(checkout.join(sub_dir)).with_context(|| {
        format!(
            "failed to resolve package subdirectory for {name}: {}",
            sub_dir.display()
        )
    })?;
    if !root.starts_with(checkout) || !root.is_dir() {
        bail!(
            "package {name} has an unsafe subdirectory: {}",
            sub_dir.display()
        );
    }
    Ok(root)
}

fn validate_package_file(name: &str, root: &Path, relative: &Path, field: &str) -> Result<()> {
    let path = fs::canonicalize(root.join(relative)).with_context(|| {
        format!(
            "package {name} has a missing {field}: {}",
            relative.display()
        )
    })?;
    if !path.starts_with(root) || !path.is_file() {
        bail!(
            "package {name} has an unsafe {field}: {}",
            relative.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::dependency::reservoir::ReservoirDependency;
    use crate::project::manifest::{LakeManifest, ManifestPackage};

    use super::{
        CommittedRoot, advertised_tag_commit, merge_committed_roots, validate_release_snapshot,
    };

    #[test]
    fn registry_and_committed_manifests_must_agree() {
        let revision = "0123456789abcdef0123456789abcdef01234567";
        let manifest: LakeManifest = serde_json::from_str(&format!(
            r#"{{
                "packages": [{{
                    "name": "child",
                    "type": "git",
                    "scope": "example",
                    "url": "https://example.invalid/child.git",
                    "rev": "{revision}",
                    "inputRev": "main"
                }}]
            }}"#
        ))
        .unwrap();
        let mut snapshot = vec![ReservoirDependency {
            kind: "git".to_owned(),
            name: "child".to_owned(),
            scope: Some("example".to_owned()),
            rev: revision.to_owned(),
            input_rev: Some("main".to_owned()),
            url: "https://example.invalid/child".to_owned(),
            transitive: false,
        }];

        validate_release_snapshot(&snapshot, &manifest).unwrap();
        snapshot[0].rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
        let error = validate_release_snapshot(&snapshot, &manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("disagrees"), "{error}");
    }

    #[test]
    fn merges_compatible_roots_deterministically() {
        let revision = "0123456789abcdef0123456789abcdef01234567";
        let shared = package(
            "shared",
            "https://example.invalid/shared.git",
            revision,
            "main",
        );
        let roots = vec![
            CommittedRoot {
                name: "root-b".to_owned(),
                package: package("root-b", "https://example.invalid/root-b", revision, "v1"),
                dependencies: vec![shared.clone()],
            },
            CommittedRoot {
                name: "root-a".to_owned(),
                package: package("root-a", "https://example.invalid/root-a", revision, "v1"),
                dependencies: vec![
                    package("leaf", "https://example.invalid/leaf", revision, "main"),
                    shared,
                ],
            },
        ];

        let merged = merge_committed_roots(roots).unwrap();

        assert_eq!(
            merged
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["root-b", "root-a", "leaf", "shared"]
        );
        assert!(merged[..2].iter().all(|package| !package.inherited));
        assert!(merged[2..].iter().all(|package| package.inherited));
    }

    #[test]
    fn rejects_conflicting_root_manifests() {
        let first_revision = "0123456789abcdef0123456789abcdef01234567";
        let second_revision = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let roots = vec![
            CommittedRoot {
                name: "root-a".to_owned(),
                package: package(
                    "root-a",
                    "https://example.invalid/root-a",
                    first_revision,
                    "v1",
                ),
                dependencies: vec![package(
                    "shared",
                    "https://example.invalid/shared.git",
                    first_revision,
                    "main",
                )],
            },
            CommittedRoot {
                name: "root-b".to_owned(),
                package: package(
                    "root-b",
                    "https://example.invalid/root-b",
                    first_revision,
                    "v1",
                ),
                dependencies: vec![package(
                    "shared",
                    "https://example.invalid/shared",
                    second_revision,
                    "main",
                )],
            },
        ];

        let error = merge_committed_roots(roots).unwrap_err().to_string();

        assert!(error.contains("shared"), "{error}");
        assert!(error.contains("root-a"), "{error}");
        assert!(error.contains("root-b"), "{error}");
    }

    #[test]
    fn tag_resolution_prefers_an_annotated_tags_peeled_commit() {
        let tag_object = "1111111111111111111111111111111111111111";
        let commit = "2222222222222222222222222222222222222222";
        let reference = "refs/tags/v1.0.0";
        assert_eq!(
            advertised_tag_commit(
                &format!(
                    "{tag_object}\t{reference}\n\
                     {commit}\t{reference}^{{}}\n"
                ),
                reference
            )
            .unwrap(),
            commit
        );
        assert_eq!(
            advertised_tag_commit(&format!("{commit}\t{reference}\n"), reference).unwrap(),
            commit
        );
    }

    fn package(name: &str, url: &str, revision: &str, input_revision: &str) -> ManifestPackage {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "type": "git",
            "url": url,
            "rev": revision,
            "inputRev": input_revision,
            "inherited": false,
            "manifestFile": "lake-manifest.json",
            "configFile": "lakefile.toml"
        }))
        .unwrap()
    }
}
