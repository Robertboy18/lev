//! `lev inspect build`: run Lake once and explain from its traces why each
//! module rebuilt.
//!
//! lev reads the module traces of every package, runs `lake --verbose build`
//! through elan while relaying its output, reads the traces again, and reports
//! the modules whose traces changed together with the inputs that changed.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use super::AppContext;
use crate::cache::registry;
use crate::cli::InspectBuildArgs;
use crate::core::json_output::{self, schema};
use crate::core::lock;
use crate::core::process::{describe, exit_code};
use crate::project::build_diagnostics::{
    self, BuildOutput, JobAction, JobLine, OutputReader, RootCause, RootCauseKind,
};
use crate::project::build_trace::{self, Cause, ChangeStatus, InputKind, ModuleChange, Snapshot};
use crate::project::{Package, Project};

/// How many entries a text list shows before it says how many it left out.
const TEXT_LIMIT: usize = 20;

#[derive(Debug, Default, Serialize)]
struct Actions {
    built: usize,
    fetched: usize,
    unpacked: usize,
    replayed: usize,
    reused: usize,
    ran: usize,
    canceled: usize,
}

#[derive(Debug, Serialize)]
struct ModuleReport {
    module: String,
    package: String,
    status: ChangeStatus,
    action: Option<JobAction>,
    elapsed_ms: Option<f64>,
    causes: Vec<Cause>,
    /// A trace recorded no inputs, so the causes come from changed imports.
    inferred: bool,
}

#[derive(Debug, Serialize)]
struct Report {
    project: String,
    toolchain: String,
    command: String,
    status: &'static str,
    exit_code: i32,
    elapsed_ms: f64,
    actions: Actions,
    events: Vec<JobLine>,
    modules: Vec<ModuleReport>,
    /// Modules whose own source file changed.
    edited: Vec<String>,
    /// Modules whose trace changed without a recorded or inferred cause.
    unexplained: Vec<String>,
    /// Modules whose trace the build removed.
    removed: Vec<String>,
    /// Whether lev read the traces both before and after the build. When it
    /// could not, `modules` is empty and `warnings` says why.
    traces_read: bool,
    root_causes: Vec<RootCause>,
    failed_targets: Vec<String>,
    /// With `--why`, the chain from the module back to the edit behind it.
    #[serde(skip_serializing_if = "Option::is_none")]
    why: Option<Vec<String>>,
    warnings: Vec<String>,
}

pub(super) fn run(context: &AppContext, args: InspectBuildArgs) -> Result<i32> {
    let project = context.project()?;
    let mut warnings = Vec::new();

    // elan may not have the toolchain yet. `elan run --install` below installs
    // it, and the cache is located again after the build.
    let lake_cache = context
        .lake_cache(&project)
        .ok()
        .map(|location| location.path);
    let _lock = match &lake_cache {
        Some(path) => lock::shared(&context.cache.lake_lock_path(path))
            .map_err(|error| warnings.push(format!("ran without the Lake cache lock: {error:#}")))
            .ok(),
        None => None,
    };
    if let Err(error) = registry::record(&context.cache, &project, lake_cache.as_deref()) {
        warnings.push(format!("could not record the project: {error:#}"));
    }

    let packages = workspace_packages(&project, &mut warnings);
    let before = capture(&packages, "before", &mut warnings);

    let mut lake_args = vec![OsString::from("--verbose"), OsString::from("build")];
    lake_args.extend(args.args);
    let mut command = context.elan_run(&project.toolchain, "lake", true);
    command.args(&lake_args).current_dir(&project.root);
    context.detail(format!("running {}", describe(&command)));
    let started = Instant::now();
    let (status, output) = run_relayed(command, !context.quiet)?;
    let build = Finished {
        command: std::iter::once("lake".to_owned())
            .chain(
                lake_args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned()),
            )
            .collect::<Vec<_>>()
            .join(" "),
        status,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        output,
    };

    if lake_cache.is_none() {
        if let Ok(location) = context.lake_cache(&project) {
            if let Err(error) = registry::record(&context.cache, &project, Some(&location.path)) {
                warnings.push(format!("could not record the project: {error:#}"));
            }
        }
    }

    // The build may have written the manifest, so list the packages again.
    let packages = workspace_packages(&project, &mut Vec::new());
    let after = capture(&packages, "after", &mut warnings);
    let comparison = match (&before, &after) {
        (Some(before), Some(after)) => {
            warnings.extend(after.warnings.iter().cloned());
            Some(build_trace::compare(before, after))
        }
        _ => None,
    };

    let report = report(&project, build, comparison, args.why.as_deref(), warnings);
    if args.json {
        json_output::print(schema::INSPECT_BUILD, &report)?;
    } else {
        io::stdout()
            .lock()
            .write_all(render(&report).as_bytes())
            .context("failed to write the build report")?;
    }
    Ok(report.exit_code)
}

