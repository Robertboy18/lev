//! Shared Git mirrors and exact dependency checkouts.
//!
//! Writers are locked per mirror, and dirty checkouts are never reset.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;

use crate::cache::{CacheLayout, digest};
use crate::core::process::{checked_output, checked_status, output_text};
use crate::project::manifest::GitPackage;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_CONCURRENT_GIT_JOBS: usize = 8;

#[derive(Debug, Default)]
pub struct SyncStats {
    pub mirrors_created: usize,
    pub mirrors_deferred: usize,
    pub mirrors_fetched: usize,
    pub packages_created: usize,
    pub packages_reused: usize,
    pub packages_updated: usize,
}

pub struct GitCache<'a> {
    cache: &'a CacheLayout,
    git: &'a OsStr,
    offline: bool,
    seed_packages_dir: Option<&'a Path>,
    defer_mirrors_for_existing: bool,
}

impl<'a> GitCache<'a> {
    pub fn new(cache: &'a CacheLayout, git: &'a OsStr, offline: bool) -> Self {
        Self {
            cache,
            git,
            offline,
            seed_packages_dir: None,
            defer_mirrors_for_existing: false,
        }
    }

    pub fn with_seed_packages_dir(mut self, path: Option<&'a Path>) -> Self {
        self.seed_packages_dir = path;
        self
    }

    /// Avoid eagerly duplicating Git metadata for complete shared checkouts.
    ///
    /// Lake has already downloaded these repositories while resolving a new
    /// environment, and the entire package tree now lives under lev's managed
    /// dependency cache. Creating a second bare mirror immediately makes the
    /// first sync slower without improving reuse of that exact graph. A mirror
    /// is still created later when a checkout is missing or another revision
    /// must be materialized.
    pub fn with_deferred_mirrors_for_existing(mut self, enabled: bool) -> Self {
        self.defer_mirrors_for_existing = enabled;
        self
    }

    pub fn sync(&self, packages_dir: &Path, packages: &[GitPackage<'_>]) -> Result<SyncStats> {
        self.cache.ensure()?;
        fs::create_dir_all(packages_dir)
            .with_context(|| format!("failed to create {}", packages_dir.display()))?;

        let mut stats = SyncStats::default();
        let jobs = packages
            .len()
            .min(MAX_CONCURRENT_GIT_JOBS)
            .min(
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1),
            )
            .max(1);
        for batch in packages.chunks(jobs) {
            let partials = std::thread::scope(|scope| {
                let handles = batch
                    .iter()
                    .map(|package| {
                        (
                            package.name,
                            scope.spawn(move || self.sync_package(packages_dir, package)),
                        )
                    })
                    .collect::<Vec<_>>();
                let mut partials = Vec::with_capacity(handles.len());
                let mut first_error = None;
                for (name, handle) in handles {
                    match handle.join() {
                        Ok(Ok(partial)) => partials.push(partial),
                        Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                        Ok(Err(_)) => {}
                        Err(_) if first_error.is_none() => {
                            first_error = Some(anyhow!(
                                "Git synchronization worker panicked for package {name}"
                            ));
                        }
                        Err(_) => {}
                    }
                }
                first_error.map_or_else(|| Ok(partials), Err)
            })?;
            for partial in partials {
                stats.merge(partial);
            }
        }
        Ok(stats)
    }

    fn sync_package(&self, packages_dir: &Path, package: &GitPackage<'_>) -> Result<SyncStats> {
        let mut stats = SyncStats::default();
        let package_dir = packages_dir.join(&package.dir_name);
        let mirror = self.cache.mirror_path(package.url);
        if mirror.is_dir()
            && checkout_head_file(&package_dir).as_deref() == Some(package.rev)
            && loose_pin_matches(&mirror, package.rev)
            && self.selector_marker_matches(&mirror, true, package)?
            && self.selector_marker_matches(&package_dir, false, package)?
        {
            stats.packages_reused += 1;
            return Ok(stats);
        }
        if self.defer_mirrors_for_existing
            && checkout_head_file(&package_dir).as_deref() == Some(package.rev)
        {
            stats.packages_reused += 1;
            stats.mirrors_deferred += 1;
            return Ok(stats);
        }
        let _lock = self.lock_mirror(package.url)?;

        if !mirror.exists() {
            self.create_mirror(package, &package_dir, &mirror)?;
            stats.mirrors_created += 1;
        }

        if !self.mirror_has_commit(&mirror, package.rev)? {
            self.fetch_commit(package, &package_dir, &mirror)?;
            stats.mirrors_fetched += 1;
        }

        self.ensure_selector_refs(&mirror, &package_dir, package)?;
        self.pin_commit(&mirror, package.rev)?;
        let materialized = self.materialize(package, &package_dir, &mirror)?;
        self.install_selector_refs(&mirror, &package_dir, package)?;
        match materialized {
            Materialized::Created => stats.packages_created += 1,
            Materialized::Reused => stats.packages_reused += 1,
            Materialized::Updated => stats.packages_updated += 1,
        }
        Ok(stats)
    }

