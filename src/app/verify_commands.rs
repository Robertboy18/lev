//! Read-only project checks with repository-owned policy gates.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Write};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cli::{CheckArgs, VerifyArgs};
use crate::core::json_output::{self, schema};
use crate::core::process::exit_code;
use crate::dependency::environment as dependency_env;
use crate::dependency::pin_analysis::{self, PinAnalysis, PinStatus};
use crate::diagnostics::{Finding, FindingCategory, PolicyDisposition, RunStatus};
use crate::project::audit::AuditReport;
use crate::project::config::LevConfig;
use crate::project::lakefile;
use crate::project::lockfile;

use super::AppContext;
use super::execution_commands::{display_os_command, project_command};
use super::toolchain_commands::ensure_toolchain;

#[derive(Debug, Serialize)]
struct VerificationPhase {
    name: String,
    status: RunStatus,
    elapsed_ms: f64,
    detail: String,
}

impl VerificationPhase {
    /// Adapt the shared finding model to the published check/verify DTO.
    fn from_finding(name: impl Into<String>, finding: Finding) -> Self {
        let status = match finding.policy {
            PolicyDisposition::Observe | PolicyDisposition::Pass => RunStatus::Passed,
            PolicyDisposition::Fail => RunStatus::Failed,
        };
        Self {
            name: name.into(),
            status,
            elapsed_ms: finding.elapsed_ms.unwrap_or_default(),
            detail: finding.detail,
        }
    }
}

#[derive(Debug, Serialize)]
struct VerificationReport {
    project: String,
    toolchain: String,
    status: RunStatus,
    phases: Vec<VerificationPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trust: Option<super::inspect_commands::TrustInspectionReport>,
}

