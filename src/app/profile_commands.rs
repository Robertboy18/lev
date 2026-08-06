//! Repeatable command and project-file profiling for Lean environments.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cli::{LegacyProfileArgs, ProfileArgs};
use crate::core::atomic_file::replace as atomic_write;
use crate::core::json_output::{self, schema};
use crate::core::process::exit_code;
use crate::dependency::environment as dependency_env;

use super::AppContext;
use super::execution_commands::{display_os_command, prepared_project, project_command};

const BASELINE_VERSION: u32 = 1;
const MAX_PROJECT_ENTRIES: usize = 500_000;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProfileMode {
    Command,
    Files,
}

#[derive(Debug, Clone, Copy)]
enum ProfileContract {
    Legacy,
    Inspect,
}

#[derive(Debug, Serialize)]
struct ProfileReport {
    project: String,
    toolchain: String,
    mode: ProfileMode,
    command: Option<String>,
    warmup: u32,
    requested_repeats: u32,
    completed_repeats: u32,
    samples_ms: Vec<f64>,
    summary: Option<ProfileSummary>,
    files: Vec<FileProfile>,
    baseline: Option<BaselineComparison>,
    exit_code: i32,
}

#[derive(Debug, Clone, Serialize)]
struct ProfileSummary {
    min_ms: f64,
    median_ms: f64,
    mean_ms: f64,
    max_ms: f64,
}

#[derive(Debug, Serialize)]
struct FileProfile {
    path: String,
    completed_repeats: u32,
    samples_ms: Vec<f64>,
    summary: Option<ProfileSummary>,
    baseline_median_ms: Option<f64>,
    regression_percent: Option<f64>,
    exit_code: i32,
}

#[derive(Debug, Serialize)]
struct BaselineComparison {
    path: String,
    toolchain: String,
    compared_files: usize,
    new_files: Vec<String>,
    missing_files: Vec<String>,
    max_regression_percent: Option<f64>,
    regressions: Vec<FileRegression>,
}

