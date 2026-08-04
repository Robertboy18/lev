//! Read-only project verification with optional build policy gates.

use std::ffi::OsString;
use std::io::{self, Write};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cli::VerifyArgs;
use crate::core::json_output::{self, schema};
use crate::core::process::exit_code;
use crate::dependency::environment as dependency_env;
use crate::project::audit::AuditReport;
use crate::project::lockfile;

use super::AppContext;
use super::execution_commands::{display_os_command, project_command};
use super::toolchain_commands::ensure_toolchain;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum VerificationStatus {
    Passed,
    Failed,
}

#[derive(Debug, Serialize)]
struct VerificationPhase {
    name: &'static str,
    status: VerificationStatus,
    elapsed_ms: f64,
    detail: String,
}

#[derive(Debug, Serialize)]
struct VerificationReport {
    project: String,
    toolchain: String,
    status: VerificationStatus,
    phases: Vec<VerificationPhase>,
}

pub(super) fn verify(context: &AppContext, args: VerifyArgs) -> Result<i32> {
    let project = context.project()?;
    let mut phases = Vec::new();

    let lock_ok = record_check(&mut phases, "lock", || {
        lockfile::verify(&project)?;
        Ok("lev.lock matches project configuration and manifest".to_owned())
    });
    if !lock_ok {
        return emit_report(&args, &project, phases, 1);
    }

    let audit_started = Instant::now();
    let audit = AuditReport::inspect(&project, &context.git, true);
    let audit_code = audit.exit_code(args.strict);
    phases.push(VerificationPhase {
        name: "audit",
        status: if audit_code == 0 {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        },
        elapsed_ms: audit_started.elapsed().as_secs_f64() * 1000.0,
        detail: format!(
            "{} passed, {} warnings, {} errors",
            audit.summary.passed, audit.summary.warnings, audit.summary.errors
        ),
    });
    if audit_code != 0 {
        return emit_report(&args, &project, phases, audit_code);
    }

    let toolchain_ok = record_check(&mut phases, "toolchain", || {
        ensure_toolchain(context, &project, !args.offline)?;
        Ok(format!("{} is available", project.toolchain))
    });
    if !toolchain_ok {
        return emit_report(&args, &project, phases, 1);
    }

    let _dependency_environment = dependency_env::lock_attached(&context.cache, &project)?;
    let _artifact_cache = lake_artifact_cache::lock_shared(&context.cache, &project.toolchain)?;
    registry::record(&context.cache, &project)?;

    let mut commands = Vec::<(&'static str, Vec<OsString>, bool)>::new();
    if args.clean {
        commands.push((
            "clean",
            vec![OsString::from("lake"), OsString::from("clean")],
            false,
        ));
    }
    let mut build = vec![OsString::from("lake"), OsString::from("build")];
    build.extend(args.build_args.clone());
    commands.push(("build", build, false));
    if args.test {
        commands.push((
            "test",
            vec![OsString::from("lake"), OsString::from("test")],
            false,
        ));
    }
    if args.lint {
        commands.push((
            "lint",
            vec![OsString::from("lake"), OsString::from("lint")],
            false,
        ));
    }
    if args.imports {
        commands.push((
            "imports",
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
        phases.push(VerificationPhase {
            name,
            status: if code == 0 {
                VerificationStatus::Passed
            } else {
                VerificationStatus::Failed
            },
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            detail: if code == 0 {
                display_os_command(&command)
            } else if import_drift {
                "Lake reported imports that can be minimized".to_owned()
            } else {
                format!("{} exited with {code}", display_os_command(&command))
            },
        });
        if code != 0 {
            final_code = code;
            break;
        }
    }
    emit_report(&args, &project, phases, final_code)
}

fn record_check(
    phases: &mut Vec<VerificationPhase>,
    name: &'static str,
    check: impl FnOnce() -> Result<String>,
) -> bool {
    let started = Instant::now();
    let result = check();
    phases.push(VerificationPhase {
        name,
        status: if result.is_ok() {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        },
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        detail: match result {
            Ok(detail) => detail,
            Err(error) => format!("{error:#}"),
        },
    });
    phases
        .last()
        .is_some_and(|phase| matches!(phase.status, VerificationStatus::Passed))
}

fn relay_process_output(output: &std::process::Output, json: bool) -> Result<()> {
    if json {
        let mut diagnostics = io::stderr().lock();
        diagnostics
            .write_all(&output.stdout)
            .context("failed to relay verification stdout")?;
        diagnostics
            .write_all(&output.stderr)
            .context("failed to relay verification stderr")?;
    } else {
        io::stdout()
            .lock()
            .write_all(&output.stdout)
            .context("failed to relay verification stdout")?;
        io::stderr()
            .lock()
            .write_all(&output.stderr)
            .context("failed to relay verification stderr")?;
    }
    Ok(())
}

fn emit_report(
    args: &VerifyArgs,
    project: &crate::project::Project,
    phases: Vec<VerificationPhase>,
    code: i32,
) -> Result<i32> {
    let report = VerificationReport {
        project: project.root.display().to_string(),
        toolchain: project.toolchain.clone(),
        status: if code == 0 {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        },
        phases,
    };
    if args.json {
        json_output::print(schema::VERIFY, &report)?;
    } else {
        for phase in &report.phases {
            let status = match phase.status {
                VerificationStatus::Passed => "PASS",
                VerificationStatus::Failed => "FAIL",
            };
            println!(
                "{status}\t{:<12}\t{:>10.3} ms\t{}",
                phase.name, phase.elapsed_ms, phase.detail
            );
        }
    }
    Ok(code)
}