pub(super) fn check(context: &AppContext, args: CheckArgs) -> Result<i32> {
    let project = context.project()?;
    let mut phases = Vec::new();
    let mut loaded_config = None;

    let configuration_ok = record_check(
        &mut phases,
        "configuration",
        "check.configuration",
        FindingCategory::Configuration,
        || {
            let config = LevConfig::read(&project.root)?;
            let toml = project.root.join("lakefile.toml");
            let lean = project.root.join("lakefile.lean");
            let detail = if toml.is_file() {
                let dependencies = lakefile::dependency_names(&toml)?;
                format!(
                    "parsed lakefile.toml and lev.toml ({} direct dependencies)",
                    dependencies.len()
                )
            } else if lean.is_file() {
                "found executable lakefile.lean and parsed lev.toml".to_owned()
            } else {
                bail!("project has neither lakefile.toml nor lakefile.lean")
            };
            validate_requested_tasks(&config, &args.tasks)?;
            loaded_config = Some(config);
            Ok(detail)
        },
    );
    if !configuration_ok {
        return emit_report(&args, &project, phases, None, 1);
    }
    let config = loaded_config.expect("successful configuration phase stores the parsed config");

    let lock_ok = record_check(
        &mut phases,
        "lock",
        "check.lock",
        FindingCategory::Project,
        || {
            lockfile::verify(&project)?;
            Ok("lev.lock matches project configuration and manifest".to_owned())
        },
    );
    if !lock_ok {
        return emit_report(&args, &project, phases, None, 1);
    }

    let dependencies_ok = record_check(
        &mut phases,
        "dependencies",
        "check.dependencies",
        FindingCategory::Dependency,
        || {
            let analysis = pin_analysis::analyze(&project)?;
            let detail = dependency_pin_detail(&analysis);
            if analysis.count(PinStatus::Stale) > 0 {
                bail!("{detail}")
            }
            Ok(detail)
        },
    );
    if !dependencies_ok {
        return emit_report(&args, &project, phases, None, 1);
    }

    let audit_started = Instant::now();
    let audit = AuditReport::inspect(&project, &context.git, true);
    let audit_code = audit.exit_code(args.strict);
    push_phase(
        &mut phases,
        "audit",
        "check.audit",
        FindingCategory::Project,
        audit_code == 0,
        audit_started.elapsed().as_secs_f64() * 1000.0,
        format!(
            "{} passed, {} warnings, {} errors",
            audit.summary.passed, audit.summary.warnings, audit.summary.errors
        ),
    );
    if audit_code != 0 {
        return emit_report(&args, &project, phases, None, audit_code);
    }

    let toolchain_ok = record_check(
        &mut phases,
        "toolchain",
        "check.toolchain",
        FindingCategory::Environment,
        || {
            ensure_toolchain(context, &project, !args.offline)?;
            Ok(format!("{} is available", project.toolchain))
        },
    );
    if !toolchain_ok {
        return emit_report(&args, &project, phases, None, 1);
    }

    let _dependency_environment = dependency_env::lock_attached(&context.cache, &project)?;
    let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
    registry::record(&context.cache, &project)?;

    let mut commands = Vec::<(String, Vec<OsString>, bool)>::new();
    if args.clean {
        commands.push((
            "clean".to_owned(),
            vec![OsString::from("lake"), OsString::from("clean")],
            false,
        ));
    }
    let mut build = vec![OsString::from("lake"), OsString::from("build")];
    build.extend(args.args.clone());
    commands.push(("build".to_owned(), build, false));
    for task in selected_tasks(&config, &args.tasks) {
        let command = config
            .tasks
            .get(&task)
            .expect("requested tasks were validated")
            .clone();
        commands.push((format!("task:{task}"), command, false));
    }
    if config.check.imports || args.imports {
        commands.push((
            "imports".to_owned(),
            vec![OsString::from("lake"), OsString::from("shake")],
            true,
        ));
    }

    let mut final_code = 0;
    for (name, command, detect_import_drift) in commands {
        let started = Instant::now();
        let mut process = project_command(context, &project, &command, !args.offline)?;
        let output = process
            .output()
            .with_context(|| format!("failed to start {}", display_os_command(&command)))?;
        relay_process_output(&output, args.json)?;
        let import_drift = detect_import_drift
            && (super::shake_commands::output_has_suggestions(&output.stdout)
                || super::shake_commands::output_has_suggestions(&output.stderr));
        let code = if import_drift {
            1
        } else {
            exit_code(output.status)
        };
        let category = if name == "build" {
            FindingCategory::Build
        } else {
            FindingCategory::Project
        };
        let id = match name.as_str() {
            "clean" => "check.command.clean",
            "build" => "check.command.build",
            "imports" => "check.command.imports",
            _ => "check.command.task",
        };
        push_phase(
            &mut phases,
            name,
            id,
            category,
            code == 0,
            started.elapsed().as_secs_f64() * 1000.0,
            if code == 0 {
                display_os_command(&command)
            } else if import_drift {
                "Lake reported imports that can be minimized".to_owned()
            } else {
                format!("{} exited with {code}", display_os_command(&command))
            },
        );
        if code != 0 {
            final_code = code;
            break;
        }
    }

    let mut trust = None;
    if final_code == 0
        && (!config.check.denied_axioms.is_empty()
            || !args.denied_axioms.is_empty()
            || config.check.deny_unsafe
            || args.deny_unsafe)
    {
        let mut denied_axioms = config
            .check
            .denied_axioms
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        denied_axioms.extend(args.denied_axioms.iter().cloned());
        denied_axioms.sort();
        denied_axioms.dedup();

        let started = Instant::now();
        match super::inspect_commands::scan_trust(
            context,
            &project,
            &project,
            &denied_axioms,
            config.check.deny_unsafe || args.deny_unsafe,
            false,
            args.offline,
        ) {
            Ok(report) => {
                let failed = report.failed();
                push_phase(
                    &mut phases,
                    "trust",
                    "check.trust",
                    FindingCategory::Trust,
                    !failed,
                    started.elapsed().as_secs_f64() * 1000.0,
                    report.check_detail(),
                );
                if failed {
                    final_code = 1;
                }
                trust = Some(report);
            }
            Err(error) => {
                push_phase(
                    &mut phases,
                    "trust",
                    "check.trust",
                    FindingCategory::Trust,
                    false,
                    started.elapsed().as_secs_f64() * 1000.0,
                    format!("{error:#}"),
                );
                final_code = 1;
            }
        }
    }

    emit_report(&args, &project, phases, trust, final_code)
}

