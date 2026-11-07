//! `lev check`: export every module of a project, check each module's slice
//! with external kernels, reuse the verdicts of slices that did not change,
//! and check that the per-module results compose.
//!
//! Exit status: 0 when the development is accepted, 1 when a kernel rejects a
//! slice in every view or a linker rule fails, 2 when lev could not check it:
//! a setup error, or a kernel run that ended in an error, a timeout or a
//! decline.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::AppContext;
use super::exporter::{self, Workspace};
use crate::cache::registry;
use crate::check::{self, Conclusion, KernelKind, Report, Sandbox, Verdict};
use crate::cli::{CheckArgs, CheckKernel};
use crate::core::json_output::{self, schema};
use crate::core::lock;
use crate::core::process::{describe, exit_code};
use crate::project::Project;

/// How many entries a text list shows before it says how many it left out.
const TEXT_LIMIT: usize = 20;

#[derive(Debug, Serialize)]
struct CheckReport {
    project: PathBuf,
    toolchain: String,
    /// Whether lev ran `lake build` before exporting.
    built: bool,
    sandboxed: bool,
    /// The modules of the root package's default targets, exported with
    /// their imports.
    roots: Vec<String>,
    exporter: PathBuf,
    reference: PathBuf,
    seconds: Seconds,
    /// The engine's report (`lev-check/1`).
    check: Report,
}

#[derive(Debug, Default, Serialize)]
struct Seconds {
    build: f64,
    /// Building lev-export, when no earlier check did.
    exporter: f64,
    /// Exporting the toolchain's `Init`, when no earlier check did.
    reference: f64,
    export: f64,
    check: f64,
    total: f64,
}

pub(super) fn run(context: &AppContext, args: CheckArgs) -> Result<i32> {
    let report = match check_project(context, &args) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("lev: error: {error:#}");
            return Ok(2);
        }
    };
    if args.json {
        json_output::print(schema::CHECK, &report)?;
    } else {
        io::stdout()
            .lock()
            .write_all(render(&report).as_bytes())
            .context("failed to write the check report")?;
    }
    Ok(exit_status(report.check.verdict))
}

/// 0 accept, 1 reject, 2 when some module could not be checked.
fn exit_status(verdict: Conclusion) -> i32 {
    match verdict {
        Conclusion::Accept => 0,
        Conclusion::Reject => 1,
        Conclusion::Unknown => 2,
    }
}

fn check_project(context: &AppContext, args: &CheckArgs) -> Result<CheckReport> {
    let started = Instant::now();
    let project = context.project()?;
    // Before the build, so a missing bwrap does not cost one.
    let sandbox = if args.no_sandbox {
        Sandbox::None
    } else {
        Sandbox::bwrap()?
    };
    let layout = context.cache.check();
    // Collection waits for running checks, and a check for a running collection.
    let _check_lock = lock::shared(&layout.lock)?;
    let lake_cache = context
        .lake_cache(&project)
        .ok()
        .map(|location| location.path);
    if let Err(error) = registry::record(&context.cache, &project, lake_cache.as_deref()) {
        context.info(format!("could not record the project: {error:#}"));
    }
    let mut seconds = Seconds::default();

    if !args.no_build {
        let _lake_lock = lake_cache
            .as_ref()
            .and_then(|path| lock::shared(&context.cache.lake_lock_path(path)).ok());
        let begun = Instant::now();
        build(context, &project)?;
        seconds.build = begun.elapsed().as_secs_f64();
        // elan may have installed the toolchain, and with it Lake's cache.
        if lake_cache.is_none() {
            if let Ok(location) = context.lake_cache(&project) {
                let _ = registry::record(&context.cache, &project, Some(&location.path));
            }
        }
    }
    let toolchain = context.toolchain_root(&project.toolchain)?;
    let jobs = args.jobs.unwrap_or_else(check::default_jobs);

    let begun = Instant::now();
    let exporter = exporter::ensure(context, &layout, &project.toolchain)?;
    seconds.exporter = begun.elapsed().as_secs_f64();
    let begun = Instant::now();
    let reference = exporter::reference(context, &layout, &exporter, &toolchain, &sandbox, jobs)?;
    seconds.reference = begun.elapsed().as_secs_f64();

    let begun = Instant::now();
    let workspace = Workspace::new(&project, &toolchain)?;
    let roots = workspace.default_modules(&sandbox)?;
    let export = exporter::scratch_dir(&layout, "export-")?;
    context.info(format!(
        "exporting {} with {} root modules",
        project.root.display(),
        roots.len()
    ));
    workspace.export(context, &exporter, &roots, export.path(), jobs, &sandbox)?;
    seconds.export = begun.elapsed().as_secs_f64();

    let mut options = check::Options::new(export.path(), &reference);
    options.kernels = kernels(&args.kernels);
    options.toolchain = Some(toolchain);
    options.jobs = jobs;
    options.cache = Some(layout.verdicts());
    options.no_sandbox = args.no_sandbox;
    options.only = args.only.clone();
    options.timeout = Duration::from_secs(args.timeout);
    options.work_dir = Some(layout.scratch());
    let begun = Instant::now();
    let report = check::run(&options)?;
    seconds.check = begun.elapsed().as_secs_f64();
    seconds.total = started.elapsed().as_secs_f64();

    Ok(CheckReport {
        project: project.root,
        toolchain: project.toolchain,
        built: !args.no_build,
        sandboxed: !args.no_sandbox,
        roots,
        exporter: exporter.binary,
        reference,
        seconds,
        check: report,
    })
}

