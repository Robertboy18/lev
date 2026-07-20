//! Resolution and activation of version-specific Lean environments.
//!
//! Alternate versions use isolated workspaces keyed by their exact lock.
//! Declarative Lakefiles may use Reservoir metadata; executable Lakefiles need
//! an explicit alternate configuration.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;

use crate::cache::digest;
use crate::cache::registry;
use crate::cli::SyncArgs;
use crate::core::atomic_file::replace as atomic_write;
use crate::dependency::reservoir::{ReservoirClient, ReservoirPackage, compatible_release};
use crate::dependency::resolution::ResolutionIdentity;
use crate::project::Project;
use crate::project::config::{EnvironmentConfig, LevConfig};
use crate::project::lakefile;
use crate::project::local_workspace as workspace;
use crate::project::lockfile::{self, LockedEnvironment};
use crate::project::manifest::LakeManifest;
use crate::toolchain;

use super::AppContext;
use super::native_resolver::Plan;
use super::sync_commands::sync;

/// Select one locked environment, resolving it on demand when permitted.
pub(super) fn select(
    context: &AppContext,
    source: &Project,
    selector: &str,
    offline: bool,
    resolve_missing: bool,
) -> Result<Project> {
    let locked = ensure_locked(context, source, selector, offline, false, resolve_missing)?;
    activate(context, source, &locked)
}

/// Resolve or validate one version-specific entry under the project lock.
pub(super) fn ensure_locked(
    context: &AppContext,
    source: &Project,
    selector: &str,
    offline: bool,
    refresh: bool,
    resolve_missing: bool,
) -> Result<LockedEnvironment> {
    let target = toolchain::normalize(selector)?;
    let policy = EnvironmentPolicy::load(source, &target)?;
    let _guard = resolution_lock(context, source, &target)?;

    if !refresh {
        match lockfile::environment(source, &target, &policy.sha256) {
            Ok(Some(environment)) => {
                context.detail(format!("reusing locked environment {target}"));
                return Ok(environment);
            }
            Ok(None) => {}
            Err(error) if resolve_missing && !offline => {
                context.detail(format!(
                    "re-resolving stale environment {target}: {error:#}"
                ));
            }
            Err(error) => return Err(error),
        }
    }

    if !resolve_missing {
        bail!("{target} is not locked for this project; run `lev lock --lean {target}`");
    }
    resolve(context, source, &policy, refresh, offline)
}

/// Require an existing lock without installing a toolchain or touching caches.
pub(super) fn verify_locked(source: &Project, selector: &str) -> Result<()> {
    let target = toolchain::normalize(selector)?;
    let policy = EnvironmentPolicy::load(source, &target)?;
    if lockfile::environment(source, &target, &policy.sha256)?.is_none() {
        bail!(
            "{target} is not present in {}; run `lev lock --lean {target}`",
            source.lock_path().display()
        );
    }
    Ok(())
}

