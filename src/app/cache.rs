//! `lev cache status` and `lev cache gc`.
//!
//! Both read the project registry. Status inventories lev's cache, the
//! caches of `lev check`, and every Lake cache a registered project used.
//! Collection lists the caches of `lev check` that no check used within the
//! age gate (exporter builds, references and verdicts) and the records of
//! projects that are gone. With `--lake` it also lists the Lake cache
//! entries older than the gate that no live project's traces reference, for
//! every Lake cache whose references it could read in full. `--apply`
//! deletes the list.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::AppContext;
use crate::cache::check as check_cache;
use crate::cache::lake::{self, Inventory, References};
use crate::cache::registry::{self, ProjectRecord};
use crate::cache::{GcPlan, human_bytes, path_bytes};
use crate::cli::{CacheGcArgs, CacheStatusArgs};
use crate::core::clock;
use crate::core::json_output::{self, schema};
use crate::core::lock;
use crate::project::Project;
use crate::project::build_trace::collect_references;

#[derive(Debug, Serialize)]
struct StatusReport {
    lev_cache: LevCacheStatus,
    check_cache: check_cache::Inventory,
    lake_caches: Vec<LakeCacheStatus>,
}

#[derive(Debug, Serialize)]
struct LevCacheStatus {
    path: PathBuf,
    bytes: u64,
    projects: usize,
    /// Registered projects whose directory no longer has a `lean-toolchain`.
    missing_projects: usize,
}

#[derive(Debug, Serialize)]
struct LakeCacheStatus {
    /// Registered projects whose last `lev inspect build` used this cache.
    projects: usize,
    #[serde(flatten)]
    inventory: Inventory,
}

