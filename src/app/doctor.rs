//! `lev doctor`: the facts a build or a kernel check depends on, each stated
//! once, followed by the problems they reveal.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::{AppContext, exporter};
use crate::cache::lake::Source;
use crate::cache::registry;
use crate::cli::DoctorArgs;
use crate::core::json_output::{self, schema};
use crate::core::process::{checked_output, output_text};

/// Kernels a toolchain may ship for checking exported environments.
const KERNELS: &[&str] = &[
    "leanchecker",
    "leanchecker-paranoid",
    "lean4lean",
    "nanoda_bin",
];

#[derive(Debug, Serialize)]
struct Report {
    lev_version: &'static str,
    host: String,
    project: Option<ProjectFacts>,
    elan: Option<String>,
    lean: Option<String>,
    lake: Option<String>,
    git: Option<String>,
    bwrap: Option<String>,
    /// Each kernel binary in the toolchain's `bin`, or null when it is absent.
    kernels: BTreeMap<&'static str, Option<PathBuf>>,
    /// What `lev check` has already built for the project's toolchain.
    check: Option<CheckFacts>,
    lake_cache: Option<LakeCacheFacts>,
    lev_cache: LevCacheFacts,
    problems: Vec<Problem>,
}

#[derive(Debug, Serialize)]
struct ProjectFacts {
    root: PathBuf,
    toolchain: String,
}

#[derive(Debug, Serialize)]
struct CheckFacts {
    /// The lev-export binary, or null until the first check builds it.
    exporter: Option<PathBuf>,
    /// The export of the toolchain's `Init` that pins the kernel's
    /// primitives, or null until the first check makes it.
    reference: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct LakeCacheFacts {
    path: PathBuf,
    source: Source,
    exists: bool,
    /// Lake hard-links artifacts only within one filesystem; across
    /// filesystems it copies them. Null when the platform cannot tell.
    same_filesystem: Option<bool>,
}

#[derive(Debug, Serialize)]
struct LevCacheFacts {
    path: PathBuf,
    registered_projects: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Serialize)]
struct Problem {
    severity: Severity,
    message: String,
}

pub(super) fn run(context: &AppContext, args: DoctorArgs) -> Result<i32> {
    let report = inspect(context);
    if args.json {
        json_output::print(schema::DOCTOR, &report)?;
    } else {
        io::stdout()
            .lock()
            .write_all(render(&report).as_bytes())
            .context("failed to write the doctor report")?;
    }
    Ok(0)
}

fn inspect(context: &AppContext) -> Report {
    let mut problems = Vec::new();
    let mut problem = |severity, message: String| problems.push(Problem { severity, message });

    let elan = version(Command::new(&context.elan).arg("--version"))
        .map_err(|error| {
            problem(
                Severity::Error,
                format!("elan is not available, and lev runs Lean through it: {error:#}"),
            )
        })
        .ok();
    let git = version(Command::new(&context.git).arg("--version"))
        .map_err(|error| {
            problem(
                Severity::Warning,
                format!(
                    "git is not available, and Lake needs it to fetch dependencies: \
                     {error:#}"
                ),
            )
        })
        .ok();
    let bwrap = version(Command::new("bwrap").arg("--version"))
        .map_err(|error| {
            if cfg!(target_os = "linux") {
                problem(
                    Severity::Warning,
                    format!(
                        "bwrap is not available, and `lev check` and `lake check` need it \
                         to sandbox the exporter and the kernels: {error:#}"
                    ),
                )
            }
        })
        .ok();

    let project = match context.project() {
        Ok(project) => Some(project),
        Err(error) => {
            if context
                .project_start
                .ancestors()
                .any(|directory| directory.join("lean-toolchain").is_file())
            {
                problem(Severity::Error, format!("{error:#}"));
            }
            None
        }
    };

    let mut lean = None;
    let mut lake = None;
    let mut kernels = BTreeMap::new();
    let mut lake_cache = None;
    if let (Some(project), true) = (&project, elan.is_some()) {
        let toolchain = &project.toolchain;
        match context.toolchain_root(toolchain) {
            Ok(root) => {
                lean = version(context.elan_run(toolchain, "lean", false).arg("--version"))
                    .map_err(|error| problem(Severity::Error, format!("{error:#}")))
                    .ok();
                lake = version(context.elan_run(toolchain, "lake", false).arg("--version"))
                    .map_err(|error| problem(Severity::Error, format!("{error:#}")))
                    .ok();
                for kernel in KERNELS {
                    let path = root
                        .join("bin")
                        .join(format!("{kernel}{}", std::env::consts::EXE_SUFFIX));
                    kernels.insert(*kernel, path.is_file().then_some(path));
                }
            }
            Err(error) => problem(
                Severity::Error,
                format!("elan cannot find the toolchain {toolchain}: {error:#}"),
            ),
        }
        // Without LAKE_CACHE_DIR the cache lives in the toolchain, whose
        // absence is reported above.
        if let Ok(location) = context.lake_cache(project) {
            let same_filesystem = same_filesystem(&project.root, &location.path);
            if same_filesystem == Some(false) {
                problem(
                    Severity::Warning,
                    format!(
                        "the Lake cache {} is on another filesystem than the project, so Lake \
                         copies artifacts instead of hard-linking them; point LAKE_CACHE_DIR at \
                         a directory on the project's filesystem to share them",
                        location.path.display()
                    ),
                );
            }
            lake_cache = Some(LakeCacheFacts {
                exists: location.path.is_dir(),
                path: location.path,
                source: location.source,
                same_filesystem,
            });
        }
    }

    let check = project.as_ref().map(|project| {
        let (exporter, reference) = exporter::built(&context.cache.check(), &project.toolchain);
        CheckFacts {
            exporter,
            reference,
        }
    });

    let registered_projects = registry::load(&context.cache)
        .map(|records| records.len())
        .map_err(|error| {
            problem(
                Severity::Warning,
                format!("lev cache status and gc cannot run until this is fixed: {error:#}"),
            )
        })
        .ok();

    Report {
        lev_version: env!("CARGO_PKG_VERSION"),
        host: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        project: project.map(|project| ProjectFacts {
            root: project.root,
            toolchain: project.toolchain,
        }),
        elan,
        lean,
        lake,
        git,
        bwrap,
        kernels,
        check,
        lake_cache,
        lev_cache: LevCacheFacts {
            path: context.cache.root.clone(),
            registered_projects,
        },
        problems,
    }
}