fn resolve(
    context: &AppContext,
    source: &Project,
    policy: &EnvironmentPolicy,
    refresh_metadata: bool,
    offline: bool,
) -> Result<LockedEnvironment> {
    let base_manifest_path = source.manifest_path();
    let base_manifest = fs::read(&base_manifest_path)
        .with_context(|| format!("failed to read {}", base_manifest_path.display()))?;
    // Do not cache an identity derived from malformed source state.
    serde_json::from_slice::<LakeManifest>(&base_manifest)
        .with_context(|| format!("failed to parse {}", base_manifest_path.display()))?;
    let resolution = ResolutionIdentity::new(&policy.toolchain, &policy.sha256, &base_manifest)?;
    let cache_resolution = policy.effective_kind == LakefileKind::Toml;
    let _resolution_guard = cache_resolution
        .then(|| resolution.lock(&context.cache))
        .transpose()?;

    if cache_resolution && !refresh_metadata {
        match resolution.load(&context.cache) {
            Ok(Some(environment)) => {
                context.detail(format!(
                    "reusing cross-project resolution {} for {}",
                    resolution.short_key(),
                    policy.toolchain
                ));
                return activate_cached(
                    context,
                    source,
                    policy,
                    environment,
                    offline,
                    resolution.short_key(),
                );
            }
            Ok(None) => {}
            Err(error) if !offline => {
                context.info(format!(
                    "warning: discarding invalid cached resolution {} ({error:#})",
                    resolution.short_key()
                ));
                resolution.remove(&context.cache)?;
            }
            Err(error) => {
                return Err(error).context(
                    "cached resolution is invalid and cannot be replaced in offline mode",
                );
            }
        }
    }

    if offline {
        bail!(
            "{} is not available from lev.lock or the shared resolution cache in offline mode; \
             run `lev lock --lean {}` while online",
            policy.toolchain,
            policy.toolchain
        );
    }

    let revisions = policy.plan_revisions(context, source, refresh_metadata)?;
    let provisional_key = digest(&serde_json::to_vec(&(
        "lev-environment-resolution-v1",
        &policy.sha256,
        revisions.overrides(),
    ))?);
    let project = context.environment_project(source, &policy.toolchain, &provisional_key)?;
    policy.apply(&project, &revisions.resolution_overrides())?;

    // Warm exact checkouts from Reservoir's recorded graph when possible.
    // Lake remains the fallback for incomplete or conflicting metadata.
    let prefetched = match revisions.prefetch(context, &project) {
        Ok(prefetched) => prefetched,
        Err(error) => {
            context.info(format!(
                "warning: exact dependency prefetch failed ({error:#}); continuing with Lake"
            ));
            false
        }
    };

    let mut resolved_natively = false;
    if prefetched {
        match revisions.resolve_from_committed_manifest(context, &project) {
            Ok(true) => {
                context.info("resolved exact dependency graph without a Lake update");
                resolved_natively = true;
            }
            Ok(false) => {}
            Err(error) => {
                context.info(format!(
                    "warning: native dependency resolution was not usable ({error:#}); continuing with Lake"
                ));
            }
        }
    }
    if !resolved_natively {
        // Executable, path-based, conflicting, and incomplete registry graphs
        // retain Lake's fully general resolver. The native path above is an
        // optimization over Lake's own committed manifests, not a replacement
        // for unsupported package semantics.
        sync(
            context,
            &project,
            SyncArgs {
                offline: false,
                update: true,
                locked: false,
                frozen: false,
            },
        )?;
    }
    revisions.verify(&project)?;

    // Exact commits avoid redundant tag discovery during resolution, but the
    // effective Lakefile remains a user-facing project input. Restore its
    // readable release selectors before hashing and locking the environment;
    // the manifest still records Lake's validated immutable commits.
    policy.apply(&project, revisions.overrides())?;
    let environment =
        LockedEnvironment::capture(&project, policy.sha256.clone(), revisions.overrides())?;
    if cache_resolution && resolution.store(&context.cache, &environment)? {
        context.detail(format!(
            "stored cross-project resolution {} for {}",
            resolution.short_key(),
            policy.toolchain
        ));
    } else if !cache_resolution {
        context.detail(format!(
            "not caching executable Lakefile resolution for {}",
            policy.toolchain
        ));
    } else {
        context.detail(format!(
            "not caching location-dependent resolution for {}",
            policy.toolchain
        ));
    }
    let final_key = environment.workspace_key()?;
    let final_project = workspace::rekey(&context.cache, source, &project, &final_key)?;
    registry::replace(&context.cache, &project, &final_project)?;
    lockfile::upsert_environment(source, environment.clone())?;
    context.info(format!(
        "locked {} environment with {} direct override{}",
        policy.toolchain,
        revisions.override_count(),
        if revisions.override_count() == 1 {
            ""
        } else {
            "s"
        }
    ));
    Ok(environment)
}

/// Activate and materialize a globally cached resolver result.
///
/// Reusing the manifest skips only `lake update`. The ordinary sync path still
/// verifies the toolchain, attaches or creates the exact dependency
/// environment, and proves that every locked Git revision is locally
/// available (or fetchable when online).
fn activate_cached(
    context: &AppContext,
    source: &Project,
    policy: &EnvironmentPolicy,
    environment: LockedEnvironment,
    offline: bool,
    resolution_key: &str,
) -> Result<LockedEnvironment> {
    environment.verify_policy(&policy.toolchain, &policy.sha256)?;
    let workspace_key = environment.workspace_key()?;
    let project = context.environment_project(source, &policy.toolchain, &workspace_key)?;
    policy.apply(&project, &environment.overrides())?;
    environment.activate(&project)?;
    sync(
        context,
        &project,
        SyncArgs {
            offline,
            update: false,
            locked: true,
            frozen: false,
        },
    )
    .with_context(|| {
        format!(
            "failed to materialize cached {} resolution {resolution_key}",
            policy.toolchain
        )
    })?;
    lockfile::upsert_environment(source, environment.clone())?;
    context.info(format!(
        "reused shared {} dependency resolution",
        policy.toolchain
    ));
    Ok(environment)
}