#[derive(Debug, Serialize)]
struct GcReport {
    max_age_days: u64,
    applied: bool,
    /// Whether Lake's artifact caches were collected (`--lake`).
    lake: bool,
    #[serde(flatten)]
    plan: GcPlan,
    reclaimable_bytes: u64,
    skipped: Vec<Skipped>,
    /// Collected Lake caches that projects lev has not registered may use
    /// too; nothing protects their entries.
    shared: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Skipped {
    path: PathBuf,
    reason: String,
}

pub(super) fn status(context: &AppContext, args: CacheStatusArgs) -> Result<i32> {
    let records = registry::load(&context.cache)?;
    let root = &context.cache.root;
    let mut lake_caches = Vec::new();
    for path in lake_caches_of(&records) {
        let _lock = lock::shared(&context.cache.lake_lock_path(&path))?;
        context.detail(format!("reading the Lake cache {}", path.display()));
        lake_caches.push(LakeCacheStatus {
            projects: records
                .iter()
                .filter(|(_, record)| record.lake_cache.as_ref() == Some(&path))
                .count(),
            inventory: lake::scan(&path)?.inventory,
        });
    }
    let report = StatusReport {
        lev_cache: LevCacheStatus {
            path: root.clone(),
            bytes: if root.exists() { path_bytes(root)? } else { 0 },
            projects: records.len(),
            missing_projects: records
                .iter()
                .filter(|(_, record)| !record.exists())
                .count(),
        },
        check_cache: check_cache::inventory(&context.cache.check())?,
        lake_caches,
    };

    if args.json {
        json_output::print(schema::CACHE_STATUS, &report)?;
    } else {
        write_stdout(&render_status(&report))?;
        if report.lev_cache.projects == 0 {
            context.info(
                "no project is registered yet; `lev inspect build` and `lev check` register one",
            );
        }
    }
    let damaged = report
        .lake_caches
        .iter()
        .any(|cache| !cache.inventory.missing.is_empty());
    Ok(i32::from(damaged))
}

pub(super) fn gc(context: &AppContext, args: CacheGcArgs) -> Result<i32> {
    let cutoff = clock::cutoff(args.max_age_days);

    // A running `lev check` holds this lock shared until it is done.
    let check = context.cache.check();
    let _check_lock = if check.root.is_dir() {
        Some(lock::exclusive(&check.lock)?)
    } else {
        None
    };
    // A build registers its project while it holds the lock of the Lake
    // cache it builds into, so the registry is read with those locks held: a
    // project registered meanwhile is then in the records, and its traces
    // are on disk.
    let (records, _lake_locks) = if args.lake {
        locked_registry(
            || registry::load(&context.cache),
            |path| lock::exclusive(&context.cache.lake_lock_path(path)),
        )?
    } else {
        (registry::load(&context.cache)?, Vec::new())
    };
    let mut plan = GcPlan {
        candidates: registry::gc_candidates(&context.cache, &records, cutoff)?,
    };
    plan.candidates
        .extend(check_cache::gc_candidates(&check, cutoff)?);

    let caches = lake_caches_of(&records);
    let mut skipped = Vec::new();
    let mut shared = Vec::new();
    if args.lake {
        let discovery = discover(context, &records, cutoff);
        for path in &caches {
            if let Some(reason) = discovery.incomplete_for(path) {
                skipped.push(Skipped {
                    path: path.clone(),
                    reason,
                });
                continue;
            }
            context.detail(format!("reading the Lake cache {}", path.display()));
            let scan = lake::scan(path)?;
            let reason = if scan.inventory.legacy_layout {
                Some("it uses Lake's older inputs/ layout, which lev does not read".to_owned())
            } else if !scan.inventory.missing.is_empty() {
                Some(format!(
                    "{} local mappings name missing artifacts; see `lev cache status`",
                    scan.inventory.missing.len()
                ))
            } else {
                None
            };
            match reason {
                Some(reason) => skipped.push(Skipped {
                    path: path.clone(),
                    reason,
                }),
                None => {
                    plan.candidates
                        .extend(scan.gc_candidates(&discovery.references, cutoff));
                    if is_shared(&records, path) {
                        shared.push(path.clone());
                    }
                }
            }
        }
    }
    if args.apply {
        plan.apply()?;
    }

    let report = GcReport {
        max_age_days: args.max_age_days,
        applied: args.apply,
        lake: args.lake,
        reclaimable_bytes: plan.bytes(),
        plan,
        skipped,
        shared,
    };
    if args.json {
        json_output::print(schema::CACHE_GC, &report)?;
    } else {
        write_stdout(&render_gc(&report))?;
        if !args.lake && !caches.is_empty() {
            context.info("Lake's artifact caches are left alone; pass --lake to collect them too");
        }
        if !args.apply && !report.plan.candidates.is_empty() {
            context.info("dry run; pass --apply to remove these entries");
        }
    }
    Ok(0)
}

/// The registry's records, each with the file it was read from.
type Records = Vec<(PathBuf, ProjectRecord)>;

/// The registry, read with every Lake cache it names locked by `lock`. The
/// caches one read names are locked and the registry is read again; when
/// that read names other caches, the locks are dropped and it starts over.
fn locked_registry<L>(
    mut load: impl FnMut() -> Result<Records>,
    mut lock: impl FnMut(&Path) -> Result<L>,
) -> Result<(Records, Vec<L>)> {
    let mut caches = lake_caches_of(&load()?);
    loop {
        let locks = caches
            .iter()
            .map(|path| lock(path.as_path()))
            .collect::<Result<Vec<L>>>()?;
        let records = load()?;
        let named = lake_caches_of(&records);
        if named == caches {
            return Ok((records, locks));
        }
        drop(locks);
        caches = named;
    }
}

/// What the traces of the live registered projects name, and which
/// projects' traces could not all be read.
#[derive(Default)]
struct Discovery {
    references: References,
    /// The Lake cache the record of each such project names, and why.
    incomplete: Vec<(Option<PathBuf>, String)>,
}

impl Discovery {
    /// Why the references of `cache` may be incomplete: a project whose
    /// record names it, or names no cache at all, could not be read in full.
    fn incomplete_for(&self, cache: &Path) -> Option<String> {
        let mut reasons = self
            .incomplete
            .iter()
            .filter(|(used, _)| used.as_deref().is_none_or(|used| used == cache))
            .map(|(_, reason)| reason);
        let first = reasons.next()?;
        Some(match reasons.count() {
            0 => format!("not every reference could be read: {first}"),
            more => format!("not every reference could be read: {first}, and {more} more"),
        })
    }
}

fn discover(context: &AppContext, records: &[(PathBuf, ProjectRecord)], cutoff: u64) -> Discovery {
    let mut discovery = Discovery::default();
    for (_, record) in records {
        if !record.is_live(cutoff) || !record.exists() {
            continue;
        }
        if let Err(error) = project_references(context, &record.root, &mut discovery.references) {
            discovery.incomplete.push((
                record.lake_cache.clone(),
                format!("{}: {error:#}", record.root.display()),
            ));
        }
    }
    discovery
}

/// Add what the traces of every package of the project at `root` name. An
/// error means some could not be read; the readable ones are added anyway.
fn project_references(
    context: &AppContext,
    root: &Path,
    references: &mut References,
) -> Result<()> {
    let packages = Project::load(root.to_path_buf())?.packages()?;
    let mut first = None;
    for package in packages {
        context.detail(format!("reading traces of {}", package.root.display()));
        let read = package.build_dir().and_then(|dir| {
            collect_references(&dir, &mut references.hashes, &mut references.artifacts)
        });
        if let Err(error) = read {
            first.get_or_insert(error);
        }
    }
    first.map_or(Ok(()), Err)
}

/// Whether projects lev has not registered may use `cache` too: it is not
/// the own `.lake/cache` of the registered projects that use it. The
/// toolchain's default cache serves every project built with that toolchain.
fn is_shared(records: &[(PathBuf, ProjectRecord)], cache: &Path) -> bool {
    !records
        .iter()
        .filter(|(_, record)| record.lake_cache.as_deref() == Some(cache))
        .all(|(_, record)| record.root.join(".lake/cache") == cache)
}

/// The Lake caches registered projects used, in path order.
fn lake_caches_of(records: &[(PathBuf, ProjectRecord)]) -> BTreeSet<PathBuf> {
    records
        .iter()
        .filter_map(|(_, record)| record.lake_cache.clone())
        .collect()
}

fn write_stdout(text: &str) -> Result<()> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .context("failed to write the cache report")
}