/// One finished `lake build`.
struct Finished {
    command: String,
    status: ExitStatus,
    elapsed_ms: f64,
    output: BuildOutput,
}

fn workspace_packages(project: &Project, warnings: &mut Vec<String>) -> Vec<Package> {
    project.packages().unwrap_or_else(|error| {
        warnings.push(format!("read only the root package's traces: {error:#}"));
        vec![Package {
            name: "root".to_owned(),
            root: project.root.clone(),
        }]
    })
}

fn capture(packages: &[Package], when: &str, warnings: &mut Vec<String>) -> Option<Snapshot> {
    Snapshot::capture(packages)
        .map_err(|error| {
            warnings.push(format!(
                "could not read Lake traces {when} the build, so no rebuild is explained: {error:#}"
            ))
        })
        .ok()
}

/// Run `command`, copying its output to stderr line by line while parsing it.
fn run_relayed(mut command: Command, relay: bool) -> Result<(ExitStatus, BuildOutput)> {
    let description = describe(&command);
    let mut child = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {description}"))?;
    let stdout = child.stdout.take().context("missing build stdout")?;
    let stderr = child.stderr.take().context("missing build stderr")?;
    let stdout = thread::spawn(move || read_stream(stdout, relay));
    let stderr = thread::spawn(move || read_stream(stderr, relay));
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {description}"))?;
    let mut readers = Vec::with_capacity(2);
    for reader in [stdout, stderr] {
        let reader = reader
            .join()
            .map_err(|_| anyhow!("the reader of {description} panicked"))?
            .with_context(|| format!("failed to read the output of {description}"))?;
        readers.push(reader);
    }
    Ok((status, build_diagnostics::combine(readers)))
}

fn read_stream(stream: impl Read, relay: bool) -> io::Result<OutputReader> {
    let mut stream = BufReader::new(stream);
    let mut reader = OutputReader::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        if stream.read_until(b'\n', &mut line)? == 0 {
            return Ok(reader);
        }
        if relay {
            // A closed stderr must not stop lev from draining the pipe.
            let _ = io::stderr().lock().write_all(&line);
        }
        reader.line(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']));
    }
}

