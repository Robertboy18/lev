//! Small end-to-end checks that run on every supported host.
//!
//! The larger CLI fixture pretends to be elan and Lake with a shell script, so
//! it is Unix-only. These tests launch the real lev binary and cover the
//! project-independent surface on Windows too.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn lev() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lev"))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn help_and_version_launch_without_a_project() {
    let help = lev().arg("--help").output().unwrap();
    assert_success(&help);
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("Explain Lake rebuilds"),
        "{}",
        String::from_utf8_lossy(&help.stdout)
    );

    let version = lev().arg("--version").output().unwrap();
    assert_success(&version);
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("lev {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn empty_cache_commands_use_paths_with_spaces_and_create_nothing() {
    let temporary = TempDir::new().unwrap();
    let cache = temporary.path().join("cache with spaces");

    let status = isolated_lev(&cache)
        .args(["cache", "status"])
        .output()
        .unwrap();
    assert_success(&status);
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.contains(&format!("lev cache   {}", cache.display())),
        "{status}"
    );
    assert!(status.contains("0 registered"), "{status}");

    let gc = isolated_lev(&cache).args(["cache", "gc"]).output().unwrap();
    assert_success(&gc);
    assert!(
        String::from_utf8_lossy(&gc.stdout).contains("nothing to collect"),
        "{}",
        String::from_utf8_lossy(&gc.stdout)
    );
    assert!(
        !cache.exists(),
        "a cache inspection with nothing registered should not create state"
    );
}

fn isolated_lev(cache: &Path) -> Command {
    let mut command = lev();
    command.arg("--cache-dir").arg(cache);
    command
}