/// Preserve the published `lev verify` behavior and JSON contract.
pub(super) fn verify(context: &AppContext, args: VerifyArgs) -> Result<i32> {
    let project = context.project()?;
    let mut phases = Vec::new();

    let lock_ok = record_check(
        &mut phases,
        "lock",
        "verify.lock",
        FindingCategory::Project,
        || {
            lockfile::verify(&project)?;
            Ok("lev.lock matches project configuration and manifest".to_owned())
        },
    );
    if !lock_ok {
        return emit_legacy_report(&args, &project, phases, 1);
    }

    let audit_started = Instant::now();
    let audit = AuditReport::inspect(&project, &context.git, true);
    let audit_code = audit.exit_code(args.strict);
    push_phase(
        &mut phases,
        "audit",
        "verify.audit",
        FindingCategory::Project,
        audit_code == 0,
        audit_started.elapsed().as_secs_f64() * 1000.0,
        format!(
            "{} passed, {} warnings, {} errors",
            audit.summary.passed, audit.summary.warnings, audit.summary.errors
        ),
    );
    if audit_code != 0 {
        return emit_legacy_report(&args, &project, phases, audit_code);
    }

    let toolchain_ok = record_check(
        &mut phases,
        "toolchain",
        "verify.toolchain",
        FindingCategory::Environment,
        || {
            ensure_toolchain(context, &project, !args.offline)?;
            Ok(format!("{} is available", project.toolchain))
        },
    );
    if !toolchain_ok {
        return emit_legacy_report(&args, &project, phases, 1);
    }

    let _dependency_environment = dependency_env::lock_attached(&context.cache, &project)?;
    let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
    registry::record(&context.cache, &project)?;

    let mut commands = Vec::<(String, Vec<OsString>, bool)>::new();
    if args.clean {
        commands.push((
            "clean".to_owned(),
            vec![OsString::from("lake"), OsString::from("clean")],
            false,
        ));
    }
    let mut build = vec![OsString::from("lake"), OsString::from("build")];
    build.extend(args.build_args.clone());
    commands.push(("build".to_owned(), build, false));
    if args.test {
        commands.push((
            "test".to_owned(),
            vec![OsString::from("lake"), OsString::from("test")],
            false,
        ));
    }
    if args.lint {
        commands.push((
            "lint".to_owned(),
            vec![OsString::from("lake"), OsString::from("lint")],
            false,
        ));
    }
    if args.imports {
        commands.push((
            "imports".to_owned(),
            vec![OsString::from("lake"), OsString::from("shake")],
            true,
        ));
    }

    let mut final_code = 0;
    for (name, command, detect_import_drift) in commands {
        let started = Instant::now();
        let mut process = project_command(context, &project, &command, !args.offline)?;
        let output = process
            .output()
            .with_context(|| format!("failed to start {}", display_os_command(&command)))?;
        relay_process_output(&output, args.json)?;
        let import_drift = detect_import_drift
            && (super::shake_commands::output_has_suggestions(&output.stdout)
                || super::shake_commands::output_has_suggestions(&output.stderr));
        let code = if import_drift {
            1
        } else {
            exit_code(output.status)
        };
        let category = if name == "build" {
            FindingCategory::Build
        } else {
            FindingCategory::Project
        };
        let id = match name.as_str() {
            "clean" => "verify.command.clean",
            "build" => "verify.command.build",
            "test" => "verify.command.test",
            "lint" => "verify.command.lint",
            "imports" => "verify.command.imports",
            _ => "verify.command",
        };
        push_phase(
            &mut phases,
            name,
            id,
            category,
            code == 0,
            started.elapsed().as_secs_f64() * 1000.0,
            if code == 0 {
                display_os_command(&command)
            } else if import_drift {
                "Lake reported imports that can be minimized".to_owned()
            } else {
                format!("{} exited with {code}", display_os_command(&command))
            },
        );
        if code != 0 {
            final_code = code;
            break;
        }
    }
    emit_legacy_report(&args, &project, phases, final_code)
}

fn validate_requested_tasks(config: &LevConfig, requested: &[String]) -> Result<()> {
    for task in requested {
        if !config.tasks.contains_key(task) {
            let path = config
                .path
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("lev.toml"));
            bail!("task {task:?} is not configured in {}", path.display());
        }
    }
    Ok(())
}

fn selected_tasks(config: &LevConfig, requested: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    config
        .check
        .tasks
        .iter()
        .chain(requested)
        .filter(|task| seen.insert((*task).clone()))
        .cloned()
        .collect()
}

fn dependency_pin_detail(analysis: &PinAnalysis) -> String {
    let stale = analysis.count(PinStatus::Stale);
    let conflict = analysis.count(PinStatus::Conflict);
    let root_override = analysis.count(PinStatus::RootOverride);
    let unknown = analysis.count(PinStatus::Unknown);
    let mut detail = format!(
        "{stale} stale, {conflict} conflict{}, {root_override} root override{}, {unknown} unknown",
        if conflict == 1 { "" } else { "s" },
        if root_override == 1 { "" } else { "s" },
    );
    if stale > 0 {
        let names = analysis
            .packages
            .iter()
            .filter(|package| package.status == PinStatus::Stale)
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        detail.push_str(&format!("; stale selections: {names}"));
    }
    detail
}