#[derive(Debug, Serialize)]
struct FileRegression {
    path: String,
    baseline_median_ms: f64,
    current_median_ms: f64,
    percent: f64,
    exceeds_limit: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PerformanceBaseline {
    version: u32,
    toolchain: String,
    files: BTreeMap<String, f64>,
}

pub(super) fn profile(context: &AppContext, args: ProfileArgs) -> Result<i32> {
    profile_with_contract(context, args, ProfileContract::Inspect)
}

pub(super) fn profile_legacy(context: &AppContext, args: LegacyProfileArgs) -> Result<i32> {
    profile_with_contract(context, args.into(), ProfileContract::Legacy)
}

fn profile_with_contract(
    context: &AppContext,
    args: ProfileArgs,
    contract: ProfileContract,
) -> Result<i32> {
    if let Some(output) = args.output.as_deref() {
        for baseline in [args.baseline.as_deref(), args.save_baseline.as_deref()]
            .into_iter()
            .flatten()
        {
            if same_destination(output, baseline)? {
                bail!("the performance report and baseline must use different paths");
            }
        }
    }

    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;

    let _dependency_environment = dependency_env::lock_attached(&context.cache, &project)?;
    let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
    registry::record(&context.cache, &project)?;

    if args.files {
        profile_files(context, &source, &project, &args, contract)
    } else {
        profile_command(context, &source, &project, &args, contract)
    }
}

fn profile_command(
    context: &AppContext,
    source: &crate::project::Project,
    project: &crate::project::Project,
    args: &ProfileArgs,
    contract: ProfileContract,
) -> Result<i32> {
    let command = if args.command.is_empty() {
        vec![OsString::from("lake"), OsString::from("build")]
    } else {
        args.command.clone()
    };

    for index in 0..args.warmup {
        let label = match contract {
            ProfileContract::Legacy => "profile",
            ProfileContract::Inspect => "performance",
        };
        context.info(format!("{label} warmup {}/{}", index + 1, args.warmup));
        let code = run_once(
            context,
            project,
            &command,
            args.json,
            !args.execution.offline,
        )?;
        if code != 0 {
            return emit_profile(
                args,
                ProfileReport {
                    project: source.root.display().to_string(),
                    toolchain: project.toolchain.clone(),
                    mode: ProfileMode::Command,
                    command: Some(display_os_command(&command)),
                    warmup: args.warmup,
                    requested_repeats: args.repeat,
                    completed_repeats: 0,
                    samples_ms: Vec::new(),
                    summary: None,
                    files: Vec::new(),
                    baseline: None,
                    exit_code: code,
                },
                contract,
            );
        }
    }

    let mut samples = Vec::with_capacity(args.repeat as usize);
    let mut code = 0;
    for index in 0..args.repeat {
        let label = match contract {
            ProfileContract::Legacy => "profile",
            ProfileContract::Inspect => "performance",
        };
        context.info(format!("{label} sample {}/{}", index + 1, args.repeat));
        let started = Instant::now();
        code = run_once(
            context,
            project,
            &command,
            args.json,
            !args.execution.offline,
        )?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        if code != 0 {
            break;
        }
    }
    emit_profile(
        args,
        ProfileReport {
            project: source.root.display().to_string(),
            toolchain: project.toolchain.clone(),
            mode: ProfileMode::Command,
            command: Some(display_os_command(&command)),
            warmup: args.warmup,
            requested_repeats: args.repeat,
            completed_repeats: samples.len() as u32,
            summary: summarize(&samples),
            samples_ms: samples,
            files: Vec::new(),
            baseline: None,
            exit_code: code,
        },
        contract,
    )
}

fn profile_files(
    context: &AppContext,
    source: &crate::project::Project,
    project: &crate::project::Project,
    args: &ProfileArgs,
    contract: ProfileContract,
) -> Result<i32> {
    let baseline = args
        .baseline
        .as_deref()
        .map(|path| read_baseline(path, &project.toolchain))
        .transpose()?;
    let files = project_lean_files(&source.root)?;
    if files.is_empty() {
        bail!(
            "no project Lean files were found under {}",
            source.root.display()
        );
    }

    context.info("prebuilding project dependencies before file profiling");
    let prebuild = vec![OsString::from("lake"), OsString::from("build")];
    let prebuild_code = run_once(
        context,
        project,
        &prebuild,
        args.json,
        !args.execution.offline,
    )?;
    if prebuild_code != 0 {
        return emit_profile(
            args,
            ProfileReport {
                project: source.root.display().to_string(),
                toolchain: project.toolchain.clone(),
                mode: ProfileMode::Files,
                command: None,
                warmup: args.warmup,
                requested_repeats: args.repeat,
                completed_repeats: 0,
                samples_ms: Vec::new(),
                summary: None,
                files: Vec::new(),
                baseline: None,
                exit_code: prebuild_code,
            },
            contract,
        );
    }

    let mut reports = Vec::with_capacity(files.len());
    let mut execution_code = 0;
    for (file_index, relative) in files.iter().enumerate() {
        context.info(format!(
            "performance file {}/{}: {}",
            file_index + 1,
            files.len(),
            display_path(relative)
        ));
        let execution_file = project.root.join(relative);
        let command = vec![
            OsString::from("lake"),
            OsString::from("lean"),
            execution_file.into_os_string(),
        ];
        let mut samples = Vec::with_capacity(args.repeat as usize);
        let mut code = 0;

        for _ in 0..args.warmup {
            code = run_once(
                context,
                project,
                &command,
                args.json,
                !args.execution.offline,
            )?;
            if code != 0 {
                break;
            }
        }
        if code == 0 {
            for _ in 0..args.repeat {
                let started = Instant::now();
                code = run_once(
                    context,
                    project,
                    &command,
                    args.json,
                    !args.execution.offline,
                )?;
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                if code != 0 {
                    break;
                }
            }
        }

        let path = display_path(relative);
        let summary = summarize(&samples);
        let baseline_median_ms = baseline
            .as_ref()
            .and_then(|baseline| baseline.files.get(&path))
            .copied();
        let regression_percent = summary
            .as_ref()
            .zip(baseline_median_ms)
            .and_then(|(summary, previous)| regression(previous, summary.median_ms));
        reports.push(FileProfile {
            path,
            completed_repeats: samples.len() as u32,
            samples_ms: samples,
            summary,
            baseline_median_ms,
            regression_percent,
            exit_code: code,
        });
        if code != 0 {
            execution_code = code;
            break;
        }
    }

    reports.sort_by(|left, right| {
        let left_median = left.summary.as_ref().map(|summary| summary.median_ms);
        let right_median = right.summary.as_ref().map(|summary| summary.median_ms);
        match (left_median, right_median) {
            (Some(left_median), Some(right_median)) => right_median
                .total_cmp(&left_median)
                .then_with(|| left.path.cmp(&right.path)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => left.path.cmp(&right.path),
        }
    });

    let comparison = if execution_code == 0 {
        baseline
            .as_ref()
            .zip(args.baseline.as_deref())
            .map(|(baseline, baseline_path)| {
                compare_baseline(baseline_path, baseline, &reports, args.max_regression)
            })
    } else {
        None
    };
    let threshold_failed = comparison.as_ref().is_some_and(|comparison| {
        comparison
            .regressions
            .iter()
            .any(|regression| regression.exceeds_limit)
    });
    let final_code = if execution_code != 0 {
        execution_code
    } else if threshold_failed {
        1
    } else {
        0
    };

    if final_code == 0 {
        if let Some(path) = &args.save_baseline {
            write_baseline(path, &project.toolchain, &reports)?;
        }
    }

    let completed_repeats = reports
        .iter()
        .map(|file| file.completed_repeats)
        .sum::<u32>();
    emit_profile(
        args,
        ProfileReport {
            project: source.root.display().to_string(),
            toolchain: project.toolchain.clone(),
            mode: ProfileMode::Files,
            command: None,
            warmup: args.warmup,
            requested_repeats: args.repeat,
            completed_repeats,
            samples_ms: Vec::new(),
            summary: None,
            files: reports,
            baseline: comparison,
            exit_code: final_code,
        },
        contract,
    )
}

fn run_once(
    context: &AppContext,
    project: &crate::project::Project,
    args: &[OsString],
    capture_stdout: bool,
    install_toolchain: bool,
) -> Result<i32> {
    let mut command = project_command(context, project, args, install_toolchain)?;
    if capture_stdout {
        command.stdin(Stdio::inherit());
        let output = command
            .output()
            .with_context(|| format!("failed to start {}", display_os_command(args)))?;
        let mut diagnostics = io::stderr().lock();
        diagnostics
            .write_all(&output.stdout)
            .context("failed to relay profiled command stdout")?;
        diagnostics
            .write_all(&output.stderr)
            .context("failed to relay profiled command stderr")?;
        Ok(exit_code(output.status))
    } else {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command
            .status()
            .with_context(|| format!("failed to start {}", display_os_command(args)))?;
        Ok(exit_code(status))
    }
}

fn project_lean_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut entries = 0_usize;
    visit_project_files(root, root, &mut entries, &mut files)?;
    files.sort();
    Ok(files)
}

fn visit_project_files(
    root: &Path,
    directory: &Path,
    entries: &mut usize,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut children = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", directory.display()))?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        *entries += 1;
        if *entries > MAX_PROJECT_ENTRIES {
            bail!("stopped after {MAX_PROJECT_ENTRIES} project entries while finding Lean files");
        }
        let file_type = child
            .file_type()
            .with_context(|| format!("failed to inspect {}", child.path().display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if ignored_directory(&child.file_name()) {
                continue;
            }
            visit_project_files(root, &child.path(), entries, files)?;
        } else if file_type.is_file()
            && child.path().extension().and_then(|value| value.to_str()) == Some("lean")
            && child.file_name() != "lakefile.lean"
        {
            files.push(
                child
                    .path()
                    .strip_prefix(root)
                    .context("project file escaped its root")?
                    .to_owned(),
            );
        }
    }
    Ok(())
}

fn ignored_directory(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".lake"
                | ".direnv"
                | "build"
                | "lake-packages"
                | "lean_packages"
                | "node_modules"
                | "target"
                | "_target"
        )
    )
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn summarize(samples: &[f64]) -> Option<ProfileSummary> {
    if samples.is_empty() {
        return None;
    }
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    let middle = ordered.len() / 2;
    let median = if ordered.len() % 2 == 0 {
        (ordered[middle - 1] + ordered[middle]) / 2.0
    } else {
        ordered[middle]
    };
    Some(ProfileSummary {
        min_ms: ordered[0],
        median_ms: median,
        mean_ms: ordered.iter().sum::<f64>() / ordered.len() as f64,
        max_ms: ordered[ordered.len() - 1],
    })
}