fn activate(
    context: &AppContext,
    source: &Project,
    environment: &LockedEnvironment,
) -> Result<Project> {
    let policy = EnvironmentPolicy::load(source, environment.toolchain())?;
    environment.verify_policy(environment.toolchain(), &policy.sha256)?;
    let key = environment.workspace_key()?;
    let project = context.environment_project(source, environment.toolchain(), &key)?;
    policy.apply(&project, &environment.overrides())?;
    environment.activate(&project)?;
    Ok(project)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum LakefileKind {
    Toml,
    Lean,
}

#[derive(Debug)]
struct EnvironmentPolicy {
    toolchain: String,
    source_lakefile: PathBuf,
    effective_lakefile: PathBuf,
    effective_kind: LakefileKind,
    alternate: bool,
    auto: bool,
    dependencies: BTreeMap<String, String>,
    sha256: String,
}

#[derive(Serialize)]
struct PolicyFingerprint<'a> {
    version: u32,
    toolchain: &'a str,
    source_config_sha256: String,
    effective_lakefile: &'a str,
    effective_lakefile_sha256: String,
    auto: bool,
    dependencies: &'a BTreeMap<String, String>,
}

impl EnvironmentPolicy {
    fn load(source: &Project, toolchain: &str) -> Result<Self> {
        let config = LevConfig::read(&source.root)?;
        let selected = config
            .environments
            .get(toolchain)
            .cloned()
            .unwrap_or_default();
        let source_lakefile = project_lakefile(&source.root)?;
        let (effective_lakefile, alternate) =
            effective_lakefile(source, &selected, &source_lakefile)?;
        let effective_kind = lakefile_kind(&effective_lakefile)?;

        if effective_kind == LakefileKind::Lean && !selected.dependencies.is_empty() {
            bail!(
                "dependency overrides for {toolchain} require lakefile.toml; \
                 provide a complete alternate lakefile.lean instead"
            );
        }
        if effective_kind == LakefileKind::Lean
            && toolchain != source.toolchain
            && !alternate
            && selected.auto
        {
            bail!(
                "cannot safely resolve {toolchain} by rewriting executable {}; \
                 configure [environments.{:?}].lakefile with a complete alternate \
                 Lakefile, or set auto = false to preserve its declared revisions",
                source_lakefile.display(),
                toolchain::short_name(toolchain)
            );
        }

        let effective_relative = if let Some(path) = &selected.lakefile {
            path.to_string_lossy().into_owned()
        } else {
            source_lakefile
                .file_name()
                .context("project Lakefile has no file name")?
                .to_string_lossy()
                .into_owned()
        };
        let effective_bytes = fs::read(&effective_lakefile)
            .with_context(|| format!("failed to read {}", effective_lakefile.display()))?;
        let fingerprint = PolicyFingerprint {
            version: 1,
            toolchain,
            source_config_sha256: lockfile::project_config_digest(&source.root)?,
            effective_lakefile: &effective_relative,
            effective_lakefile_sha256: digest(&effective_bytes),
            auto: selected.auto,
            dependencies: &selected.dependencies,
        };
        let sha256 = digest(&serde_json::to_vec(&fingerprint)?);

        Ok(Self {
            toolchain: toolchain.to_owned(),
            source_lakefile,
            effective_lakefile,
            effective_kind,
            alternate,
            auto: selected.auto,
            dependencies: selected.dependencies,
            sha256,
        })
    }

    fn plan_revisions(
        &self,
        context: &AppContext,
        source: &Project,
        refresh_metadata: bool,
    ) -> Result<Plan> {
        if self.effective_kind == LakefileKind::Lean {
            return Ok(Plan::default());
        }

        let declared = lakefile::dependency_names(&self.effective_lakefile)?;
        for name in self.dependencies.keys() {
            if !declared.contains(name) {
                bail!(
                    "environment override {name} is not declared in {}",
                    self.effective_lakefile.display()
                );
            }
        }
        let mut planned = Plan::new(declared, self.dependencies.clone());
        let should_select_compatible = self.auto && self.toolchain != source.toolchain;
        if !should_select_compatible && self.dependencies.is_empty() {
            return Ok(planned);
        }

        let manifest = LakeManifest::read(&source.manifest_path())?;
        let mut packages = manifest
            .packages
            .into_iter()
            .filter(|package| !package.inherited && planned.is_declared(&package.name))
            .collect::<Vec<_>>();
        packages.sort_by(|left, right| left.name.cmp(&right.name));
        let config = LevConfig::read(&source.root)?;

        for package in packages {
            if let Some(revision) = self.dependencies.get(&package.name) {
                if crate::core::hex::is_git_object_id(revision)
                    && planned.add_exact_override(&package, revision)?
                {
                    context.detail(format!(
                        "using exact override for {} at {}",
                        package.name, revision
                    ));
                }
                continue;
            }
            if !should_select_compatible {
                continue;
            }
            if package
                .input_rev
                .as_deref()
                .is_some_and(crate::core::hex::is_git_object_id)
            {
                context.detail(format!(
                    "preserving exact commit pin for {} in {}",
                    package.name, self.toolchain
                ));
                planned.add_fixed_root(&package)?;
                continue;
            }
            let Some(identity) = ReservoirPackage::from_manifest(&package) else {
                context.detail(format!(
                    "preserving unindexed dependency {} in {}",
                    package.name, self.toolchain
                ));
                planned.add_fixed_root(&package)?;
                continue;
            };
            let client = ReservoirClient::new(&context.cache, false, refresh_metadata)
                .with_source(config.reservoir_source(&identity)?);
            let versions = client.versions(&identity).with_context(|| {
                format!(
                    "cannot select a {} release for {}; add an explicit \
                     environments.{:?}.dependencies override or set auto = false",
                    self.toolchain,
                    identity.full_name(),
                    toolchain::short_name(&self.toolchain)
                )
            })?;
            let Some(compatible) = compatible_release(&versions.versions, &self.toolchain) else {
                bail!(
                    "{} has no release compatible with {}; add an explicit \
                     environment dependency override",
                    identity.full_name(),
                    self.toolchain
                );
            };
            let tag = compatible.tag.clone().with_context(|| {
                format!(
                    "{} has no tagged release compatible with {}",
                    identity.full_name(),
                    self.toolchain
                )
            })?;
            planned.select_release(&package, compatible, tag)?;
        }
        Ok(planned)
    }