fn version(command: &mut Command) -> Result<String> {
    let text = output_text(checked_output(command)?);
    match text.lines().next() {
        Some(line) if !line.trim().is_empty() => Ok(line.trim().to_owned()),
        _ => bail!("the version command printed nothing"),
    }
}

/// Compare the devices of two paths, using the nearest existing ancestor of a
/// path that does not exist yet.
#[cfg(unix)]
fn same_filesystem(left: &Path, right: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;

    let device = |path: &Path| {
        path.ancestors()
            .find_map(|directory| std::fs::metadata(directory).ok())
            .map(|metadata| metadata.dev())
    };
    Some(device(left)? == device(right)?)
}

#[cfg(not(unix))]
fn same_filesystem(_left: &Path, _right: &Path) -> Option<bool> {
    None
}

fn render(report: &Report) -> String {
    let mut text = format!("lev {} on {}\n", report.lev_version, report.host);
    let mut line = |label: &str, value: &str| {
        let _ = writeln!(text, "{label:<11} {value}");
    };
    let unavailable =
        |value: &Option<String>| value.clone().unwrap_or_else(|| "unavailable".to_owned());

    match &report.project {
        Some(project) => line(
            "project",
            &format!("{} ({})", project.root.display(), project.toolchain),
        ),
        None => line("project", "none found"),
    }
    line("elan", &unavailable(&report.elan));
    if report.project.is_some() {
        line("lean", &unavailable(&report.lean));
        line("lake", &unavailable(&report.lake));
    }
    line("git", &unavailable(&report.git));
    line("bwrap", &unavailable(&report.bwrap));
    if !report.kernels.is_empty() {
        let present = report
            .kernels
            .iter()
            .filter(|(_, path)| path.is_some())
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        let missing = report
            .kernels
            .iter()
            .filter(|(_, path)| path.is_none())
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        let mut value = if present.is_empty() {
            "none".to_owned()
        } else {
            present.join(", ")
        };
        if !missing.is_empty() {
            let _ = write!(value, " (missing {})", missing.join(", "));
        }
        line("kernels", &value);
    }
    if let Some(check) = &report.check {
        let state = match (&check.exporter, &check.reference) {
            (Some(_), Some(_)) => "lev-export and the Init reference are built",
            (Some(_), None) => "lev-export is built; the first check exports the Init reference",
            _ => "nothing built yet; the first check builds lev-export and exports Init",
        };
        line("check", state);
    }
    if let Some(cache) = &report.lake_cache {
        let source = match cache.source {
            Source::Environment => "from LAKE_CACHE_DIR",
            Source::Workspace => "per workspace, LAKE_CACHE_DIR is empty",
            Source::Toolchain => "toolchain default",
        };
        let filesystem = match cache.same_filesystem {
            Some(true) => ", same filesystem as the project",
            Some(false) => ", another filesystem than the project",
            None => "",
        };
        let created = if cache.exists {
            ""
        } else {
            ", not created yet"
        };
        line(
            "lake cache",
            &format!("{} ({source}{filesystem}{created})", cache.path.display()),
        );
    }
    let projects = match report.lev_cache.registered_projects {
        Some(1) => "1 registered project".to_owned(),
        Some(count) => format!("{count} registered projects"),
        None => "registry unreadable".to_owned(),
    };
    line(
        "lev cache",
        &format!("{} ({projects})", report.lev_cache.path.display()),
    );

    if !report.problems.is_empty() {
        text.push('\n');
    }
    for problem in &report.problems {
        let label = match problem.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        let _ = writeln!(text, "{label}: {}", problem.message);
    }
    text
}