fn regression(previous: f64, current: f64) -> Option<f64> {
    (previous > 0.0).then_some((current - previous) * 100.0 / previous)
}

fn read_baseline(path: &Path, toolchain: &str) -> Result<PerformanceBaseline> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read performance baseline {}", path.display()))?;
    let baseline: PerformanceBaseline = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse performance baseline {}", path.display()))?;
    if baseline.version != BASELINE_VERSION {
        bail!(
            "{} uses unsupported performance baseline version {}",
            path.display(),
            baseline.version
        );
    }
    if baseline.toolchain != toolchain {
        bail!(
            "performance baseline {} was recorded with {}, but this run uses {}",
            path.display(),
            baseline.toolchain,
            toolchain
        );
    }
    for (file, median) in &baseline.files {
        if !median.is_finite() || *median <= 0.0 {
            bail!("{} contains an invalid median for {file:?}", path.display());
        }
    }
    Ok(baseline)
}

fn write_baseline(path: &Path, toolchain: &str, reports: &[FileProfile]) -> Result<()> {
    let files = reports
        .iter()
        .filter_map(|file| {
            file.summary
                .as_ref()
                .map(|summary| (file.path.clone(), summary.median_ms))
        })
        .collect();
    let baseline = PerformanceBaseline {
        version: BASELINE_VERSION,
        toolchain: toolchain.to_owned(),
        files,
    };
    ensure_parent(path)?;
    let mut bytes =
        serde_json::to_vec_pretty(&baseline).context("failed to serialize performance baseline")?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

fn compare_baseline(
    path: &Path,
    baseline: &PerformanceBaseline,
    reports: &[FileProfile],
    max_regression_percent: Option<f64>,
) -> BaselineComparison {
    let current = reports
        .iter()
        .filter_map(|file| {
            file.summary
                .as_ref()
                .map(|summary| (file.path.as_str(), summary.median_ms))
        })
        .collect::<BTreeMap<_, _>>();
    let new_files = current
        .keys()
        .filter(|file| !baseline.files.contains_key(**file))
        .map(|file| (*file).to_owned())
        .collect::<Vec<_>>();
    let missing_files = baseline
        .files
        .keys()
        .filter(|file| !current.contains_key(file.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let mut regressions = baseline
        .files
        .iter()
        .filter_map(|(file, previous)| {
            let current = current.get(file.as_str()).copied()?;
            let percent = regression(*previous, current)?;
            (percent > 0.0).then_some(FileRegression {
                path: file.clone(),
                baseline_median_ms: *previous,
                current_median_ms: current,
                percent,
                exceeds_limit: max_regression_percent.is_some_and(|maximum| percent > maximum),
            })
        })
        .collect::<Vec<_>>();
    regressions.sort_by(|left, right| {
        right
            .percent
            .total_cmp(&left.percent)
            .then_with(|| left.path.cmp(&right.path))
    });
    let compared_files = current
        .keys()
        .filter(|file| baseline.files.contains_key(**file))
        .count();

    BaselineComparison {
        path: path.display().to_string(),
        toolchain: baseline.toolchain.clone(),
        compared_files,
        new_files,
        missing_files,
        max_regression_percent,
        regressions,
    }
}

fn emit_profile(
    args: &ProfileArgs,
    report: ProfileReport,
    contract: ProfileContract,
) -> Result<i32> {
    let code = report.exit_code;
    let bytes = if args.json {
        match contract {
            ProfileContract::Legacy => {
                let legacy = LegacyProfileReport::from(&report);
                json_output::render(schema::PROFILE, &legacy)?
            }
            ProfileContract::Inspect => json_output::render(schema::INSPECT_PERFORMANCE, &report)?,
        }
    } else {
        render_profile(&report, contract).into_bytes()
    };
    if let Some(path) = &args.output {
        ensure_parent(path)?;
        atomic_write(path, &bytes)?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write performance report")?;
    }
    Ok(code)
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

fn same_destination(left: &Path, right: &Path) -> Result<bool> {
    Ok(destination_path(left)? == destination_path(right)?)
}

fn destination_path(path: &Path) -> Result<PathBuf> {
    let absolute = crate::project::absolute(path)?;
    let mut unresolved = Vec::new();
    let mut ancestor = absolute.as_path();

    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            break;
        };
        unresolved.push(name.to_owned());
        ancestor = ancestor
            .parent()
            .context("output path has no existing ancestor")?;
    }

    let mut resolved = fs::canonicalize(ancestor)
        .with_context(|| format!("failed to resolve {}", ancestor.display()))?;
    for name in unresolved.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

#[derive(Debug, Serialize)]
struct LegacyProfileReport<'a> {
    project: &'a str,
    toolchain: &'a str,
    command: &'a str,
    warmup: u32,
    requested_repeats: u32,
    completed_repeats: u32,
    samples_ms: &'a [f64],
    summary: &'a Option<ProfileSummary>,
    exit_code: i32,
}

impl<'a> From<&'a ProfileReport> for LegacyProfileReport<'a> {
    fn from(report: &'a ProfileReport) -> Self {
        Self {
            project: &report.project,
            toolchain: &report.toolchain,
            command: report
                .command
                .as_deref()
                .expect("legacy profiling only supports command mode"),
            warmup: report.warmup,
            requested_repeats: report.requested_repeats,
            completed_repeats: report.completed_repeats,
            samples_ms: &report.samples_ms,
            summary: &report.summary,
            exit_code: report.exit_code,
        }
    }
}

fn render_profile(report: &ProfileReport, contract: ProfileContract) -> String {
    match report.mode {
        ProfileMode::Command => render_command_profile(report, contract),
        ProfileMode::Files => render_file_profile(report),
    }
}

fn render_command_profile(report: &ProfileReport, contract: ProfileContract) -> String {
    let label = match contract {
        ProfileContract::Legacy => "profile",
        ProfileContract::Inspect => "performance",
    };
    let mut text = format!(
        "{label}: {} sample{}, command `{}`\n",
        report.completed_repeats,
        if report.completed_repeats == 1 {
            ""
        } else {
            "s"
        },
        report.command.as_deref().unwrap_or("<unknown>")
    );
    for (index, sample) in report.samples_ms.iter().enumerate() {
        text.push_str(&format!("  {:>4}: {:>10.3} ms\n", index + 1, sample));
    }
    if let Some(summary) = &report.summary {
        text.push_str(&format!(
            "  min {:>10.3} ms  median {:>10.3} ms  mean {:>10.3} ms  max {:>10.3} ms\n",
            summary.min_ms, summary.median_ms, summary.mean_ms, summary.max_ms
        ));
    } else {
        text.push_str("  no measured samples\n");
    }
    text
}

fn render_file_profile(report: &ProfileReport) -> String {
    let mut text = format!(
        "performance: {} Lean file{}, {} measured execution{}\n",
        report.files.len(),
        if report.files.len() == 1 { "" } else { "s" },
        report.completed_repeats,
        if report.completed_repeats == 1 {
            ""
        } else {
            "s"
        }
    );
    for file in &report.files {
        if let Some(summary) = &file.summary {
            text.push_str(&format!("{:>12.3} ms  {}\n", summary.median_ms, file.path));
        } else {
            text.push_str(&format!("{:>15}  {}\n", "failed", file.path));
        }
    }
    if let Some(comparison) = &report.baseline {
        text.push_str(&format!(
            "baseline: {} compared, {} new, {} missing ({})\n",
            comparison.compared_files,
            comparison.new_files.len(),
            comparison.missing_files.len(),
            comparison.path
        ));
        for regression in &comparison.regressions {
            let label = if regression.exceeds_limit {
                "REGRESSION"
            } else {
                "slower"
            };
            text.push_str(&format!(
                "{label:<10} +{:>8.2}%  {:>10.3} -> {:>10.3} ms  {}\n",
                regression.percent,
                regression.baseline_median_ms,
                regression.current_median_ms,
                regression.path
            ));
        }
    }
    text
}
