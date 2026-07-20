//! Multi-toolchain command planning and execution.
//!
//! Matrix entries use the normal project preparation and execution paths.

use std::collections::HashSet;
use std::ffi::OsString;

use anyhow::{Result, bail};

use crate::cli::MatrixArgs;
use crate::project::config::LevConfig;
use crate::toolchain;

use super::AppContext;
use super::environment_commands;
use super::execution_commands::run_in_project;
use super::sync_commands::sync;
use super::toolchain_commands::ensure_toolchain_name;
use crate::cli::SyncArgs;

/// Initialize or execute a deterministic set of Lean toolchain environments.
pub(super) fn matrix(context: &AppContext, args: MatrixArgs) -> Result<i32> {
    let base = context.project()?;

    if args.init {
        let requested = if args.toolchains.is_empty() {
            vec![base.toolchain.clone()]
        } else {
            args.toolchains
        };
        let toolchains = normalize_unique(requested)?;
        let command = explicit_or_default(args.command);
        let path = LevConfig::initialize_matrix(&base.root, &toolchains, &command)?;
        context.info(format!(
            "initialized {} with {} matrix toolchain{}",
            path.display(),
            toolchains.len(),
            if toolchains.len() == 1 { "" } else { "s" }
        ));
        return Ok(0);
    }

    let config = LevConfig::read(&base.root)?;
    let requested = if args.toolchains.is_empty() {
        config.matrix_toolchains
    } else {
        args.toolchains
    };
    if requested.is_empty() {
        bail!(
            "no matrix toolchains configured\n\
             initialize lev.toml with `lev matrix --init`, or run once with \
             `lev matrix --lean VERSION_A --lean VERSION_B`"
        );
    }
    // Normalize the complete plan before resolving toolchains or creating a
    // workspace. A typo in a later entry must not leave a partial matrix run.
    let toolchains = normalize_unique(requested)?;
    let command = if !args.command.is_empty() {
        args.command
    } else if !config.matrix_command.is_empty() {
        config.matrix_command
    } else {
        default_command()
    };

    // Resolve every selector before materializing the first workspace. Online
    // runs may populate the toolchain cache, but no matrix command or project
    // copy starts unless the complete set can be selected.
    for selected in &toolchains {
        ensure_toolchain_name(context, selected, !args.offline)?;
    }

    if args.in_place
        && toolchains
            .iter()
            .any(|toolchain| toolchain != &base.toolchain)
    {
        bail!(
            "`lev matrix --in-place` cannot activate a different version-resolved \
             dependency graph; remove --in-place to use isolated environments"
        );
    }

    // Resolve or validate the complete environment plan before the first user
    // command starts. A bad later dependency graph must not leave a CI matrix
    // that ran only an accidental prefix.
    let mut projects = Vec::with_capacity(toolchains.len());
    for selected in &toolchains {
        let project = if args.in_place {
            base.clone()
        } else {
            environment_commands::select(context, &base, selected, args.offline, true)?
        };
        projects.push(project);
    }

    let mut first_failure = None;

    for (selected, project) in toolchains.into_iter().zip(projects) {
        context.info(format!("matrix environment {selected}"));
        let result = sync(
            context,
            &project,
            SyncArgs {
                offline: args.offline,
                update: false,
                locked: false,
                frozen: false,
            },
        )
        .and_then(|_| run_in_project(context, &project, &command, !args.offline));
        match result {
            Ok(0) => {}
            Ok(code) => {
                eprintln!("{selected}: command failed with exit code {code}");
                first_failure.get_or_insert(code);
                if !args.keep_going {
                    return Ok(code);
                }
            }
            Err(error) => {
                eprintln!("{selected}: {error:#}");
                first_failure.get_or_insert(1);
                if !args.keep_going {
                    return Err(error);
                }
            }
        }
    }
    Ok(first_failure.unwrap_or(0))
}

/// Normalize and deduplicate while retaining the user's stable matrix order.
fn normalize_unique(requested: Vec<String>) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(requested.len());
    for requested in requested {
        let selected = toolchain::normalize(&requested)?;
        if seen.insert(selected.clone()) {
            normalized.push(selected);
        }
    }
    Ok(normalized)
}

fn explicit_or_default(command: Vec<OsString>) -> Vec<OsString> {
    if command.is_empty() {
        default_command()
    } else {
        command
    }
}

fn default_command() -> Vec<OsString> {
    vec![OsString::from("lake"), OsString::from("build")]
}

#[cfg(test)]
mod tests {
    use super::normalize_unique;

    #[test]
    fn normalization_preserves_order_and_removes_alias_duplicates() {
        let selected = normalize_unique(vec![
            "4.fixture-d".to_owned(),
            "leanprover/lean4:v4.fixture-c".to_owned(),
            "v4.fixture-d".to_owned(),
        ])
        .unwrap();

        assert_eq!(
            selected,
            [
                "leanprover/lean4:v4.fixture-d",
                "leanprover/lean4:v4.fixture-c"
            ]
        );
    }

    #[test]
    fn normalization_validates_the_entire_plan() {
        let error = normalize_unique(vec!["4.fixture-d".to_owned(), "bad name".to_owned()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot contain whitespace"), "{error}");
    }
}
