//! Compiler-backed Lean import analysis, reporting, and verified rewrites.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Output;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cli::ShakeArgs;
use crate::core::atomic_file::replace as atomic_replace;
use crate::core::json_output::{self, schema};
use crate::core::process::exit_code;
use crate::dependency::environment as dependency_env;
use crate::project::Project;
use crate::project::manifest::LakeManifest;

use super::AppContext;
use super::execution_commands::{display_os_command, prepared_project, project_command};
use super::transaction::FileTransaction;

#[derive(Debug, Clone)]
struct ParsedSuggestion {
    path: PathBuf,
    removed_imports: Vec<String>,
    added_imports: Vec<String>,
}

#[derive(Debug, Clone)]
struct ValidatedEdit {
    path: PathBuf,
    report_path: String,
    removed_imports: Vec<String>,
    added_imports: Vec<String>,
}

#[derive(Debug)]
struct EditCandidate {
    edit: ValidatedEdit,
    original: Vec<u8>,
    proposed: Vec<u8>,
}

#[derive(Debug)]
struct VerificationFailure {
    code: i32,
    output: Output,
}

#[derive(Debug, Clone, Copy)]
enum VerificationVisibility {
    Visible,
    Quiet,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShakeMode {
    Preview,
    Check,
    Fix,
    Isolate,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShakeStatus {
    Clean,
    ChangesRequired,
    Applied,
    PartiallyApplied,
    Rejected,
    AnalysisFailed,
    ApplyFailed,
    VerificationFailed,
    BaselineFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FileDisposition {
    Proposed,
    Applied,
    Rejected,
    Restored,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ShakeReport {
    mode: ShakeMode,
    status: ShakeStatus,
    files: Vec<ShakeFileReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage: Option<ShakeCoverage>,
}

#[derive(Debug, Clone, Serialize)]
struct ShakeFileReport {
    path: String,
    removed_imports: Vec<String>,
    added_imports: Vec<String>,
    disposition: FileDisposition,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keep_annotations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ShakeCoverage {
    total_files: usize,
    analyzed: Vec<String>,
    outside_roots: Vec<String>,
    legacy: Vec<String>,
}

pub(super) struct ShakeOutcome {
    pub code: i32,
    pub report: ShakeReport,
}

/// Analyze Lean imports through Lake and optionally apply verified edits.
pub(super) fn shake(context: &AppContext, args: ShakeArgs) -> Result<i32> {
    let outcome = shake_outcome(context, &args)?;
    emit_report(&args, &outcome.report)?;
    Ok(outcome.code)
}

pub(super) fn shake_outcome(context: &AppContext, args: &ShakeArgs) -> Result<ShakeOutcome> {
    if args.fix && context.local {
        bail!("`lev shake --fix` cannot be combined with `--local`");
    }
    if args.fix && args.execution.lean.is_some() {
        bail!(
            "`lev shake --fix` cannot edit a versioned workspace; select the project's default \
             toolchain before applying changes"
        );
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

    if let Some(code) = ensure_shake_modules_built(context, &project, args)? {
        return Ok(finish(
            args,
            ShakeStatus::AnalysisFailed,
            Vec::new(),
            None,
            code,
        ));
    }

    let mut analysis = captured_in_project(
        context,
        &project,
        &shake_command(args, false, false),
        !args.execution.offline,
    )?;
    if !analysis.status.success()
        && parse_shake_suggestions(&analysis.stdout).is_empty()
        && shake_needs_build(&analysis)
        && !args.no_build
        && !args.force
    {
        for command in shake_verification_commands(args) {
            context.info(format!(
                "Lake modules are stale; running `{}` before import analysis",
                display_os_command(&command)
            ));
            let build =
                captured_uncached_in_project(context, &project, &command, !args.execution.offline)?;
            relay_output(&build, machine_output(args))?;
            if !build.status.success() {
                let code = exit_code(build.status);
                return Ok(finish(
                    args,
                    ShakeStatus::AnalysisFailed,
                    Vec::new(),
                    None,
                    code,
                ));
            }
        }
        analysis = captured_in_project(
            context,
            &project,
            &shake_command(args, false, false),
            !args.execution.offline,
        )?;
    }

    let suggestions = parse_shake_suggestions(&analysis.stdout);
    let coverage = if args.coverage && (analysis.status.success() || !suggestions.is_empty()) {
        Some(collect_coverage(context, &source, &project, args)?)
    } else {
        None
    };
    let mut reports = suggestion_reports(&source.root, &project.root, &suggestions);

    if !args.fix {
        relay_output(&analysis, machine_output(args))?;
        let status = if !suggestions.is_empty() {
            ShakeStatus::ChangesRequired
        } else if analysis.status.success() {
            ShakeStatus::Clean
        } else {
            ShakeStatus::AnalysisFailed
        };
        let code = if suggestions.is_empty() {
            exit_code(analysis.status)
        } else {
            1
        };
        return Ok(finish(args, status, reports, coverage, code));
    }

    if suggestions.is_empty() {
        relay_output(&analysis, machine_output(args))?;
        let status = if analysis.status.success() {
            context.info("imports are already minimal");
            ShakeStatus::Clean
        } else {
            ShakeStatus::AnalysisFailed
        };
        let code = exit_code(analysis.status);
        return Ok(finish(args, status, reports, coverage, code));
    }

    let edits = validate_shake_edits(&source, suggestions)?;
    let edit_paths = edits
        .iter()
        .map(|edit| edit.path.clone())
        .collect::<Vec<_>>();
    let originals = edits
        .iter()
        .map(|edit| {
            fs::read(&edit.path).with_context(|| format!("failed to read {}", edit.path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut transaction = FileTransaction::capture(edit_paths.iter())?;

    let fixed = captured_in_project(
        context,
        &project,
        &shake_command(args, true, false),
        !args.execution.offline,
    )?;
    relay_output(&fixed, machine_output(args))?;
    if !fixed.status.success() {
        transaction.restore_now()?;
        set_all_dispositions(&mut reports, FileDisposition::Restored);
        context.info("shake failed; restored the original Lean sources");
        let code = exit_code(fixed.status);
        return Ok(finish(
            args,
            ShakeStatus::ApplyFailed,
            reports,
            coverage,
            code,
        ));
    }

    let proposed = edits
        .iter()
        .map(|edit| {
            fs::read(&edit.path)
                .with_context(|| format!("failed to read rewritten source {}", edit.path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let candidates = edits
        .into_iter()
        .zip(originals)
        .zip(proposed)
        .map(|((edit, original), proposed)| EditCandidate {
            edit,
            original,
            proposed,
        })
        .collect::<Vec<_>>();

    let batch_failure = verify_shake_edits(
        context,
        &project,
        args,
        "verifying rewritten imports",
        VerificationVisibility::Visible,
    )?;
    let Some(batch_failure) = batch_failure else {
        transaction.commit();
        set_all_dispositions(&mut reports, FileDisposition::Applied);
        return Ok(finish(args, ShakeStatus::Applied, reports, coverage, 0));
    };

    if !args.isolate {
        context.info("import verification failed; restoring the original Lean sources");
        transaction.restore_now()?;
        set_all_dispositions(&mut reports, FileDisposition::Restored);
        return Ok(finish(
            args,
            ShakeStatus::VerificationFailed,
            reports,
            coverage,
            batch_failure.code,
        ));
    }

    context.info("batch verification failed; isolating import edits");
    transaction.restore_now()?;
    let baseline_failure = verify_shake_edits(
        context,
        &project,
        args,
        "verifying the restored baseline",
        VerificationVisibility::Visible,
    )?;
    if let Some(failure) = baseline_failure {
        transaction.restore_now()?;
        set_all_dispositions(&mut reports, FileDisposition::Restored);
        context.info("the restored baseline does not build; no isolated edits were kept");
        return Ok(finish(
            args,
            ShakeStatus::BaselineFailed,
            reports,
            coverage,
            failure.code,
        ));
    }

    let mut accepted = BTreeSet::new();
    let mut rejected = BTreeSet::new();
    let indices = (0..candidates.len()).collect::<Vec<_>>();
    if indices.len() == 1 {
        rejected.insert(0);
    } else {
        let middle = indices.len() / 2;
        isolate_subset(
            context,
            &project,
            args,
            &candidates,
            &indices[..middle],
            &mut accepted,
            &mut rejected,
        )?;
        isolate_subset(
            context,
            &project,
            args,
            &candidates,
            &indices[middle..],
            &mut accepted,
            &mut rejected,
        )?;
    }

    if let Some(failure) = verify_shake_edits(
        context,
        &project,
        args,
        "verifying the isolated result",
        VerificationVisibility::Visible,
    )? {
        transaction.restore_now()?;
        set_all_dispositions(&mut reports, FileDisposition::Restored);
        context.info("isolated import verification failed; restored every source");
        return Ok(finish(
            args,
            ShakeStatus::VerificationFailed,
            reports,
            coverage,
            failure.code,
        ));
    }

    for index in &accepted {
        set_disposition(
            &mut reports,
            &candidates[*index].edit.report_path,
            FileDisposition::Applied,
            &[],
        );
    }
    for index in &rejected {
        let candidate = &candidates[*index];
        let annotations = candidate
            .edit
            .removed_imports
            .iter()
            .map(|import| format!("{import} -- shake: keep"))
            .collect::<Vec<_>>();
        set_disposition(
            &mut reports,
            &candidate.edit.report_path,
            FileDisposition::Rejected,
            &annotations,
        );
        context.info(format!(
            "rejected the import rewrite for {}",
            candidate.edit.report_path
        ));
        for annotation in annotations {
            context.info(format!("keep the dependency with `{annotation}`"));
        }
    }

    transaction.commit();
    let status = if rejected.is_empty() {
        ShakeStatus::Applied
    } else if accepted.is_empty() {
        ShakeStatus::Rejected
    } else {
        ShakeStatus::PartiallyApplied
    };
    Ok(finish(
        args,
        status,
        reports,
        coverage,
        if rejected.is_empty() { 0 } else { 1 },
    ))
}

fn isolate_subset(
    context: &AppContext,
    project: &Project,
    args: &ShakeArgs,
    candidates: &[EditCandidate],
    indices: &[usize],
    accepted: &mut BTreeSet<usize>,
    rejected: &mut BTreeSet<usize>,
) -> Result<()> {
    if indices.is_empty() {
        return Ok(());
    }

    apply_candidate_contents(candidates, indices, true)?;
    context.info(format!(
        "testing {} isolated import edit{}",
        indices.len(),
        if indices.len() == 1 { "" } else { "s" }
    ));
    let failure = verify_shake_edits(
        context,
        project,
        args,
        "testing isolated imports",
        VerificationVisibility::Quiet,
    )?;
    if failure.is_none() {
        accepted.extend(indices.iter().copied());
        return Ok(());
    }

    apply_candidate_contents(candidates, indices, false)?;
    if indices.len() == 1 {
        let failure = failure.expect("checked above");
        relay_output(&failure.output, machine_output(args))?;
        rejected.insert(indices[0]);
        return Ok(());
    }

    let middle = indices.len() / 2;
    isolate_subset(
        context,
        project,
        args,
        candidates,
        &indices[..middle],
        accepted,
        rejected,
    )?;
    isolate_subset(
        context,
        project,
        args,
        candidates,
        &indices[middle..],
        accepted,
        rejected,
    )
}

fn apply_candidate_contents(
    candidates: &[EditCandidate],
    indices: &[usize],
    proposed: bool,
) -> Result<()> {
    for index in indices {
        let candidate = &candidates[*index];
        let contents = if proposed {
            &candidate.proposed
        } else {
            &candidate.original
        };
        atomic_replace(&candidate.edit.path, contents)?;
    }
    Ok(())
}

fn ensure_shake_modules_built(
    context: &AppContext,
    project: &Project,
    args: &ShakeArgs,
) -> Result<Option<i32>> {
    if args.modules.is_empty() || args.no_build || args.force {
        return Ok(None);
    }

    let check_command = shake_module_build_command(&args.modules, true);
    let check =
        captured_uncached_in_project(context, project, &check_command, !args.execution.offline)?;
    if check.status.success() {
        return Ok(None);
    }
    if !shake_needs_build(&check) {
        relay_output(&check, args.json)?;
        return Ok(Some(exit_code(check.status)));
    }

    let build_command = shake_module_build_command(&args.modules, false);
    context.info(format!(
        "Lake modules are stale; running `{}` before import analysis",
        display_os_command(&build_command)
    ));
    let build =
        captured_uncached_in_project(context, project, &build_command, !args.execution.offline)?;
    relay_output(&build, args.json)?;
    if build.status.success() {
        Ok(None)
    } else {
        Ok(Some(exit_code(build.status)))
    }
}

fn verify_shake_edits(
    context: &AppContext,
    project: &Project,
    args: &ShakeArgs,
    message: &str,
    visibility: VerificationVisibility,
) -> Result<Option<VerificationFailure>> {
    for command in shake_verification_commands(args) {
        if matches!(visibility, VerificationVisibility::Visible) {
            context.info(format!("{message} with `{}`", display_os_command(&command)));
        }
        let output =
            captured_uncached_in_project(context, project, &command, !args.execution.offline)?;
        if matches!(visibility, VerificationVisibility::Visible) {
            relay_output(&output, machine_output(args))?;
        }
        if !output.status.success() {
            return Ok(Some(VerificationFailure {
                code: exit_code(output.status),
                output,
            }));
        }
    }

    for command in shake_freshness_commands(args) {
        let output =
            captured_uncached_in_project(context, project, &command, !args.execution.offline)?;
        if matches!(visibility, VerificationVisibility::Visible) {
            relay_output(&output, machine_output(args))?;
        }
        if !output.status.success() {
            return Ok(Some(VerificationFailure {
                code: exit_code(output.status),
                output,
            }));
        }
    }
    Ok(None)
}

fn captured_in_project(
    context: &AppContext,
    project: &Project,
    args: &[OsString],
    install_toolchain: bool,
) -> Result<Output> {
    let mut command = project_command(context, project, args, install_toolchain)?;
    command
        .output()
        .with_context(|| format!("failed to start {}", display_os_command(args)))
}

fn captured_uncached_in_project(
    context: &AppContext,
    project: &Project,
    args: &[OsString],
    install_toolchain: bool,
) -> Result<Output> {
    let mut command = project_command(context, project, args, install_toolchain)?;
    command.env("LAKE_ARTIFACT_CACHE", "false");
    command
        .output()
        .with_context(|| format!("failed to start {}", display_os_command(args)))
}

fn shake_command(args: &ShakeArgs, fix: bool, force_explain: bool) -> Vec<OsString> {
    let mut command = vec![OsString::from("lake"), OsString::from("shake")];
    for (enabled, option) in [
        (args.force, "--force"),
        (args.keep_implied, "--keep-implied"),
        (args.keep_prefix, "--keep-prefix"),
        (args.keep_public, "--keep-public"),
        (args.add_public, "--add-public"),
        (args.explain || force_explain, "--explain"),
        (args.gh_style, "--gh-style"),
        (args.trace, "--trace"),
        (fix, "--fix"),
    ] {
        if enabled {
            command.push(OsString::from(option));
        }
    }
    for module in &args.only_modules {
        command.push(OsString::from("--only"));
        command.push(OsString::from(module));
    }
    command.extend(args.modules.iter().map(OsString::from));
    command
}

fn shake_module_build_command(modules: &[String], no_build: bool) -> Vec<OsString> {
    let mut command = vec![OsString::from("lake"), OsString::from("build")];
    if no_build {
        command.push(OsString::from("--no-build"));
    }
    command.extend(
        modules
            .iter()
            .map(|module| OsString::from(format!("+{module}"))),
    );
    command
}

fn shake_verification_commands(args: &ShakeArgs) -> Vec<Vec<OsString>> {
    let mut commands = Vec::with_capacity(2);
    commands.push(vec![OsString::from("lake"), OsString::from("build")]);
    if !args.modules.is_empty() {
        commands.push(shake_module_build_command(&args.modules, false));
    }
    commands
}

fn shake_freshness_commands(args: &ShakeArgs) -> Vec<Vec<OsString>> {
    let mut commands = Vec::with_capacity(2);
    commands.push(shake_module_build_command(&[], true));
    if !args.modules.is_empty() {
        commands.push(shake_module_build_command(&args.modules, true));
    }
    commands
}

fn shake_needs_build(output: &Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [&*stdout, &*stderr].into_iter().any(|text| {
        text.contains("out of date oleans")
            || text.contains("out-of-date oleans")
            || text.contains("target is out-of-date")
            || (text.contains("object file")
                && text.contains(".olean")
                && text.contains("does not exist"))
    })
}

fn parse_shake_suggestions(stdout: &[u8]) -> Vec<ParsedSuggestion> {
    let mut current = None;
    let mut suggestions = BTreeMap::<PathBuf, ParsedSuggestion>::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        if let Some(path) = shake_heading_path(line) {
            current = Some(path);
            continue;
        }
        let Some(path) = current.as_ref() else {
            continue;
        };
        let (removed, imports) = if let Some(imports) = parse_import_array(line, "  remove #[") {
            (true, imports)
        } else if let Some(imports) = parse_import_array(line, "  add #[") {
            (false, imports)
        } else {
            continue;
        };
        let suggestion = suggestions
            .entry(path.clone())
            .or_insert_with(|| ParsedSuggestion {
                path: path.clone(),
                removed_imports: Vec::new(),
                added_imports: Vec::new(),
            });
        let target = if removed {
            &mut suggestion.removed_imports
        } else {
            &mut suggestion.added_imports
        };
        for import in imports {
            if !target.contains(&import) {
                target.push(import);
            }
        }
    }
    suggestions.into_values().collect()
}

pub(super) fn output_has_suggestions(stdout: &[u8]) -> bool {
    !parse_shake_suggestions(stdout).is_empty()
}

fn parse_import_array(line: &str, prefix: &str) -> Option<Vec<String>> {
    let imports = line.strip_prefix(prefix)?.strip_suffix(']')?;
    Some(
        imports
            .split(',')
            .map(str::trim)
            .filter(|import| !import.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn shake_heading_path(line: &str) -> Option<PathBuf> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let value = line.strip_suffix(':')?;
    (Path::new(value)
        .extension()
        .is_some_and(|ext| ext == "lean"))
    .then(|| PathBuf::from(value))
}

fn validate_shake_edits(
    project: &Project,
    suggestions: Vec<ParsedSuggestion>,
) -> Result<Vec<ValidatedEdit>> {
    let root = fs::canonicalize(&project.root)
        .with_context(|| format!("failed to resolve project root {}", project.root.display()))?;
    let packages_dir = dependency_directory(project, &root)?;
    let mut edits = BTreeMap::<PathBuf, ValidatedEdit>::new();

    for suggestion in suggestions {
        let candidate = if suggestion.path.is_absolute() {
            suggestion.path
        } else {
            root.join(suggestion.path)
        };
        let canonical = fs::canonicalize(&candidate)
            .with_context(|| format!("failed to resolve shake edit {}", candidate.display()))?;
        let relative = canonical.strip_prefix(&root).with_context(|| {
            format!(
                "refusing to edit {} because it is outside project {}",
                canonical.display(),
                root.display()
            )
        })?;
        if is_excluded_source_path(relative, packages_dir.as_deref()) {
            bail!(
                "refusing to edit generated or dependency source {}",
                canonical.display()
            );
        }
        if !canonical.is_file() {
            bail!("shake edit is not a regular file: {}", canonical.display());
        }

        let report_path = report_path(relative);
        let edit = edits
            .entry(canonical.clone())
            .or_insert_with(|| ValidatedEdit {
                path: canonical,
                report_path,
                removed_imports: Vec::new(),
                added_imports: Vec::new(),
            });
        extend_unique(&mut edit.removed_imports, suggestion.removed_imports);
        extend_unique(&mut edit.added_imports, suggestion.added_imports);
    }
    Ok(edits.into_values().collect())
}

fn extend_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn collect_coverage(
    context: &AppContext,
    source: &Project,
    project: &Project,
    args: &ShakeArgs,
) -> Result<ShakeCoverage> {
    let output = captured_in_project(
        context,
        project,
        &shake_command(args, false, true),
        !args.execution.offline,
    )?;

    let headings = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(shake_heading_path)
        .collect::<Vec<_>>();
    if !output.status.success() && headings.is_empty() {
        relay_output(&output, args.json)?;
        bail!("Lake could not determine import-analysis coverage");
    }
    relay_stderr(&output)?;

    let root = fs::canonicalize(&source.root)
        .with_context(|| format!("failed to resolve project root {}", source.root.display()))?;
    let packages_dir = dependency_directory(source, &root)?;
    let files = collect_project_lean_files(&root, packages_dir.as_deref())?;
    let module_files = files
        .iter()
        .filter_map(|(path, has_module)| has_module.then_some(path.clone()))
        .collect::<BTreeSet<_>>();
    let legacy = files
        .iter()
        .filter_map(|(path, has_module)| (!has_module).then_some(report_path(path)))
        .collect::<Vec<_>>();

    let analyzed = headings
        .into_iter()
        .filter_map(|path| {
            let relative = project_relative_path(&path, &project.root, &root)?;
            module_files.contains(&relative).then_some(relative)
        })
        .collect::<BTreeSet<_>>();
    let outside_roots = module_files
        .difference(&analyzed)
        .map(|path| report_path(path))
        .collect::<Vec<_>>();
    let analyzed = analyzed
        .iter()
        .map(|path| report_path(path))
        .collect::<Vec<_>>();

    Ok(ShakeCoverage {
        total_files: files.len(),
        analyzed,
        outside_roots,
        legacy,
    })
}

fn collect_project_lean_files(
    root: &Path,
    packages_dir: Option<&Path>,
) -> Result<BTreeMap<PathBuf, bool>> {
    let mut files = BTreeMap::new();
    visit_project_sources(root, root, packages_dir, &mut files)?;
    Ok(files)
}

fn visit_project_sources(
    root: &Path,
    directory: &Path,
    packages_dir: Option<&Path>,
    files: &mut BTreeMap<PathBuf, bool>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to read {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .context("project source traversal escaped its root")?;
        if file_type.is_dir() {
            if !is_excluded_source_path(relative, packages_dir) {
                visit_project_sources(root, &path, packages_dir, files)?;
            }
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "lean") {
            let source = fs::read_to_string(&path)
                .with_context(|| format!("failed to read Lean source {}", path.display()))?;
            files.insert(relative.to_owned(), has_module_header(&source));
        }
    }
    Ok(())
}

fn dependency_directory(project: &Project, canonical_root: &Path) -> Result<Option<PathBuf>> {
    let manifest_path = project.manifest_path();
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let manifest = LakeManifest::read(&manifest_path)?;
    let packages = manifest.packages_path(canonical_root)?;
    Ok(packages
        .strip_prefix(canonical_root)
        .ok()
        .map(Path::to_owned))
}

fn is_excluded_source_path(relative: &Path, packages_dir: Option<&Path>) -> bool {
    if packages_dir.is_some_and(|packages| relative.starts_with(packages)) {
        return true;
    }
    relative.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(
                    name.to_str(),
                    Some(
                        ".git"
                            | ".lake"
                            | "_build"
                            | "_target"
                            | "build"
                            | "lake-packages"
                            | "lean_packages"
                            | "target"
                    )
                )
        )
    })
}

fn has_module_header(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = usize::from(bytes.starts_with(&[0xef, 0xbb, 0xbf])) * 3;

    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index..index + 2) == Some(b"--") {
            index += 2;
            while bytes.get(index).is_some_and(|byte| *byte != b'\n') {
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"/-") {
            index += 2;
            let mut depth = 1usize;
            while index < bytes.len() && depth > 0 {
                if bytes.get(index..index + 2) == Some(b"/-") {
                    depth += 1;
                    index += 2;
                } else if bytes.get(index..index + 2) == Some(b"-/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            continue;
        }
        break;
    }

    let Some(rest) = source.get(index..) else {
        return false;
    };
    let Some(after) = rest.strip_prefix("module") else {
        return false;
    };
    after.is_empty()
        || after
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'-' | b'/'))
}

fn suggestion_reports(
    source_root: &Path,
    execution_root: &Path,
    suggestions: &[ParsedSuggestion],
) -> Vec<ShakeFileReport> {
    suggestions
        .iter()
        .map(|suggestion| ShakeFileReport {
            path: display_suggestion_path(source_root, execution_root, &suggestion.path),
            removed_imports: suggestion.removed_imports.clone(),
            added_imports: suggestion.added_imports.clone(),
            disposition: FileDisposition::Proposed,
            keep_annotations: Vec::new(),
        })
        .collect()
}

fn display_suggestion_path(source_root: &Path, execution_root: &Path, path: &Path) -> String {
    project_relative_path(path, execution_root, source_root)
        .map(|relative| report_path(&relative))
        .unwrap_or_else(|| report_path(path))
}

fn project_relative_path(
    path: &Path,
    execution_root: &Path,
    source_root: &Path,
) -> Option<PathBuf> {
    let execution_root = fs::canonicalize(execution_root).ok()?;
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        execution_root.join(path)
    };
    let candidate = fs::canonicalize(candidate).ok()?;
    if let Ok(relative) = candidate.strip_prefix(&execution_root) {
        return Some(relative.to_owned());
    }

    let source_root = fs::canonicalize(source_root).ok()?;
    candidate.strip_prefix(source_root).ok().map(Path::to_owned)
}

fn report_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn set_all_dispositions(reports: &mut [ShakeFileReport], disposition: FileDisposition) {
    for report in reports {
        report.disposition = disposition;
        report.keep_annotations.clear();
    }
}

fn set_disposition(
    reports: &mut [ShakeFileReport],
    path: &str,
    disposition: FileDisposition,
    keep_annotations: &[String],
) {
    if let Some(report) = reports.iter_mut().find(|report| report.path == path) {
        report.disposition = disposition;
        report.keep_annotations = keep_annotations.to_vec();
    }
}

fn finish(
    args: &ShakeArgs,
    status: ShakeStatus,
    files: Vec<ShakeFileReport>,
    coverage: Option<ShakeCoverage>,
    code: i32,
) -> ShakeOutcome {
    let mode = if args.isolate {
        ShakeMode::Isolate
    } else if args.fix {
        ShakeMode::Fix
    } else if args.check {
        ShakeMode::Check
    } else {
        ShakeMode::Preview
    };
    ShakeOutcome {
        code,
        report: ShakeReport {
            mode,
            status,
            files,
            coverage,
        },
    }
}

pub(super) fn emit_report(args: &ShakeArgs, report: &ShakeReport) -> Result<()> {
    if let Some(path) = &args.sarif {
        write_sarif(path, [(None, report)])
    } else if args.json {
        json_output::print(schema::SHAKE, report)
    } else {
        if let Some(coverage) = &report.coverage {
            print_coverage(coverage)?;
        }
        Ok(())
    }
}

fn machine_output(args: &ShakeArgs) -> bool {
    args.json || args.sarif.is_some()
}

pub(super) fn write_workspace_sarif<'a>(
    path: &Path,
    reports: impl IntoIterator<Item = (&'a str, &'a ShakeReport)>,
) -> Result<()> {
    write_sarif(
        path,
        reports
            .into_iter()
            .map(|(member, report)| (Some(member), report)),
    )
}

fn write_sarif<'a>(
    path: &Path,
    reports: impl IntoIterator<Item = (Option<&'a str>, &'a ShakeReport)>,
) -> Result<()> {
    let mut results = Vec::new();
    for (member, report) in reports {
        results.extend(report.files.iter().filter_map(|file| {
            if matches!(file.disposition, FileDisposition::Applied) {
                return None;
            }
            let rejected = matches!(
                file.disposition,
                FileDisposition::Rejected | FileDisposition::Restored
            );
            let rule = if rejected {
                "lev.import-rewrite-rejected"
            } else {
                "lev.import-drift"
            };
            let mut changes = Vec::new();
            if !file.removed_imports.is_empty() {
                changes.push(format!("remove {}", file.removed_imports.join(", ")));
            }
            if !file.added_imports.is_empty() {
                changes.push(format!("add {}", file.added_imports.join(", ")));
            }
            let uri = sarif_uri(member, &file.path);
            Some(serde_json::json!({
                "ruleId": rule,
                "level": if rejected { "error" } else { "warning" },
                "message": {
                    "text": if changes.is_empty() {
                        "Lean import policy check failed".to_owned()
                    } else {
                        format!("Import minimization: {}", changes.join("; "))
                    }
                },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": uri },
                        "region": { "startLine": 1, "startColumn": 1 }
                    }
                }]
            }))
        }));
    }
    let document = serde_json::json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "lev",
                    "informationUri": env!("CARGO_PKG_HOMEPAGE"),
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": [
                        {
                            "id": "lev.import-drift",
                            "shortDescription": { "text": "Lean imports can be minimized" }
                        },
                        {
                            "id": "lev.import-rewrite-rejected",
                            "shortDescription": {
                                "text": "A proposed Lean import rewrite did not verify"
                            }
                        }
                    ]
                }
            },
            "results": results
        }]
    });
    let mut bytes = serde_json::to_vec_pretty(&document).context("failed to serialize SARIF")?;
    bytes.push(b'\n');
    if path == Path::new("-") {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write SARIF output")
    } else {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_replace(path, &bytes)
    }
}