/// `lake build` through elan, its output on stderr so stdout stays the report.
fn build(context: &AppContext, project: &Project) -> Result<()> {
    let mut command = context.elan_run(&project.toolchain, "lake", true);
    command
        .arg("build")
        .current_dir(&project.root)
        .stdin(Stdio::null());
    let description = describe(&command);
    context.detail(format!("running {description}"));
    let status = if context.quiet {
        let output = command
            .output()
            .with_context(|| format!("failed to start {description}"))?;
        if !output.status.success() {
            let mut stderr = io::stderr().lock();
            let _ = stderr.write_all(&output.stdout);
            let _ = stderr.write_all(&output.stderr);
        }
        output.status
    } else {
        command
            .stdout(io::stderr())
            .stderr(io::stderr())
            .status()
            .with_context(|| format!("failed to start {description}"))?
    };
    if !status.success() {
        bail!(
            "`lake build` failed with exit code {}; fix the build or pass --no-build",
            exit_code(status)
        );
    }
    Ok(())
}

/// The selected kernels in order, the official one when none is selected.
fn kernels(selected: &[CheckKernel]) -> Vec<KernelKind> {
    let mut kernels = Vec::new();
    for kernel in selected {
        let kind = match kernel {
            CheckKernel::Official => KernelKind::Official,
            CheckKernel::Paranoid => KernelKind::Paranoid,
            CheckKernel::Nanoda => KernelKind::Nanoda,
        };
        if !kernels.contains(&kind) {
            kernels.push(kind);
        }
    }
    if kernels.is_empty() {
        kernels.push(KernelKind::Official);
    }
    kernels
}