fn render_status(report: &StatusReport) -> String {
    let lev = &report.lev_cache;
    let mut text = format!(
        "lev cache   {} ({})\nprojects    {} registered, {} no longer on disk\n",
        lev.path.display(),
        human_bytes(lev.bytes),
        lev.projects,
        lev.missing_projects
    );
    let check = &report.check_cache;
    if check.exists {
        let _ = writeln!(text, "check cache {}", check.path.display());
        for (label, group) in [
            ("exporters", &check.exporters),
            ("references", &check.references),
            ("verdicts", &check.verdicts),
            ("scratch", &check.scratch),
        ] {
            let _ = writeln!(
                text,
                "  {label:<11} {} ({})",
                group.entries,
                human_bytes(group.bytes)
            );
        }
    }
    for cache in &report.lake_caches {
        let inventory = &cache.inventory;
        let users = if cache.projects == 1 {
            "1 project".to_owned()
        } else {
            format!("{} projects", cache.projects)
        };
        let _ = writeln!(text, "Lake cache  {} ({users})", inventory.path.display());
        if !inventory.exists {
            text.push_str("  not created yet\n");
            continue;
        }
        if inventory.legacy_layout {
            text.push_str("  uses Lake's older inputs/ layout; not inventoried\n");
            continue;
        }
        let _ = writeln!(
            text,
            "  artifacts   {} ({}), {} hard-linked ({})",
            inventory.artifacts,
            human_bytes(inventory.artifact_bytes),
            inventory.hardlinked_artifacts,
            human_bytes(inventory.hardlinked_bytes)
        );
        let _ = writeln!(text, "  mappings    {}", inventory.mappings);
        let _ = writeln!(
            text,
            "  unmapped    {} artifacts ({})",
            inventory.unmapped_artifacts,
            human_bytes(inventory.unmapped_bytes)
        );
        let _ = writeln!(text, "  missing     {}", inventory.missing.len());
        for missing in &inventory.missing {
            let _ = writeln!(
                text,
                "    {} named by {}",
                missing.artifact,
                missing.mapping.display()
            );
        }
    }
    text
}