fn record_check(
    phases: &mut Vec<VerificationPhase>,
    name: impl Into<String>,
    id: &'static str,
    category: FindingCategory,
    check: impl FnOnce() -> Result<String>,
) -> bool {
    let started = Instant::now();
    let result = check();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let finding = match result {
        Ok(detail) => Finding::passed(id, category, name.into(), detail)
            .with_elapsed_ms(elapsed_ms)
            .with_policy(PolicyDisposition::Pass),
        Err(error) => Finding::error(id, category, name.into(), format!("{error:#}"))
            .with_elapsed_ms(elapsed_ms)
            .with_policy(PolicyDisposition::Fail),
    };
    phases.push(VerificationPhase::from_finding(
        finding.title.clone(),
        finding,
    ));
    phases
        .last()
        .is_some_and(|phase| matches!(phase.status, RunStatus::Passed))
}

fn push_phase(
    phases: &mut Vec<VerificationPhase>,
    name: impl Into<String>,
    id: &'static str,
    category: FindingCategory,
    passed: bool,
    elapsed_ms: f64,
    detail: String,
) {
    let name = name.into();
    let finding = if passed {
        Finding::passed(id, category, name.clone(), detail).with_policy(PolicyDisposition::Pass)
    } else {
        Finding::error(id, category, name.clone(), detail).with_policy(PolicyDisposition::Fail)
    }
    .with_elapsed_ms(elapsed_ms);
    phases.push(VerificationPhase::from_finding(name, finding));
}

fn relay_process_output(output: &std::process::Output, json: bool) -> Result<()> {
    if json {
        let mut diagnostics = io::stderr().lock();
        diagnostics
            .write_all(&output.stdout)
            .context("failed to relay check stdout")?;
        diagnostics
            .write_all(&output.stderr)
            .context("failed to relay check stderr")?;
    } else {
        io::stdout()
            .lock()
            .write_all(&output.stdout)
            .context("failed to relay check stdout")?;
        io::stderr()
            .lock()
            .write_all(&output.stderr)
            .context("failed to relay check stderr")?;
    }
    Ok(())
}

fn emit_report(
    args: &CheckArgs,
    project: &crate::project::Project,
    phases: Vec<VerificationPhase>,
    trust: Option<super::inspect_commands::TrustInspectionReport>,
    code: i32,
) -> Result<i32> {
    emit_report_with_schema(project, phases, trust, code, args.json, schema::CHECK, 16)
}

fn emit_legacy_report(
    args: &VerifyArgs,
    project: &crate::project::Project,
    phases: Vec<VerificationPhase>,
    code: i32,
) -> Result<i32> {
    emit_report_with_schema(project, phases, None, code, args.json, schema::VERIFY, 12)
}

fn emit_report_with_schema(
    project: &crate::project::Project,
    phases: Vec<VerificationPhase>,
    trust: Option<super::inspect_commands::TrustInspectionReport>,
    code: i32,
    json: bool,
    report_schema: &'static str,
    name_width: usize,
) -> Result<i32> {
    let report = VerificationReport {
        project: project.root.display().to_string(),
        toolchain: project.toolchain.clone(),
        status: if code == 0 {
            RunStatus::Passed
        } else {
            RunStatus::Failed
        },
        phases,
        trust,
    };
    if json {
        json_output::print(report_schema, &report)?;
    } else {
        for phase in &report.phases {
            let status = match phase.status {
                RunStatus::Passed => "PASS",
                RunStatus::Failed => "FAIL",
            };
            println!(
                "{status}\t{:<name_width$}\t{:>10.3} ms\t{}",
                phase.name, phase.elapsed_ms, phase.detail,
            );
        }
        if let Some(trust) = &report.trust {
            super::inspect_commands::print_trust(trust);
        }
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use crate::diagnostics::{Finding, FindingCategory, PolicyDisposition, RunStatus};

    use super::VerificationPhase;

    #[test]
    fn verification_adapter_uses_policy_instead_of_severity() {
        let observed_error = Finding::error(
            "check.example",
            FindingCategory::Project,
            "example",
            "the condition is serious but this command only observes it",
        );
        assert_eq!(
            VerificationPhase::from_finding("example", observed_error).status,
            RunStatus::Passed
        );

        let policy_failure = Finding::warning(
            "check.warning",
            FindingCategory::Project,
            "warning",
            "the repository rejects this warning",
        )
        .with_policy(PolicyDisposition::Fail);
        assert_eq!(
            VerificationPhase::from_finding("warning", policy_failure).status,
            RunStatus::Failed
        );
    }
}