fn render(report: &CheckReport) -> String {
    let check = &report.check;
    let kernels = check
        .kernels
        .iter()
        .map(|kernel| kernel.kernel.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let checked = format!(
        "{} of {} modules checked with {kernels}",
        check.checked_modules, check.modules
    );
    let mut text = match check.verdict {
        Conclusion::Accept => format!("accept: {checked}\n"),
        Conclusion::Reject => format!("reject: {checked}\n"),
        Conclusion::Unknown => format!(
            "unknown: {} of {} modules could not be checked with {kernels}\n",
            check.unchecked.len(),
            check.checked_modules
        ),
    };
    let _ = writeln!(
        text,
        "slices: {} checked, {} reused",
        check.slices.checked, check.slices.reused
    );
    if !check.fallbacks.is_empty() {
        let shown = check
            .fallbacks
            .iter()
            .map(|fallback| {
                format!(
                    "{} ({} view, {})",
                    fallback.module,
                    fallback.view.as_str(),
                    fallback.kernel.as_str()
                )
            })
            .collect::<Vec<_>>();
        list(&mut text, "fallbacks", &shown);
    }
    let axioms = if check.axioms.is_empty() {
        "none".to_owned()
    } else {
        check.axioms.join(", ")
    };
    let _ = writeln!(text, "axioms: {axioms}");

    let seconds = &report.seconds;
    let mut parts = Vec::new();
    if report.built {
        parts.push(format!("build {:.1} s", seconds.build));
    }
    for (label, value) in [
        ("exporter build", seconds.exporter),
        ("reference", seconds.reference),
    ] {
        if value >= 0.5 {
            parts.push(format!("{label} {value:.1} s"));
        }
    }
    parts.push(format!("export {:.1} s", seconds.export));
    parts.push(format!(
        "check {:.1} s (kernels {:.1} s summed)",
        seconds.check, check.timings.kernels
    ));
    let _ = writeln!(text, "time: {}", parts.join(", "));

    if !check.violations.is_empty() {
        let shown = check
            .violations
            .iter()
            .map(|violation| match &violation.module {
                Some(module) => format!("{} {module}: {}", violation.rule, violation.message),
                None => format!("{} {}", violation.rule, violation.message),
            })
            .collect::<Vec<_>>();
        block(&mut text, "violations", &shown);
    }
    // A kernel's last word on each module it rejected or could not check.
    let results = |wanted: &[Verdict]| {
        check
            .results
            .iter()
            .filter(|result| wanted.contains(&result.verdict))
            .map(|result| {
                let last = result.attempts.last();
                let message = last
                    .and_then(|attempt| {
                        attempt
                            .message
                            .lines()
                            .rev()
                            .find(|line| !line.trim().is_empty())
                    })
                    .unwrap_or("no message");
                let view = result.view.map_or("no", |view| view.as_str());
                format!(
                    "{} ({}, {view} view): {message}",
                    result.module,
                    result.kernel.as_str()
                )
            })
            .collect::<Vec<_>>()
    };
    let rejected = results(&[Verdict::Reject]);
    if !rejected.is_empty() {
        block(&mut text, "rejected", &rejected);
    }
    let unchecked = results(&[Verdict::Error, Verdict::Decline]);
    if !unchecked.is_empty() {
        block(&mut text, "could not check", &unchecked);
    }
    text
}

fn list(text: &mut String, label: &str, values: &[String]) {
    let shown = values
        .iter()
        .take(TEXT_LIMIT)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if values.len() > TEXT_LIMIT {
        let _ = writeln!(
            text,
            "{label}: {shown}, and {} more",
            values.len() - TEXT_LIMIT
        );
    } else {
        let _ = writeln!(text, "{label}: {shown}");
    }
}

fn block(text: &mut String, label: &str, lines: &[String]) {
    let _ = writeln!(text, "{label}:");
    for line in lines.iter().take(TEXT_LIMIT) {
        let _ = writeln!(text, "  {line}");
    }
    if lines.len() > TEXT_LIMIT {
        let _ = writeln!(text, "  and {} more", lines.len() - TEXT_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CheckReport, Seconds, exit_status, kernels, render};
    use crate::check::{
        Attempt, Conclusion, KernelKind, Lean, ModuleResult, Report, SliceCounts, Timings, Verdict,
        View,
    };
    use crate::cli::CheckKernel;

    fn result(module: &str, verdict: Verdict, message: &str) -> ModuleResult {
        ModuleResult {
            module: module.to_owned(),
            kernel: KernelKind::Official,
            verdict,
            view: Some(View::Exported),
            attempts: vec![Attempt {
                view: View::Exported,
                key: "00".to_owned(),
                constants: 1,
                verdict,
                cached: false,
                seconds: 1.0,
                exit_code: None,
                message: message.to_owned(),
            }],
        }
    }

    #[test]
    fn an_unfinished_check_exits_two_and_names_what_it_could_not_check() {
        assert_eq!(exit_status(Conclusion::Accept), 0);
        assert_eq!(exit_status(Conclusion::Reject), 1);
        assert_eq!(exit_status(Conclusion::Unknown), 2);

        let report = CheckReport {
            project: PathBuf::from("/project"),
            toolchain: "toolchain".to_owned(),
            built: false,
            sandboxed: true,
            roots: vec!["Demo".to_owned()],
            exporter: PathBuf::from("/lev-export"),
            reference: PathBuf::from("/reference"),
            seconds: Seconds::default(),
            check: Report {
                format: "lev-check/1",
                verdict: Conclusion::Unknown,
                lean: Lean {
                    version: "0.0.0".to_owned(),
                    githash: "0".repeat(40),
                },
                kernels: Vec::new(),
                modules: 2,
                checked_modules: 2,
                slices: SliceCounts::default(),
                views: Vec::new(),
                fallbacks: Vec::new(),
                rejections: Vec::new(),
                unchecked: vec!["Demo".to_owned()],
                axioms: Vec::new(),
                violations: Vec::new(),
                timings: Timings::default(),
                results: vec![
                    result("Demo.A", Verdict::Accept, ""),
                    result("Demo", Verdict::Error, "timed out after 60 s"),
                ],
            },
        };
        let text = render(&report);
        assert!(
            text.starts_with("unknown: 1 of 2 modules could not be checked with "),
            "{text}"
        );
        assert!(
            text.contains(
                "could not check:\n  Demo (official, exported view): timed out after 60 s\n"
            ),
            "{text}"
        );
        assert!(!text.contains("rejected:"), "{text}");
    }

    #[test]
    fn kernels_default_to_the_official_one_and_drop_repeats() {
        assert_eq!(kernels(&[]), [KernelKind::Official]);
        assert_eq!(
            kernels(&[
                CheckKernel::Nanoda,
                CheckKernel::Official,
                CheckKernel::Nanoda
            ]),
            [KernelKind::Nanoda, KernelKind::Official]
        );
    }
}