fn render_gc(report: &GcReport) -> String {
    let mut text = String::new();
    for candidate in &report.plan.candidates {
        let _ = writeln!(
            text,
            "{:<15} {:>10}  {}",
            candidate.kind,
            human_bytes(candidate.bytes),
            candidate.path.display()
        );
    }
    let count = report.plan.candidates.len();
    if count == 0 {
        let _ = writeln!(
            text,
            "nothing to collect: no unreferenced entry is older than {} days",
            report.max_age_days
        );
    } else if report.applied {
        let _ = writeln!(
            text,
            "removed {count} entries, {}",
            human_bytes(report.reclaimable_bytes)
        );
    } else {
        let _ = writeln!(
            text,
            "{count} entries, {} reclaimable",
            human_bytes(report.reclaimable_bytes)
        );
    }
    for skipped in &report.skipped {
        let _ = writeln!(
            text,
            "skipped {}: {}",
            skipped.path.display(),
            skipped.reason
        );
    }
    for path in &report.shared {
        let _ = writeln!(
            text,
            "shared {}: projects lev has not registered may use it too, and nothing protects their entries",
            path.display()
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::{is_shared, locked_registry};
    use crate::cache::CacheLayout;
    use crate::cache::registry::{self, ProjectRecord};
    use crate::project::Project;

    fn project(root: &Path) -> Project {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("lean-toolchain"),
            "leanprover/lean4:v4.fixture-g\n",
        )
        .unwrap();
        Project::discover(root).unwrap()
    }

    #[test]
    fn the_registry_is_read_again_until_every_cache_it_names_is_locked() {
        let temp = tempfile::tempdir().unwrap();
        let cache = CacheLayout {
            root: temp.path().join("cache"),
        };
        let (first, second) = (temp.path().join("lake-a"), temp.path().join("lake-b"));
        registry::record(&cache, &project(&temp.path().join("a")), Some(&first)).unwrap();
        let mut loads = 0;
        let mut locked = Vec::new();
        let (records, locks) = locked_registry(
            || {
                let records = registry::load(&cache);
                loads += 1;
                if loads == 1 {
                    // Another build registers its project, with another
                    // Lake cache, before the first locks are taken.
                    let other = project(&temp.path().join("b"));
                    registry::record(&cache, &other, Some(&second)).unwrap();
                }
                records
            },
            |path| {
                locked.push(path.to_owned());
                Ok(path.to_owned())
            },
        )
        .unwrap();
        assert_eq!(loads, 3);
        assert_eq!(records.len(), 2);
        assert_eq!(locked, [first.clone(), first.clone(), second.clone()]);
        assert_eq!(locks, [first, second]);
    }

    #[test]
    fn only_a_project_s_own_lake_cache_is_not_shared() {
        let record = |root: &str, lake: &str| -> (PathBuf, ProjectRecord) {
            let record = json!({
                "version": 2,
                "root": root,
                "toolchain": "toolchain",
                "lake_cache": lake,
                "last_used": 0
            });
            (
                PathBuf::from(format!("{root}.json")),
                serde_json::from_value(record).unwrap(),
            )
        };
        let own = Path::new("/p/.lake/cache");
        let default = Path::new("/toolchain/lake/cache");
        let records = [
            record("/p", "/p/.lake/cache"),
            record("/q", "/toolchain/lake/cache"),
        ];
        assert!(!is_shared(&records, own));
        assert!(is_shared(&records, default));
        let borrowed = [
            record("/p", "/p/.lake/cache"),
            record("/r", "/p/.lake/cache"),
        ];
        assert!(is_shared(&borrowed, own));
    }
}
