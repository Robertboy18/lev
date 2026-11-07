//! Incremental external checking of Lean developments.
//!
//! [`run`] reads a `lev-export/1` directory, which holds one LeanExport
//! 3.1.0 file per module with only that module's own constants, and checks
//! every module as a self-contained slice with external kernels. In a slice,
//! imported theorems, opaques and definitions hidden by the module system
//! are axioms with the same name, universe parameters and type; every other
//! imported constant appears in full. Each verdict is cached under the
//! kernel's identity and the slice's canonical content hash, so a proof-only
//! edit rechecks the edited module and the importers that needed the closure
//! view, whose slices copy imported proofs. A linker then checks that the
//! per-module results compose.
//!
//! Components: `read` (export reader), `hash` (canonical hashes and
//! references), `slice` (views and slice keys), `write` (slice files),
//! `kernel` (sandboxed kernel runs), `cache` (verdicts), `link` (rules L1-L8)
//! and `driver` (the run and its report).

mod cache;
mod driver;
#[cfg(test)]
mod fixture;
mod hash;
mod kernel;
mod link;
mod read;
mod slice;
mod write;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;

pub use driver::{
    Attempt, Conclusion, Fallback, KernelReport, Lean, ModuleResult, Rejection, Report,
    SliceCounts, Timings, ViewCounts,
};
pub use kernel::{KernelKind, Verdict};
pub(crate) use kernel::{Sandbox, sandbox_args};
pub use link::Violation;
pub use slice::View;

/// What to check and how.
#[derive(Debug, Clone)]
pub struct Options {
    /// The `lev-export/1` directory to check.
    pub export: PathBuf,
    /// An export of the toolchain's own `Init`: a `lev-export/1` directory or
    /// a plain LeanExport file. Pinned constants, standard axioms and
    /// everything they depend on must match it.
    pub reference: PathBuf,
    /// Kernels that must all accept every module.
    pub kernels: Vec<KernelKind>,
    /// Toolchain root with the kernels in `bin/`. Defaults to the elan
    /// toolchain of the export's Lean version.
    pub toolchain: Option<PathBuf>,
    /// Kernel runs (and parsing threads) at once.
    pub jobs: usize,
    /// Verdict cache directory; `None` disables the cache.
    pub cache: Option<PathBuf>,
    /// Run kernels without the bwrap sandbox.
    pub no_sandbox: bool,
    /// Check only these modules and what their slices rely on: their
    /// imports and the owners of the constants they reference.
    pub only: Vec<String>,
    /// Time limit of one kernel run.
    pub timeout: Duration,
    /// Directory for slice files. Defaults to a temporary directory inside
    /// the cache, or in the system's temporary directory without a cache.
    pub work_dir: Option<PathBuf>,
}

/// One job per CPU, at most 16.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(16)
}

impl Options {
    /// The official kernel, sandboxed, no cache, up to 16 parallel runs.
    pub fn new(export: impl Into<PathBuf>, reference: impl Into<PathBuf>) -> Self {
        Self {
            export: export.into(),
            reference: reference.into(),
            kernels: vec![KernelKind::Official],
            toolchain: None,
            jobs: default_jobs(),
            cache: None,
            no_sandbox: false,
            only: Vec::new(),
            timeout: Duration::from_secs(3600),
            work_dir: None,
        }
    }
}

/// Check a development. The report's verdict is accept when every slice was
/// accepted and every linker rule holds, reject when a kernel rejected a
/// slice in every view or a rule failed, and unknown when nothing was
/// rejected but a kernel run ended in an error, a timeout or a decline.
/// Errors are reserved for unreadable inputs and setup failures.
pub fn run(options: &Options) -> Result<Report> {
    driver::run(options)
}

/// Map `f` over `items` on up to `jobs` scoped threads, keeping input order.
pub(crate) fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    jobs: usize,
    f: impl Fn(usize, &T) -> R + Sync,
) -> Vec<R> {
    let jobs = jobs.clamp(1, items.len().max(1));
    if jobs == 1 {
        return items
            .iter()
            .enumerate()
            .map(|(i, item)| f(i, item))
            .collect();
    }
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break;
                        };
                        out.push((index, f(index, item)));
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}