fn report(
    project: &Project,
    build: Finished,
    comparison: Option<build_trace::Comparison>,
    why: Option<&str>,
    mut warnings: Vec<String>,
) -> Report {
    let traces_read = comparison.is_some();
    let comparison = comparison.unwrap_or_default();
    let Finished {
        command,
        status,
        elapsed_ms,
        output,
    } = build;
    let mut actions = Actions::default();
    let mut last_job = BTreeMap::new();
    for job in &output.jobs {
        let count = match job.action {
            JobAction::Built => &mut actions.built,
            JobAction::Fetched => &mut actions.fetched,
            JobAction::Unpacked => &mut actions.unpacked,
            JobAction::Replayed => &mut actions.replayed,
            JobAction::Reused => &mut actions.reused,
            JobAction::Ran => &mut actions.ran,
            JobAction::Canceled => &mut actions.canceled,
        };
        *count += 1;
        last_job.insert(job.target.as_str(), job);
    }

    let edited = names(&comparison.changes, |change| {
        change
            .causes
            .iter()
            .any(|cause| cause.kind == InputKind::Source)
    });
    let unexplained = names(&comparison.changes, |change| {
        change.status == ChangeStatus::Changed && change.causes.is_empty()
    });
    let why = why.map(|module| {
        let chain = why_chain(&comparison.changes, module);
        if chain.is_empty() {
            warnings.push(format!("{module} did not rebuild in this build"));
        }
        chain
    });
    let modules = comparison
        .changes
        .into_iter()
        .map(|change| {
            let job = last_job.get(change.module.as_str());
            ModuleReport {
                action: job.map(|job| job.action),
                elapsed_ms: job.and_then(|job| job.elapsed_ms),
                module: change.module,
                package: change.package,
                status: change.status,
                causes: change.causes,
                inferred: change.inferred,
            }
        })
        .collect();

    let mut seen = BTreeSet::new();
    warnings.retain(|warning| seen.insert(warning.clone()));
    Report {
        project: project.root.display().to_string(),
        toolchain: project.toolchain.clone(),
        command,
        status: if status.success() { "passed" } else { "failed" },
        exit_code: exit_code(status),
        elapsed_ms,
        actions,
        events: output.jobs,
        modules,
        edited,
        unexplained,
        removed: comparison.removed,
        traces_read,
        root_causes: output.root_causes,
        failed_targets: output.failed_targets,
        why,
        warnings,
    }
}

fn names(changes: &[ModuleChange], keep: impl Fn(&ModuleChange) -> bool) -> Vec<String> {
    changes
        .iter()
        .filter(|change| keep(change))
        .map(|change| change.module.clone())
        .collect()
}

/// Follow import causes from `module` through other rebuilt modules until a
/// module rebuilt for its own reasons. Empty when `module` did not rebuild.
fn why_chain(changes: &[ModuleChange], module: &str) -> Vec<String> {
    let by_module = changes
        .iter()
        .map(|change| (change.module.as_str(), change))
        .collect::<BTreeMap<_, _>>();
    let mut chain = Vec::new();
    let mut current = module;
    while let Some(change) = by_module.get(current) {
        if chain.iter().any(|seen| seen == current) {
            break;
        }
        chain.push(current.to_owned());
        if change
            .causes
            .iter()
            .any(|cause| cause.kind != InputKind::Import)
        {
            break;
        }
        let Some(next) = change
            .causes
            .iter()
            .find(|cause| by_module.contains_key(cause.input.as_str()))
        else {
            break;
        };
        current = &next.input;
    }
    chain
}

