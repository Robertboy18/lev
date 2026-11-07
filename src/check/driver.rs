//! Driver: read, hash, check every module's slice with every kernel, link.
//!
//! Each (module, kernel) pair starts with the exported view and falls back
//! to the theorems view and then the closure view when the kernel rejects.
//! A view whose slice equals the previous one is skipped. Verdicts come from
//! the cache when the kernel identity and slice key match. A pair whose last
//! run ends in an error, a timeout or a decline is unchecked, and then the
//! development is neither accepted nor, unless something failed, rejected.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::Options;
use super::cache::Cache;
use super::hash::{self, Index};
use super::kernel::{Kernel, KernelKind, Run, Sandbox, Verdict};
use super::link::{self, Outcome, Pinned, Pins, SliceLink, Violation};
use super::parallel_map;
use super::read::{self, Development, NameId};
use super::slice::{self, Slice, View};
use super::write;
use crate::core::hex;

/// The verdict on the whole development.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Conclusion {
    /// Every selected kernel accepted every slice and every linker rule holds.
    Accept,
    /// A kernel rejected a slice in every view, or a linker rule failed.
    Reject,
    /// Nothing was rejected, but some module could not be checked: a kernel
    /// run ended in an error, a timeout or a decline.
    Unknown,
}

/// Result of [`run`](super::run); serializes as `lev-check/1` JSON.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub format: &'static str,
    pub verdict: Conclusion,
    pub lean: Lean,
    pub kernels: Vec<KernelReport>,
    /// Modules in the export.
    pub modules: usize,
    /// Modules checked: all, or the `only` modules and their imports.
    pub checked_modules: usize,
    pub slices: SliceCounts,
    /// Verdicts of every kernel run or cached verdict, by view.
    pub views: Vec<ViewCounts>,
    /// (module, kernel) pairs that needed a view after `exported`.
    pub fallbacks: Vec<Fallback>,
    /// Every run that did not accept, with the kernel's message.
    pub rejections: Vec<Rejection>,
    /// Modules that a kernel could not check: its last run ended in an
    /// error, a timeout or a decline.
    pub unchecked: Vec<String>,
    /// Real axioms used by the checked modules.
    pub axioms: Vec<String>,
    pub violations: Vec<Violation>,
    pub timings: Timings,
    pub results: Vec<ModuleResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lean {
    pub version: String,
    pub githash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KernelReport {
    pub kernel: KernelKind,
    pub binary: PathBuf,
    pub identity: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SliceCounts {
    /// Kernel runs.
    pub checked: usize,
    /// Verdicts taken from the cache.
    pub reused: usize,
    /// Slice files written, and their total size.
    pub written: usize,
    pub written_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewCounts {
    pub view: View,
    pub accept: usize,
    pub reject: usize,
    pub decline: usize,
    pub error: usize,
}

/// A module whose slice needed a view after `exported`. Expected for files
/// outside the module system and for modules with `import all`, whose build
/// saw more than the exported view shows.
#[derive(Debug, Clone, Serialize)]
pub struct Fallback {
    pub module: String,
    pub kernel: KernelKind,
    pub view: View,
    pub is_module: bool,
    /// Modules imported with `import all`.
    pub imports_all: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rejection {
    pub module: String,
    pub kernel: KernelKind,
    pub view: View,
    pub verdict: Verdict,
    pub exit_code: Option<i32>,
    pub message: String,
}

/// Wall-clock seconds per phase, and summed seconds inside the check phase.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Timings {
    pub read: f64,
    pub hash: f64,
    /// Reading and hashing the reference, alongside `read` and `hash`.
    pub reference: f64,
    pub check: f64,
    pub link: f64,
    pub total: f64,
    /// Summed over tasks: building slices, writing slice files, kernel runs.
    pub slices: f64,
    pub write: f64,
    pub kernels: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModuleResult {
    pub module: String,
    pub kernel: KernelKind,
    pub verdict: Verdict,
    /// The view of the last attempt.
    pub view: Option<View>,
    pub attempts: Vec<Attempt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub view: View,
    /// Slice key, lowercase hex.
    pub key: String,
    /// Constants in the slice.
    pub constants: usize,
    pub verdict: Verdict,
    pub cached: bool,
    pub seconds: f64,
    pub exit_code: Option<i32>,
    pub message: String,
}

struct Checker<'a> {
    dev: &'a Development,
    index: &'a Index,
    pinned: &'a HashSet<NameId>,
    cache: &'a Cache,
    sandbox: &'a Sandbox,
    work: &'a Path,
    timeout: std::time::Duration,
}

/// Slices, files and link summaries of one module, shared by its kernels.
#[derive(Default)]
struct Shared {
    slices: [Option<Result<Arc<Slice>, String>>; 3],
    files: [Option<Result<PathBuf, String>>; 3],
    links: [Option<Arc<SliceLink>>; 3],
}

struct ModuleState {
    shared: Mutex<Shared>,
    remaining: AtomicUsize,
}

#[derive(Default)]
struct Spent {
    slices: f64,
    write: f64,
    kernels: f64,
    runs: usize,
    reused: usize,
    written: usize,
    written_bytes: u64,
}

struct TaskResult {
    module: u32,
    kernel: KernelKind,
    attempts: Vec<Attempt>,
    verdict: Verdict,
    view: Option<View>,
    link: Option<Arc<SliceLink>>,
    spent: Spent,
}

pub(crate) fn run(options: &Options) -> Result<Report> {
    let total = Instant::now();
    let jobs = options.jobs.max(1);

    if options.kernels.is_empty() {
        bail!("no kernel selected");
    }
    let toolchain = match &options.toolchain {
        Some(path) => path.clone(),
        None => default_toolchain(&read::export_lean_version(&options.export)?)?,
    };
    // The reference is usually one large file, read on one thread, and the
    // kernel identities hash whole binaries: both run alongside reading the
    // development. Cached pins skip the reference.
    let cache = Cache::new(options.cache.as_deref())?;
    let (development, pins, kernels) = std::thread::scope(|scope| {
        let reference = scope.spawn(|| reference_pins(options, &cache, jobs));
        let kernels = scope.spawn(|| {
            options
                .kernels
                .iter()
                .map(|kind| Kernel::locate(*kind, &toolchain))
                .collect::<Result<Vec<_>>>()
        });
        let development = (|| -> Result<_> {
            let started = Instant::now();
            let dev = read::read_export(&options.export, jobs)?;
            let read_seconds = started.elapsed().as_secs_f64();
            let started = Instant::now();
            let index = hash::index(&dev, jobs);
            Ok((dev, index, read_seconds, started.elapsed().as_secs_f64()))
        })();
        let pins = joined(reference).with_context(|| {
            format!(
                "failed to read the reference {}",
                options.reference.display()
            )
        });
        (development, pins, joined(kernels))
    });
    let (dev, index, read_seconds, hash_seconds) = development?;
    let (pins, reference_seconds) = pins?;
    let kernels: Vec<Kernel> = kernels?;
    let scope = scope(&dev, &index, &options.only)?;
    let sandbox = if options.no_sandbox {
        Sandbox::None
    } else {
        Sandbox::bwrap()?
    };
    let work = match (&options.work_dir, &options.cache) {
        (Some(dir), _) | (None, Some(dir)) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            tempfile::Builder::new()
                .prefix("lev-check-")
                .tempdir_in(dir)
        }
        (None, None) => tempfile::Builder::new().prefix("lev-check-").tempdir(),
    }
    .context("failed to create a work directory for slice files")?;

    let started = Instant::now();
    let pinned = Pinned::new(&dev, &pins);
    let cx = Checker {
        dev: &dev,
        index: &index,
        pinned: &pinned.names,
        cache: &cache,
        sandbox: &sandbox,
        work: work.path(),
        timeout: options.timeout,
    };
    let states: Vec<ModuleState> = scope
        .iter()
        .map(|_| ModuleState {
            shared: Mutex::new(Shared::default()),
            remaining: AtomicUsize::new(kernels.len()),
        })
        .collect();
    let tasks: Vec<(usize, &Kernel)> = (0..scope.len())
        .flat_map(|position| kernels.iter().map(move |kernel| (position, kernel)))
        .collect();
    let results = parallel_map(&tasks, jobs, |_, (position, kernel)| {
        let state = &states[*position];
        let result = check(&cx, state, scope[*position], *position, kernel);
        if state.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut shared = state.shared.lock().unwrap_or_else(PoisonError::into_inner);
            for file in shared.files.iter().flatten().flatten() {
                let _ = std::fs::remove_file(file);
            }
            *shared = Shared::default();
        }
        result
    });
    let check_seconds = started.elapsed().as_secs_f64();

    let started = Instant::now();
    let outcomes: Vec<Outcome<'_>> = results
        .iter()
        .map(|result| Outcome {
            module: result.module,
            kernel: result.kernel,
            verdict: result.verdict,
            slice: result.link.as_deref(),
        })
        .collect();
    let linked = link::link(&dev, &index, &pins, &pinned, &scope, &outcomes);
    let link_seconds = started.elapsed().as_secs_f64();

    let timings = Timings {
        read: read_seconds,
        hash: hash_seconds,
        reference: reference_seconds,
        check: check_seconds,
        link: link_seconds,
        ..Timings::default()
    };
    let mut report = assemble(&dev, &kernels, scope.len(), results, linked, timings);
    report.timings.total = total.elapsed().as_secs_f64();
    Ok(report)
}

/// The report: the verdict, per-module results, and their summaries.
fn assemble(
    dev: &Development,
    kernels: &[Kernel],
    checked_modules: usize,
    results: Vec<TaskResult>,
    linked: link::Linked,
    timings: Timings,
) -> Report {
    let mut unchecked: Vec<String> = results
        .iter()
        .filter(|result| matches!(result.verdict, Verdict::Error | Verdict::Decline))
        .map(|result| dev.modules[result.module as usize].name.clone())
        .collect();
    unchecked.dedup();
    let verdict = if !linked.violations.is_empty() {
        Conclusion::Reject
    } else if unchecked.is_empty() {
        Conclusion::Accept
    } else {
        Conclusion::Unknown
    };
    let mut report = Report {
        format: "lev-check/1",
        verdict,
        lean: Lean {
            version: dev.lean_version.clone(),
            githash: dev.githash.clone(),
        },
        kernels: kernels
            .iter()
            .map(|kernel| KernelReport {
                kernel: kernel.kind,
                binary: kernel.exe.clone(),
                identity: kernel.identity.clone(),
            })
            .collect(),
        modules: dev.modules.len(),
        checked_modules,
        slices: SliceCounts::default(),
        views: View::ALL
            .into_iter()
            .map(|view| ViewCounts {
                view,
                accept: 0,
                reject: 0,
                decline: 0,
                error: 0,
            })
            .collect(),
        fallbacks: Vec::new(),
        rejections: Vec::new(),
        unchecked,
        axioms: linked.axioms,
        violations: linked.violations,
        timings,
        results: Vec::new(),
    };
    for result in results {
        let module = dev.modules[result.module as usize].name.clone();
        report.timings.slices += result.spent.slices;
        report.timings.write += result.spent.write;
        report.timings.kernels += result.spent.kernels;
        report.slices.written += result.spent.written;
        report.slices.written_bytes += result.spent.written_bytes;
        report.slices.checked += result.spent.runs;
        report.slices.reused += result.spent.reused;
        for attempt in &result.attempts {
            if let Some(counts) = report.views.iter_mut().find(|c| c.view == attempt.view) {
                match attempt.verdict {
                    Verdict::Accept => counts.accept += 1,
                    Verdict::Reject => counts.reject += 1,
                    Verdict::Decline => counts.decline += 1,
                    Verdict::Error => counts.error += 1,
                }
            }
            if attempt.verdict != Verdict::Accept {
                report.rejections.push(Rejection {
                    module: module.clone(),
                    kernel: result.kernel,
                    view: attempt.view,
                    verdict: attempt.verdict,
                    exit_code: attempt.exit_code,
                    message: attempt.message.clone(),
                });
            }
        }
        if let (Verdict::Accept, Some(view)) = (result.verdict, result.view) {
            if view != View::Exported {
                let source = &dev.modules[result.module as usize];
                report.fallbacks.push(Fallback {
                    module: module.clone(),
                    kernel: result.kernel,
                    view,
                    is_module: source.is_module,
                    imports_all: source
                        .imports
                        .iter()
                        .filter(|import| import.all)
                        .map(|import| import.module.clone())
                        .collect(),
                });
            }
        }
        report.results.push(ModuleResult {
            module,
            kernel: result.kernel,
            verdict: result.verdict,
            view: result.view,
            attempts: result.attempts,
        });
    }
    report
}

/// Check one module with one kernel, falling back to larger views on reject.
fn check(
    cx: &Checker<'_>,
    state: &ModuleState,
    module: u32,
    position: usize,
    kernel: &Kernel,
) -> TaskResult {
    let mut spent = Spent::default();
    let mut attempts = Vec::new();
    let mut previous: Option<Arc<Slice>> = None;
    let mut last: Option<(View, Verdict)> = None;
    for view in View::ALL {
        let slice = match slice_for(cx, state, module, view, &mut spent) {
            Ok(slice) => slice,
            Err(message) => {
                attempts.push(failed(view, String::new(), 0, message));
                last = Some((view, Verdict::Error));
                break;
            }
        };
        let key = hex::lowercase(&slice.key);
        if !slice.missing.is_empty() {
            let message = format!(
                "{} referenced constants are declared by no module",
                slice.missing.len()
            );
            attempts.push(failed(view, key, slice.entries.len(), message));
            last = Some((view, Verdict::Error));
            break;
        }
        // A larger view that changes nothing would only repeat the rejection.
        if previous
            .as_ref()
            .is_some_and(|previous| previous.entries == slice.entries)
        {
            continue;
        }
        previous = Some(Arc::clone(&slice));
        let (run, cached) = match cx.cache.get(kernel, &slice.key) {
            Some(run) => {
                spent.reused += 1;
                (run, true)
            }
            None => {
                let mut run = match file_for(cx, state, position, view, &slice, &mut spent) {
                    Ok(path) => {
                        spent.runs += 1;
                        kernel
                            .run(
                                &path,
                                &slice.assumptions(cx.dev),
                                cx.work,
                                cx.sandbox,
                                cx.timeout,
                            )
                            .unwrap_or_else(|error| error_run(format!("{error:#}")))
                    }
                    Err(message) => error_run(message),
                };
                spent.kernels += run.seconds;
                let module_name = &cx.dev.modules[module as usize].name;
                if let Err(error) = cx.cache.put(kernel, &slice.key, module_name, view, &run) {
                    run.message = format!("{}\n(not cached: {error:#})", run.message);
                }
                (run, false)
            }
        };
        attempts.push(Attempt {
            view,
            key,
            constants: slice.entries.len(),
            verdict: run.verdict,
            cached,
            seconds: run.seconds,
            exit_code: run.exit_code,
            message: run.message,
        });
        last = Some((view, run.verdict));
        if run.verdict != Verdict::Reject {
            break;
        }
    }
    let (view, verdict) = match last {
        Some((view, verdict)) => (Some(view), verdict),
        None => (None, Verdict::Error),
    };
    let link = view.and_then(|view| link_for(cx, state, view));
    TaskResult {
        module,
        kernel: kernel.kind,
        attempts,
        verdict,
        view,
        link,
        spent,
    }
}

fn failed(view: View, key: String, constants: usize, message: String) -> Attempt {
    Attempt {
        view,
        key,
        constants,
        verdict: Verdict::Error,
        cached: false,
        seconds: 0.0,
        exit_code: None,
        message,
    }
}

fn error_run(message: String) -> Run {
    Run {
        verdict: Verdict::Error,
        exit_code: None,
        seconds: 0.0,
        message,
    }
}

fn view_slot(view: View) -> usize {
    match view {
        View::Exported => 0,
        View::Theorems => 1,
        View::Closure => 2,
    }
}

fn slice_for(
    cx: &Checker<'_>,
    state: &ModuleState,
    module: u32,
    view: View,
    spent: &mut Spent,
) -> Result<Arc<Slice>, String> {
    let mut shared = state.shared.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = &mut shared.slices[view_slot(view)];
    if slot.is_none() {
        let started = Instant::now();
        *slot = Some(
            slice::build(cx.dev, cx.index, cx.pinned, module, view)
                .map(Arc::new)
                .map_err(|error| format!("{error:#}")),
        );
        spent.slices += started.elapsed().as_secs_f64();
    }
    slot.clone().unwrap_or_else(|| Err("no slice".to_owned()))
}

fn file_for(
    cx: &Checker<'_>,
    state: &ModuleState,
    position: usize,
    view: View,
    slice: &Slice,
    spent: &mut Spent,
) -> Result<PathBuf, String> {
    let mut shared = state.shared.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = &mut shared.files[view_slot(view)];
    if slot.is_none() {
        let started = Instant::now();
        let path = cx.work.join(format!("{position}-{}.ndjson", view.as_str()));
        *slot = Some(match write::write(cx.dev, cx.index, slice, &path) {
            Ok(written) => {
                spent.written += 1;
                spent.written_bytes += written.bytes;
                Ok(path)
            }
            Err(error) => Err(format!("failed to write the slice: {error:#}")),
        });
        spent.write += started.elapsed().as_secs_f64();
    }
    slot.clone()
        .unwrap_or_else(|| Err("no slice file".to_owned()))
}

fn link_for(cx: &Checker<'_>, state: &ModuleState, view: View) -> Option<Arc<SliceLink>> {
    let mut shared = state.shared.lock().unwrap_or_else(PoisonError::into_inner);
    let slot = view_slot(view);
    if shared.links[slot].is_none() {
        let slice = shared.slices[slot].clone()?.ok()?;
        shared.links[slot] = Some(Arc::new(link::check_slice(
            cx.dev, cx.index, cx.pinned, &slice,
        )));
    }
    shared.links[slot].clone()
}

fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Pins of the reference and the seconds spent on them, from the cache when
/// the reference's bytes are unchanged.
fn reference_pins(options: &Options, cache: &Cache, jobs: usize) -> Result<(Pins, f64)> {
    let started = Instant::now();
    let digest = match options.cache {
        Some(_) => Some(read::reference_digest(&options.reference)?),
        None => None,
    };
    if let Some(pins) = digest.as_deref().and_then(|digest| cache.pins(digest)) {
        return Ok((pins, started.elapsed().as_secs_f64()));
    }
    let reference = read::read_reference(&options.reference, jobs)?;
    let pins = Pins::new(&reference, &hash::index(&reference, jobs));
    if let Some(digest) = &digest {
        cache.put_pins(digest, &pins)?;
    }
    Ok((pins, started.elapsed().as_secs_f64()))
}

/// All modules, or the `only` modules and everything their slices rely on:
/// their imports, and the owners of the constants they reference (the
/// earliest owner of a theorem declared twice need not be an import).
fn scope(dev: &Development, index: &Index, only: &[String]) -> Result<Vec<u32>> {
    let count = dev.modules.len();
    if only.is_empty() {
        return Ok((0..count as u32).collect());
    }
    let mut selected = vec![false; count];
    let mut queue = Vec::new();
    for name in only {
        let module = dev
            .module_index(name)
            .with_context(|| format!("module {name} is not in the export"))?;
        queue.push(module);
    }
    while let Some(module) = queue.pop() {
        if std::mem::replace(&mut selected[module as usize], true) {
            continue;
        }
        for import in &dev.modules[module as usize].imports {
            queue.extend(dev.module_index(&import.module));
        }
        for unit in &index.modules[module as usize].units {
            for name in &unit.refs {
                queue.extend(dev.owner_for(module, *name).map(|owner| owner.module));
            }
        }
    }
    Ok((0..count as u32)
        .filter(|module| selected[*module as usize])
        .collect())
}

/// The elan toolchain of a released Lean version.
fn default_toolchain(version: &str) -> Result<PathBuf> {
    let elan = match std::env::var_os("ELAN_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".elan"),
    };
    let path = elan
        .join("toolchains")
        .join(format!("leanprover--lean4---v{version}"));
    if !path.join("bin").is_dir() {
        bail!(
            "no toolchain for Lean {version} at {}; pass the toolchain directory",
            path.display()
        );
    }
    Ok(path)
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    use super::super::fixture;
    use super::super::{Conclusion, KernelKind, Options, Verdict, View, run};

    /// A stand-in for leanchecker that rejects every slice with an axiom record.
    const FAKE_KERNEL: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'{"axiom"'*) echo "saw an axiom"; exit 1 ;;
  esac
done < "$2"
exit 0
"##;

    /// A stand-in for leanchecker that outlives any time limit on Top's
    /// slices and rejects every other slice.
    const SLOW_ON_TOP: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"str":"Top"'*) exec sleep 60 ;;
  esac
done < "$2"
echo "rejected"
exit 1
"##;

    /// The Base and Top fixture, checked by `kernel` standing in for leanchecker.
    fn with_kernel(dir: &Path, kernel: &str) -> Options {
        let (base, top) = fixture::base_and_top();
        fixture::write_export(
            &dir.join("export"),
            &[("Base", &[], &base), ("Top", &["Base"], &top)],
        );
        let reference = fixture::reference(dir);
        let toolchain = dir.join("toolchain");
        fs::create_dir_all(toolchain.join("bin")).expect("bin");
        fs::create_dir_all(toolchain.join("lib").join("lean")).expect("lib");
        let exe = toolchain.join("bin").join("leanchecker");
        fs::write(&exe, kernel).expect("kernel");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod");

        let mut options = Options::new(dir.join("export"), &reference);
        options.toolchain = Some(toolchain);
        options.no_sandbox = true;
        options.cache = Some(dir.join("cache"));
        options.jobs = 2;
        options.kernels = vec![KernelKind::Official];
        options
    }

    #[test]
    fn falls_back_on_rejection_and_reuses_cached_verdicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut options = with_kernel(dir.path(), FAKE_KERNEL);

        let report = run(&options).expect("run");
        assert_eq!(
            report.verdict,
            Conclusion::Accept,
            "{:?}",
            report.violations
        );
        let top = report
            .results
            .iter()
            .find(|result| result.module == "Top")
            .expect("Top");
        assert_eq!(top.view, Some(View::Closure));
        let verdicts: Vec<Verdict> = top.attempts.iter().map(|attempt| attempt.verdict).collect();
        assert_eq!(
            verdicts,
            [Verdict::Reject, Verdict::Reject, Verdict::Accept]
        );
        assert_eq!(report.fallbacks.len(), 1);
        assert_eq!(report.slices.checked, 4);
        assert_eq!(report.slices.reused, 0);
        assert_eq!(report.rejections.len(), 2);
        assert!(report.rejections[0].message.contains("saw an axiom"));
        assert!(report.axioms.is_empty());

        let again = run(&options).expect("second run");
        assert_eq!(again.verdict, Conclusion::Accept);
        assert_eq!(again.slices.checked, 0);
        assert_eq!(again.slices.reused, 4);

        // `only` checks the named modules and their imports, nothing else.
        options.cache = None;
        options.only = vec!["Base".to_owned()];
        let only = run(&options).expect("only");
        assert_eq!(only.checked_modules, 1);
        assert_eq!(only.verdict, Conclusion::Accept);
    }

    #[test]
    fn a_kernel_timeout_leaves_the_result_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut options = with_kernel(dir.path(), "#!/bin/sh\nexec sleep 60\n");
        options.timeout = Duration::from_millis(300);

        let report = run(&options).expect("run");
        assert_eq!(report.verdict, Conclusion::Unknown, "{report:#?}");
        assert!(report.violations.is_empty(), "{:?}", report.violations);
        assert_eq!(report.unchecked, ["Base", "Top"]);
        for result in &report.results {
            assert_eq!(result.verdict, Verdict::Error);
            // A timeout is not a rejection, so no larger view is tried.
            assert_eq!(result.view, Some(View::Exported));
            assert!(result.attempts[0].message.starts_with("timed out"));
        }
        // Timeouts are not cached, so the next run tries again.
        let again = run(&options).expect("second run");
        assert_eq!(again.slices.reused, 0);
        assert_eq!(again.verdict, Conclusion::Unknown);
    }

    #[test]
    fn a_rejection_outweighs_a_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut options = with_kernel(dir.path(), SLOW_ON_TOP);
        options.timeout = Duration::from_millis(300);

        let report = run(&options).expect("run");
        assert_eq!(report.verdict, Conclusion::Reject, "{report:#?}");
        assert_eq!(report.unchecked, ["Top"]);
        let rules: Vec<_> = report
            .violations
            .iter()
            .map(|violation| (violation.rule, violation.module.as_deref()))
            .collect();
        // Base is rejected in every view, and Top relies on its theorems.
        assert_eq!(rules, [("L2", Some("Top")), ("L5", Some("Base"))]);
    }
}
