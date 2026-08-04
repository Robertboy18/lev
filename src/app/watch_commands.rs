//! Event-driven project watching for fast edit/build loops.

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::cli::WatchArgs;

use super::AppContext;
use super::execution_commands::{display_os_command, prepared_project, run_in_project};

const MAX_WATCH_DIRECTORIES: usize = 250_000;

pub(super) fn watch(context: &AppContext, args: WatchArgs) -> Result<i32> {
    let source = context.project()?;
    let command = if args.command.is_empty() {
        vec![OsString::from("lake"), OsString::from("build")]
    } else {
        args.command.clone()
    };
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .context("failed to initialize the filesystem watcher")?;
    watch_tree(&mut watcher, &source.root)?;

    context.info(format!(
        "watching {} and running `{}`",
        source.root.display(),
        display_os_command(&command)
    ));
    let mut runs = 0_u32;
    if !args.no_initial {
        let code = execute(context, &args, &command)?;
        runs += 1;
        if should_stop(&args, runs, code) {
            return Ok(code);
        }
    }

    let debounce = Duration::from_millis(args.debounce_ms);
    loop {
        let event = receive_event(&receiver)?;
        register_created_directories(&mut watcher, &source.root, &event)?;
        if !relevant_event(&source.root, &event) {
            continue;
        }

        let mut deadline = Instant::now() + debounce;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(Ok(event)) => {
                    register_created_directories(&mut watcher, &source.root, &event)?;
                    if relevant_event(&source.root, &event) {
                        deadline = Instant::now() + debounce;
                    }
                }
                Ok(Err(error)) => return Err(error).context("filesystem watch failed"),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("filesystem watcher stopped unexpectedly")
                }
            }
        }

        let code = execute(context, &args, &command)?;
        runs += 1;
        if should_stop(&args, runs, code) {
            return Ok(code);
        }
    }
}

fn execute(context: &AppContext, args: &WatchArgs, command: &[OsString]) -> Result<i32> {
    let source = context.project()?;
    let project = prepared_project(
        context,
        &source,
        args.execution.lean.as_deref(),
        args.execution.no_sync,
        args.execution.offline,
    )?;
    context.info(format!("watch run: {}", display_os_command(command)));
    let code = run_in_project(context, &project, command, !args.execution.offline)?;
    if code != 0 {
        context.info(format!(
            "watch command exited with code {code}; waiting for changes"
        ));
    }
    Ok(code)
}

fn should_stop(args: &WatchArgs, runs: u32, code: i32) -> bool {
    (args.exit_on_failure && code != 0) || args.max_runs.is_some_and(|limit| runs >= limit)
}

fn receive_event(receiver: &mpsc::Receiver<notify::Result<Event>>) -> Result<Event> {
    receiver
        .recv()
        .context("filesystem watcher stopped unexpectedly")
        .and_then(|event| event.context("filesystem watch failed"))
}

fn watch_tree(watcher: &mut RecommendedWatcher, root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    collect_directories(root, root, &mut directories)?;
    for directory in directories {
        watcher
            .watch(&directory, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch {}", directory.display()))?;
    }
    Ok(())
}

fn collect_directories(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    if output.len() >= MAX_WATCH_DIRECTORIES {
        bail!("project contains more than {MAX_WATCH_DIRECTORIES} watch directories");
    }
    output.push(directory.to_owned());
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .context("watch traversal escaped the project root")?;
        if !excluded(relative) {
            collect_directories(root, &path, output)?;
        }
    }
    Ok(())
}

fn register_created_directories(
    watcher: &mut RecommendedWatcher,
    root: &Path,
    event: &Event,
) -> Result<()> {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    ) {
        return Ok(());
    }
    for path in &event.paths {
        if path.is_dir() && path.starts_with(root) {
            let relative = path
                .strip_prefix(root)
                .context("new watch directory escaped the project root")?;
            if !excluded(relative) {
                watch_tree(watcher, path)?;
            }
        }
    }
    Ok(())
}

fn relevant_event(root: &Path, event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.iter().any(|path| {
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        !relative.as_os_str().is_empty()
            && !excluded(relative)
            && !matches!(
                relative.file_name().and_then(|name| name.to_str()),
                Some("lev.lock" | "lake-manifest.json")
            )
            && !relative.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.ends_with(".lev.tar.zst") || name.contains(".lev-tmp-")
            })
    })
}

fn excluded(relative: &Path) -> bool {
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