fn sarif_uri(member: Option<&str>, path: &str) -> String {
    let joined = member
        .map(|member| format!("{member}/{path}"))
        .unwrap_or_else(|| path.to_owned());
    let mut encoded = String::with_capacity(joined.len());
    for byte in joined.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn print_coverage(coverage: &ShakeCoverage) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "Import analysis coverage:")?;
    writeln!(
        output,
        "  analyzed: {} of {} Lean files",
        coverage.analyzed.len(),
        coverage.total_files
    )?;
    for path in &coverage.analyzed {
        writeln!(output, "    {path}")?;
    }
    writeln!(
        output,
        "  outside selected roots: {}",
        coverage.outside_roots.len()
    )?;
    for path in &coverage.outside_roots {
        writeln!(output, "    {path}")?;
    }
    writeln!(
        output,
        "  legacy (no module header): {}",
        coverage.legacy.len()
    )?;
    for path in &coverage.legacy {
        writeln!(output, "    {path}")?;
    }
    Ok(())
}

fn relay_output(output: &Output, json: bool) -> Result<()> {
    if !json {
        io::stdout()
            .write_all(&output.stdout)
            .context("failed to write shake output")?;
    }
    relay_stderr(output)
}

fn relay_stderr(output: &Output) -> Result<()> {
    io::stderr()
        .write_all(&output.stderr)
        .context("failed to write shake diagnostics")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{
        display_suggestion_path, has_module_header, parse_shake_suggestions, project_relative_path,
        sarif_uri,
    };

    #[test]
    fn sarif_uris_escape_reserved_and_non_ascii_path_bytes() {
        assert_eq!(
            sarif_uri(Some("packages/a b"), "Proof #1/α.lean"),
            "packages/a%20b/Proof%20%231/%CE%B1.lean"
        );
    }

    #[test]
    fn parses_multiple_import_edits_without_losing_context_to_github_diagnostics() {
        let output = br#"Example.lean:
  remove #[public import Mathlib, import Init]
Example.lean:3:1: warning: unused import
  add #[public import Mathlib.Data.Nat.Prime.Defs]
"#;
        let suggestions = parse_shake_suggestions(output);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(
            suggestions[0].removed_imports,
            ["public import Mathlib", "import Init"]
        );
        assert_eq!(
            suggestions[0].added_imports,
            ["public import Mathlib.Data.Nat.Prime.Defs"]
        );
    }

    #[test]
    fn recognizes_module_after_nested_header_comments_only() {
        assert!(has_module_header(
            "/- copyright /- nested -/ text -/\n-- note\nmodule -- shake: keep-all\n"
        ));
        assert!(!has_module_header(
            "/- module -/\nimport Mathlib\n\nmodule\n"
        ));
        assert!(!has_module_header("moduleName := \"not a header\"\n"));
    }

    #[test]
    fn maps_execution_workspace_paths_back_to_project_relative_paths() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let execution = temp.path().join("execution");
        let relative = Path::new("Example/Proof.lean");
        fs::create_dir_all(source.join("Example")).unwrap();
        fs::create_dir_all(execution.join("Example")).unwrap();
        fs::write(source.join(relative), "module\n").unwrap();
        fs::write(execution.join(relative), "module\n").unwrap();

        assert_eq!(
            project_relative_path(&execution.join(relative), &execution, &source),
            Some(relative.to_owned())
        );
        assert_eq!(
            project_relative_path(&source.join(relative), &execution, &source),
            Some(relative.to_owned())
        );
        assert_eq!(
            display_suggestion_path(&source, &execution, relative),
            "Example/Proof.lean"
        );
    }
}