    fn lock_mirror(&self, url: &str) -> Result<MirrorLock> {
        let path = self.cache.mirror_lock_path(url);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(MirrorLock(file))
    }

    fn create_mirror(
        &self,
        package: &GitPackage<'_>,
        package_dir: &Path,
        mirror: &Path,
    ) -> Result<()> {
        let parent = mirror
            .parent()
            .context("internal error: mirror has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let temporary = temporary_path(mirror);

        let seed = self.seed_checkout(package, package_dir)?;
        let local_seed = seed.is_some();
        if self.offline && !local_seed {
            bail!(
                "package {} is not cached at revision {} and --offline was requested",
                package.name,
                package.rev
            );
        }

        let result = (|| {
            let mut initialize = Command::new(self.git);
            initialize
                .arg("init")
                .arg("--bare")
                .arg("--quiet")
                .arg(&temporary);
            checked_status(&mut initialize)?;
            self.set_origin(&temporary, package.url, true)?;

            let source = seed
                .as_deref()
                .map(Path::as_os_str)
                .unwrap_or_else(|| OsStr::new(package.url));
            if !self.try_fetch_exact(&temporary, source, package.rev)? {
                // Some Git servers reject fetches by an unadvertised object
                // ID. Fall back to their advertised refs, then make one final
                // exact request for commits reachable only through a custom
                // refspec.
                if let Some(seed) = &seed {
                    self.fetch_from_path(&temporary, seed, package.rev)?;
                } else {
                    let mut update = self.git_dir_command(&temporary);
                    update.arg("fetch").arg("--prune").arg("origin");
                    checked_status(&mut update)?;
                }
            }
            if !self.mirror_has_commit(&temporary, package.rev)? {
                let mut exact = self.git_dir_command(&temporary);
                exact
                    .arg("fetch")
                    .arg("--no-tags")
                    .arg("origin")
                    .arg(package.rev);
                checked_status(&mut exact)?;
            }
            if !self.mirror_has_commit(&temporary, package.rev)? {
                bail!(
                    "revision {} for package {} was not provided by {}",
                    package.rev,
                    package.name,
                    package.url
                );
            }
            fs::rename(&temporary, mirror).with_context(|| {
                format!(
                    "failed to move mirror {} to {}",
                    temporary.display(),
                    mirror.display()
                )
            })?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    }

    fn fetch_commit(
        &self,
        package: &GitPackage<'_>,
        package_dir: &Path,
        mirror: &Path,
    ) -> Result<()> {
        if let Some(seed) = self.seed_checkout(package, package_dir)? {
            if !self.try_fetch_exact(mirror, seed.as_os_str(), package.rev)? {
                self.fetch_from_path(mirror, &seed, package.rev)?;
            }
        }

        if self.mirror_has_commit(mirror, package.rev)? {
            return Ok(());
        }
        if self.offline {
            bail!(
                "revision {} for package {} is missing from the cache",
                package.rev,
                package.name
            );
        }

        if !self.try_fetch_exact(mirror, OsStr::new(package.url), package.rev)? {
            let mut update = self.git_dir_command(mirror);
            update.arg("fetch").arg("--prune").arg("origin");
            checked_status(&mut update)?;
        }

        if !self.mirror_has_commit(mirror, package.rev)? {
            let mut exact = self.git_dir_command(mirror);
            exact
                .arg("fetch")
                .arg("--no-tags")
                .arg("origin")
                .arg(package.rev);
            checked_status(&mut exact)?;
        }

        if !self.mirror_has_commit(mirror, package.rev)? {
            bail!(
                "revision {} for package {} was not provided by {}",
                package.rev,
                package.name,
                package.url
            );
        }
        Ok(())
    }

    fn fetch_from_path(&self, mirror: &Path, source: &Path, rev: &str) -> Result<()> {
        let mut command = self.git_dir_command(mirror);
        command
            .arg("fetch")
            .arg("--no-tags")
            .arg("--")
            .arg(source)
            .arg(rev);
        checked_status(&mut command)
    }

    /// Preserve the symbolic ref Lake resolved when it still names the lock.
    ///
    /// A commit-only checkout is enough for compilation, but Lake also uses
    /// release tags to locate package cloud releases. Only advertised refs
    /// that peel to the immutable manifest revision are retained; branches
    /// that moved after the lock was written are ignored.
    fn ensure_selector_refs(
        &self,
        mirror: &Path,
        package_dir: &Path,
        package: &GitPackage<'_>,
    ) -> Result<()> {
        let candidates = self.selector_candidates(package)?;
        if candidates.is_empty() || self.selector_marker_matches(mirror, true, package)? {
            return Ok(());
        }

        let mut matched = self
            .publish_matching_refs(mirror, mirror.as_os_str(), package, &candidates)?
            .matched;
        if !matched && package_dir.is_dir() {
            matched = self
                .publish_matching_refs(mirror, package_dir.as_os_str(), package, &candidates)?
                .matched;
        }
        let mut remote_inspected = false;
        if !matched && !self.offline {
            let probe =
                self.publish_matching_refs(mirror, OsStr::new(package.url), package, &candidates)?;
            matched = probe.matched;
            remote_inspected = probe.inspected;
        }

        if matched || remote_inspected {
            self.update_selector_marker(mirror, true, package)?;
        }
        Ok(())
    }

    /// Copy cached tags and branches into the checkout Lake will inspect.
    fn install_selector_refs(
        &self,
        mirror: &Path,
        checkout: &Path,
        package: &GitPackage<'_>,
    ) -> Result<()> {
        let candidates = self.selector_candidates(package)?;
        if candidates.is_empty() {
            return Ok(());
        }
        if !self.selector_marker_matches(mirror, true, package)?
            || self.selector_marker_matches(checkout, false, package)?
        {
            return Ok(());
        }

        for candidate in candidates {
            if !self.ref_matches(mirror, true, &candidate.source, package.rev)? {
                continue;
            }
            let mut command = self.git_worktree_command(checkout);
            command
                .arg("fetch")
                .arg("--no-tags")
                .arg("--")
                .arg(mirror)
                .arg(format!("+{}:{}", candidate.source, candidate.checkout));
            checked_status(&mut command)?;
        }
        self.update_selector_marker(checkout, false, package)
    }

    /// Publish matching advertised refs from `source` into the bare mirror.
    fn publish_matching_refs(
        &self,
        mirror: &Path,
        source: &OsStr,
        package: &GitPackage<'_>,
        candidates: &[SelectorRef],
    ) -> Result<RefProbe> {
        let mut list = Command::new(self.git);
        list.arg("ls-remote").arg("--refs").arg("--").arg(source);
        for candidate in candidates {
            list.arg(&candidate.source);
        }
        let output = list
            .output()
            .with_context(|| format!("failed to inspect symbolic refs for {}", package.name))?;
        if !output.status.success() {
            return Ok(RefProbe::default());
        }
        let advertised = String::from_utf8_lossy(&output.stdout);
        let advertised = advertised
            .lines()
            .filter_map(|line| line.split_once('\t').map(|(_, reference)| reference))
            .collect::<std::collections::HashSet<_>>();

        let mut matched = false;
        for candidate in candidates {
            if self.ref_matches(mirror, true, &candidate.source, package.rev)? {
                matched = true;
                continue;
            }
            if !advertised.contains(candidate.source.as_str()) {
                continue;
            }

            let temporary = format!(
                "refs/lev/candidates/{}",
                digest(candidate.source.as_bytes())
            );
            let mut fetch = self.git_dir_command(mirror);
            fetch
                .arg("fetch")
                .arg("--no-tags")
                .arg("--depth=1")
                .arg("--")
                .arg(source)
                .arg(format!("+{}:{temporary}", candidate.source))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let fetched = fetch.status().with_context(|| {
                format!(
                    "failed to fetch symbolic ref {} for {}",
                    candidate.source, package.name
                )
            })?;
            if fetched.success() && self.ref_matches(mirror, true, &temporary, package.rev)? {
                let object = self.rev_parse(mirror, true, &temporary)?;
                let mut publish = self.git_dir_command(mirror);
                publish.arg("update-ref").arg(&candidate.source).arg(object);
                checked_status(&mut publish)?;
                matched = true;
            }

            let mut remove = self.git_dir_command(mirror);
            remove
                .arg("update-ref")
                .arg("-d")
                .arg(&temporary)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = remove.status();
        }
        Ok(RefProbe {
            inspected: true,
            matched,
        })
    }

    fn selector_candidates(&self, package: &GitPackage<'_>) -> Result<Vec<SelectorRef>> {
        let Some(selector) = package.input_rev else {
            return Ok(Vec::new());
        };
        if crate::core::hex::is_git_object_id(selector) {
            return Ok(Vec::new());
        }

        let sources = if selector.starts_with("refs/") {
            vec![selector.to_owned()]
        } else {
            vec![
                format!("refs/tags/{selector}"),
                format!("refs/heads/{selector}"),
            ]
        };
        let mut candidates = Vec::new();
        for source in sources {
            let mut check = Command::new(self.git);
            check
                .arg("check-ref-format")
                .arg(&source)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if !check
                .status()
                .with_context(|| format!("failed to validate Git selector {selector:?}"))?
                .success()
            {
                continue;
            }
            let checkout = source
                .strip_prefix("refs/heads/")
                .map(|branch| format!("refs/remotes/origin/{branch}"))
                .unwrap_or_else(|| source.clone());
            candidates.push(SelectorRef { source, checkout });
        }
        Ok(candidates)
    }

    fn selector_marker_matches(
        &self,
        repository: &Path,
        bare: bool,
        package: &GitPackage<'_>,
    ) -> Result<bool> {
        if self.selector_candidates(package)?.is_empty() {
            return Ok(true);
        }
        self.ref_matches(repository, bare, &selector_marker(package), package.rev)
    }

    fn update_selector_marker(
        &self,
        repository: &Path,
        bare: bool,
        package: &GitPackage<'_>,
    ) -> Result<()> {
        let mut command = if bare {
            self.git_dir_command(repository)
        } else {
            self.git_worktree_command(repository)
        };
        command
            .arg("update-ref")
            .arg(selector_marker(package))
            .arg(package.rev);
        checked_status(&mut command)
    }

    fn ref_matches(
        &self,
        repository: &Path,
        bare: bool,
        reference: &str,
        revision: &str,
    ) -> Result<bool> {
        if !repository.exists() {
            return Ok(false);
        }
        let mut command = if bare {
            self.git_dir_command(repository)
        } else {
            self.git_worktree_command(repository)
        };
        command
            .arg("rev-parse")
            .arg("--verify")
            .arg("--quiet")
            .arg(format!("{reference}^{{commit}}"));
        let output = command
            .output()
            .with_context(|| format!("failed to inspect {}", repository.display()))?;
        Ok(output.status.success() && output_text(output).trim() == revision)
    }

    fn rev_parse(&self, repository: &Path, bare: bool, reference: &str) -> Result<String> {
        let mut command = if bare {
            self.git_dir_command(repository)
        } else {
            self.git_worktree_command(repository)
        };
        command.arg("rev-parse").arg("--verify").arg(reference);
        Ok(output_text(checked_output(&mut command)?))
    }

    /// Attempt a depth-one fetch of exactly the immutable locked commit.
    ///
    /// Some servers reject object-ID fetches, so failure falls back to a full
    /// ref fetch.
    fn try_fetch_exact(&self, mirror: &Path, source: &OsStr, rev: &str) -> Result<bool> {
        let mut command = self.git_dir_command(mirror);
        command
            .arg("fetch")
            .arg("--no-tags")
            .arg("--depth=1")
            .arg("--")
            .arg(source)
            .arg(rev)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = command
            .status()
            .with_context(|| format!("failed to fetch locked revision {rev}"))?;
        Ok(status.success() && self.mirror_has_commit(mirror, rev)?)
    }

    fn pin_commit(&self, mirror: &Path, rev: &str) -> Result<()> {
        let key = digest(rev.as_bytes());
        let materialization_ref = format!("refs/heads/lev-cache/{key}");
        for reference in [format!("refs/lev/pins/{key}"), materialization_ref.clone()] {
            let mut command = self.git_dir_command(mirror);
            command.arg("update-ref").arg(reference).arg(rev);
            checked_status(&mut command)?;
        }
        // A private ref gives bare mirrors a valid HEAD without dropping older pins.
        let mut head = self.git_dir_command(mirror);
        head.arg("symbolic-ref")
            .arg("HEAD")
            .arg(materialization_ref);
        checked_status(&mut head)?;
        Ok(())
    }

    fn materialize(
        &self,
        package: &GitPackage<'_>,
        package_dir: &Path,
        mirror: &Path,
    ) -> Result<Materialized> {
        if package_dir.exists() {
            let head = self.checkout_head(package_dir).with_context(|| {
                format!(
                    "{} exists but is not a usable Git checkout",
                    package_dir.display()
                )
            })?;
            if head == package.rev {
                return Ok(Materialized::Reused);
            }

            let mut status = self.git_worktree_command(package_dir);
            status
                .arg("status")
                .arg("--porcelain=v1")
                .arg("--untracked-files=all");
            let changes = output_text(checked_output(&mut status)?);
            if !changes.is_empty() {
                bail!(
                    "dependency {} has local changes in {}; refusing to replace revision {} with {}",
                    package.name,
                    package_dir.display(),
                    head,
                    package.rev
                );
            }

            let mut fetch = self.git_worktree_command(package_dir);
            fetch
                .arg("fetch")
                .arg("--no-tags")
                .arg("--")
                .arg(mirror)
                .arg(package.rev);
            checked_status(&mut fetch)?;
            self.checkout_revision(package_dir, package.rev)?;
            self.set_origin(package_dir, package.url, false)?;
            return Ok(Materialized::Updated);
        }

        if let Some(parent) = package_dir.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let result = (|| {
            let local_branch = format!("lev-cache/{}", digest(package.rev.as_bytes()));
            let mut clone = Command::new(self.git);
            clone
                .arg("clone")
                .arg("--no-checkout")
                .arg("--shared")
                .arg("--single-branch")
                .arg("--branch")
                .arg(local_branch)
                .arg("--")
                .arg(mirror)
                .arg(package_dir);
            checked_status(&mut clone)?;
            self.set_origin(package_dir, package.url, false)?;
            self.checkout_revision(package_dir, package.rev)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(package_dir);
        }
        result.map(|_| Materialized::Created)
    }

    fn checkout_revision(&self, checkout: &Path, rev: &str) -> Result<()> {
        let mut command = self.git_worktree_command(checkout);
        command.arg("checkout").arg("--detach").arg(rev);
        checked_status(&mut command)
    }

    fn checkout_head(&self, checkout: &Path) -> Result<String> {
        let mut command = self.git_worktree_command(checkout);
        command.arg("rev-parse").arg("HEAD");
        Ok(output_text(checked_output(&mut command)?))
    }

    fn checkout_has_commit(&self, checkout: &Path, rev: &str) -> Result<bool> {
        if !checkout.exists() {
            return Ok(false);
        }
        let mut command = self.git_worktree_command(checkout);
        command
            .arg("cat-file")
            .arg("-e")
            .arg(format!("{rev}^{{commit}}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(command
            .status()
            .with_context(|| format!("failed to inspect {}", checkout.display()))?
            .success())
    }

    fn seed_checkout(
        &self,
        package: &GitPackage<'_>,
        package_dir: &Path,
    ) -> Result<Option<PathBuf>> {
        if self.checkout_has_commit(package_dir, package.rev)? {
            return Ok(Some(package_dir.to_owned()));
        }
        let Some(seed_root) = self.seed_packages_dir else {
            return Ok(None);
        };
        let seed = seed_root.join(&package.dir_name);
        if self.checkout_has_commit(&seed, package.rev)? {
            Ok(Some(seed))
        } else {
            Ok(None)
        }
    }

    fn mirror_has_commit(&self, mirror: &Path, rev: &str) -> Result<bool> {
        if !mirror.exists() {
            return Ok(false);
        }
        let mut command = self.git_dir_command(mirror);
        command
            .arg("cat-file")
            .arg("-e")
            .arg(format!("{rev}^{{commit}}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(command
            .status()
            .with_context(|| format!("failed to inspect {}", mirror.display()))?
            .success())
    }

    fn set_origin(&self, repository: &Path, url: &str, bare: bool) -> Result<()> {
        let mut has_origin = if bare {
            self.git_dir_command(repository)
        } else {
            self.git_worktree_command(repository)
        };
        has_origin
            .arg("remote")
            .arg("get-url")
            .arg("origin")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let exists = has_origin
            .status()
            .with_context(|| format!("failed to inspect {}", repository.display()))?
            .success();

        let mut command = if bare {
            self.git_dir_command(repository)
        } else {
            self.git_worktree_command(repository)
        };
        command.arg("remote");
        if exists {
            command.arg("set-url");
        } else {
            command.arg("add");
        }
        command.arg("origin").arg(url);
        checked_status(&mut command)?;

        if !bare {
            // `git clone --single-branch` records the selected private
            // `lev-cache/*` branch as origin's permanent fetch refspec. Once
            // the checkout is handed to Lake, origin names the upstream
            // repository again, where that private branch does not exist.
            // Restore the ordinary branch refspec so `lake update` and direct
            // user Git commands can fetch from the advertised repository.
            let mut refspec = self.git_worktree_command(repository);
            refspec
                .arg("config")
                .arg("--replace-all")
                .arg("remote.origin.fetch")
                .arg("+refs/heads/*:refs/remotes/origin/*");
            checked_status(&mut refspec)?;
        }
        Ok(())
    }

    fn git_dir_command(&self, repository: &Path) -> Command {
        let mut command = Command::new(self.git);
        command.arg("--git-dir").arg(repository);
        command
    }

    fn git_worktree_command(&self, repository: &Path) -> Command {
        let mut command = Command::new(self.git);
        command.arg("-C").arg(repository);
        command
    }
}

impl SyncStats {
    fn merge(&mut self, other: Self) {
        self.mirrors_created += other.mirrors_created;
        self.mirrors_deferred += other.mirrors_deferred;
        self.mirrors_fetched += other.mirrors_fetched;
        self.packages_created += other.packages_created;
        self.packages_reused += other.packages_reused;
        self.packages_updated += other.packages_updated;
    }
}

struct MirrorLock(File);

impl Drop for MirrorLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

enum Materialized {
    Created,
    Reused,
    Updated,
}

struct SelectorRef {
    source: String,
    checkout: String,
}

#[derive(Default)]
struct RefProbe {
    inspected: bool,
    matched: bool,
}

fn temporary_path(path: &Path) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("mirror"))
        .to_string_lossy();
    path.with_file_name(format!("{name}.tmp-{}-{counter}", std::process::id()))
}

fn checkout_head_file(checkout: &Path) -> Option<String> {
    let dot_git = checkout.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let contents = fs::read_to_string(&dot_git).ok()?;
        let path = contents.trim().strip_prefix("gitdir:")?.trim();
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_owned()
        } else {
            checkout.join(path)
        }
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if crate::core::hex::is_git_object_id(head) {
        Some(head.to_owned())
    } else {
        None
    }
}

fn loose_pin_matches(mirror: &Path, revision: &str) -> bool {
    let reference = digest(revision.as_bytes());
    fs::read_to_string(mirror.join("refs/lev/pins").join(reference))
        .is_ok_and(|contents| contents.trim() == revision)
}

fn selector_marker(package: &GitPackage<'_>) -> String {
    let selector = package.input_rev.unwrap_or_default();
    let key = digest(format!("{}\0{selector}", package.rev).as_bytes());
    format!("refs/lev/selectors/{key}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{checkout_head_file, loose_pin_matches};
    use crate::cache::digest;

    #[test]
    fn reads_detached_heads_from_git_directories_and_files() {
        let temp = tempdir().unwrap();
        let revision = "0123456789abcdef0123456789abcdef01234567";

        let checkout = temp.path().join("checkout");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::write(checkout.join(".git/HEAD"), format!("{revision}\n")).unwrap();
        assert_eq!(checkout_head_file(&checkout).as_deref(), Some(revision));

        let worktree = temp.path().join("worktree");
        let git_dir = temp.path().join("git-dir");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(worktree.join(".git"), "gitdir: ../git-dir\n").unwrap();
        fs::write(git_dir.join("HEAD"), revision).unwrap();
        assert_eq!(checkout_head_file(&worktree).as_deref(), Some(revision));
    }

    #[test]
    fn fast_path_requires_the_expected_loose_pin() {
        let temp = tempdir().unwrap();
        let revision = "0123456789abcdef0123456789abcdef01234567";
        assert!(!loose_pin_matches(temp.path(), revision));

        let pins = temp.path().join("refs/lev/pins");
        fs::create_dir_all(&pins).unwrap();
        fs::write(pins.join(digest(revision.as_bytes())), revision).unwrap();
        assert!(loose_pin_matches(temp.path(), revision));
        assert!(!loose_pin_matches(
            temp.path(),
            "1123456789abcdef0123456789abcdef01234567"
        ));
    }
}