fn render(report: &Report) -> String {
    let actions = &report.actions;
    let mut text = format!(
        "build {} in {:.3} s: {} built, {} fetched, {} unpacked, {} replayed, {} reused, {} ran",
        report.status,
        report.elapsed_ms / 1000.0,
        actions.built,
        actions.fetched,
        actions.unpacked,
        actions.replayed,
        actions.reused,
        actions.ran
    );
    if actions.canceled > 0 {
        let _ = write!(text, ", {} canceled", actions.canceled);
    }
    text.push('\n');

    let new = report
        .modules
        .iter()
        .filter(|module| module.status == ChangeStatus::New)
        .count();
    let changed = report.modules.len() - new;
    if !report.traces_read {
        text.push_str("traces: not compared\n");
    } else if report.modules.is_empty() && report.removed.is_empty() {
        text.push_str("traces: no module trace changed\n");
    } else {
        let _ = writeln!(
            text,
            "traces: {changed} changed, {new} new, {} removed",
            report.removed.len()
        );
    }
    let mut kinds = BTreeMap::<InputKind, usize>::new();
    for module in &report.modules {
        let module_kinds = module
            .causes
            .iter()
            .map(|cause| cause.kind)
            .collect::<BTreeSet<_>>();
        for kind in module_kinds {
            *kinds.entry(kind).or_default() += 1;
        }
    }
    if !kinds.is_empty() {
        let counts = kinds
            .iter()
            .map(|(kind, count)| format!("{count} {}", kind_name(*kind)))
            .collect::<Vec<_>>();
        let _ = writeln!(text, "causes: {}", counts.join(", "));
    }
    list(&mut text, "edited", &report.edited);
    list(&mut text, "unexplained", &report.unexplained);
    list(&mut text, "removed", &report.removed);

    let mut built = report
        .events
        .iter()
        .filter(|job| job.action == JobAction::Built)
        .filter_map(|job| job.elapsed_ms.map(|ms| (ms, job.target.as_str())))
        .collect::<Vec<_>>();
    built.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(right.1)));
    if !built.is_empty() {
        text.push_str("slowest:\n");
        for (ms, target) in built.iter().take(TEXT_LIMIT) {
            let _ = writeln!(text, "  {ms:>10.0} ms  {target}");
        }
    }

    if let Some(chain) = &report.why {
        let by_module = report
            .modules
            .iter()
            .map(|module| (module.module.as_str(), module))
            .collect::<BTreeMap<_, _>>();
        text.push_str("why:\n");
        for module in chain {
            let causes = by_module
                .get(module.as_str())
                .copied()
                .map(describe_causes)
                .unwrap_or_default();
            let _ = writeln!(text, "  {module}: {causes}");
        }
    }

    if !report.root_causes.is_empty() {
        text.push_str("root causes:\n");
        for cause in &report.root_causes {
            match cause.kind {
                RootCauseKind::LeanDiagnostic => {
                    let _ = writeln!(
                        text,
                        "  {}:{}:{}: {}",
                        cause.file.as_deref().unwrap_or("<unknown>"),
                        cause.line.unwrap_or(0),
                        cause.column.unwrap_or(0),
                        cause.message
                    );
                }
                RootCauseKind::LakeError => {
                    let _ = writeln!(text, "  Lake: {}", cause.message);
                }
                RootCauseKind::FailedJob => {
                    let _ = writeln!(text, "  failed job: {}", cause.message);
                }
            }
        }
    }
    list(&mut text, "failed targets", &report.failed_targets);
    for warning in &report.warnings {
        let _ = writeln!(text, "warning: {warning}");
    }
    text
}

fn describe_causes(module: &ModuleReport) -> String {
    if module.causes.is_empty() {
        return match module.status {
            ChangeStatus::New => "new module".to_owned(),
            ChangeStatus::Changed => "no recorded cause".to_owned(),
        };
    }
    module
        .causes
        .iter()
        .map(|cause| format!("{} {}", kind_name(cause.kind), cause.input))
        .collect::<Vec<_>>()
        .join("; ")
}

fn list(text: &mut String, label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
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

fn kind_name(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Source => "source",
        InputKind::Import => "import",
        InputKind::Dependency => "dependency",
        InputKind::Libraries => "libraries",
        InputKind::Toolchain => "toolchain",
        InputKind::Options => "options",
        InputKind::Configuration => "configuration",
        InputKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::why_chain;
    use crate::project::build_trace::{Cause, ChangeStatus, InputKind, ModuleChange};

    fn change(module: &str, causes: &[(InputKind, &str)]) -> ModuleChange {
        ModuleChange {
            module: module.to_owned(),
            package: "root".to_owned(),
            status: ChangeStatus::Changed,
            causes: causes
                .iter()
                .map(|(kind, input)| Cause {
                    kind: *kind,
                    input: (*input).to_owned(),
                })
                .collect(),
            inferred: false,
        }
    }

    #[test]
    fn follows_import_causes_back_to_the_edit() {
        let changes = [
            change("A", &[(InputKind::Source, "/p/A.lean")]),
            change("B", &[(InputKind::Import, "A")]),
            change(
                "C",
                &[(InputKind::Import, "Unchanged"), (InputKind::Import, "B")],
            ),
            change("X", &[(InputKind::Import, "Y")]),
            change("Y", &[(InputKind::Import, "X")]),
        ];
        assert_eq!(why_chain(&changes, "C"), ["C", "B", "A"]);
        assert_eq!(why_chain(&changes, "X"), ["X", "Y"]);
        assert!(why_chain(&changes, "Missing").is_empty());
    }
}