    fn apply(&self, project: &Project, overrides: &BTreeMap<String, String>) -> Result<()> {
        let destination_name = match self.effective_kind {
            LakefileKind::Toml => "lakefile.toml",
            LakefileKind::Lean => "lakefile.lean",
        };
        let destination = project.root.join(destination_name);
        if self.alternate {
            let contents = fs::read(&self.effective_lakefile)
                .with_context(|| format!("failed to read {}", self.effective_lakefile.display()))?;
            atomic_write(&destination, &contents)?;
        } else if !destination.is_file() {
            bail!(
                "{} was not materialized from {}",
                destination.display(),
                self.source_lakefile.display()
            );
        }

        let other = project.root.join(match self.effective_kind {
            LakefileKind::Toml => "lakefile.lean",
            LakefileKind::Lean => "lakefile.toml",
        });
        if other.exists() {
            fs::remove_file(&other)
                .with_context(|| format!("failed to remove {}", other.display()))?;
        }
        if self.effective_kind == LakefileKind::Toml {
            lakefile::set_revisions(&destination, overrides)?;
        } else if !overrides.is_empty() {
            bail!("cannot apply structured dependency revisions to lakefile.lean");
        }
        Ok(())
    }
}

fn project_lakefile(root: &Path) -> Result<PathBuf> {
    let toml = root.join("lakefile.toml");
    if toml.is_file() {
        return Ok(toml);
    }
    let lean = root.join("lakefile.lean");
    if lean.is_file() {
        return Ok(lean);
    }
    bail!(
        "no lakefile.toml or lakefile.lean found in {}",
        root.display()
    )
}

fn effective_lakefile(
    source: &Project,
    config: &EnvironmentConfig,
    default: &Path,
) -> Result<(PathBuf, bool)> {
    let Some(relative) = &config.lakefile else {
        return Ok((default.to_owned(), false));
    };
    let root = fs::canonicalize(&source.root)
        .with_context(|| format!("failed to resolve {}", source.root.display()))?;
    let selected = fs::canonicalize(source.root.join(relative)).with_context(|| {
        format!(
            "failed to resolve environment Lakefile {}",
            source.root.join(relative).display()
        )
    })?;
    if !selected.starts_with(&root) || !selected.is_file() {
        bail!(
            "environment Lakefile {} must be a regular file inside {}",
            selected.display(),
            root.display()
        );
    }
    Ok((selected, true))
}

fn lakefile_kind(path: &Path) -> Result<LakefileKind> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("toml") => Ok(LakefileKind::Toml),
        Some("lean") => Ok(LakefileKind::Lean),
        _ => bail!(
            "environment Lakefile {} must end in .toml or .lean",
            path.display()
        ),
    }
}

fn resolution_lock(
    context: &AppContext,
    source: &Project,
    _toolchain: &str,
) -> Result<EnvironmentResolutionLock> {
    context.cache.ensure()?;
    let identity = digest(source.root.to_string_lossy().as_bytes());
    let path = context
        .cache
        .lock_root()
        .join(format!("environment-{identity}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    FileExt::lock_exclusive(&file).with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(EnvironmentResolutionLock(file))
}

struct EnvironmentResolutionLock(File);

impl Drop for EnvironmentResolutionLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LakefileKind, lakefile_kind};

    #[test]
    fn classifies_supported_lakefiles() {
        assert_eq!(
            lakefile_kind(Path::new("compat/lakefile.toml")).unwrap(),
            LakefileKind::Toml
        );
        assert_eq!(
            lakefile_kind(Path::new("compat/lakefile.lean")).unwrap(),
            LakefileKind::Lean
        );
        assert!(lakefile_kind(Path::new("compat/lakefile.txt")).is_err());
    }
}
