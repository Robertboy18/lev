//! Small end-to-end checks that run on every supported host.
//!
//! The larger CLI fixture pretends to be Lean, Lake, and elan with a shell
//! script, so it is intentionally Unix-only. These tests launch the real lev
//! binary and cover the project-independent surface on Windows too.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

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
fn help_version_and_powershell_completions_launch_without_a_project() {
    let help = lev().arg("--help").output().unwrap();
    assert_success(&help);
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("Fast Lean toolchains"),
        "{}",
        String::from_utf8_lossy(&help.stdout)
    );

    let version = lev().arg("--version").output().unwrap();
    assert_success(&version);
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("lev {}", env!("CARGO_PKG_VERSION"))
    );

    // Generating PowerShell on Unix as well keeps the exact same path covered
    // before this reaches the native Windows runner.
    let completions = lev().args(["completions", "powershell"]).output().unwrap();
    assert_success(&completions);
    let completions = String::from_utf8_lossy(&completions.stdout);
    assert!(
        completions.contains("Register-ArgumentCompleter"),
        "{completions}"
    );
    assert!(completions.contains("lev"), "{completions}");
}

#[test]
fn empty_cache_commands_use_paths_with_spaces() {
    let temporary = TempDir::new().unwrap();
    let cache = temporary.path().join("cache with spaces");
    let data = temporary.path().join("data with spaces");

    let directory = isolated_lev(&cache, &data)
        .args(["cache", "dir"])
        .output()
        .unwrap();
    assert_success(&directory);
    assert_eq!(
        PathBuf::from(String::from_utf8_lossy(&directory.stdout).trim()),
        cache
    );

    let status = isolated_lev(&cache, &data)
        .args(["cache", "status"])
        .output()
        .unwrap();
    assert_success(&status);
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.contains(&format!("directory: {}", cache.display())),
        "{status}"
    );
    assert!(status.contains("files: 0"), "{status}");
    assert!(status.contains("Git mirrors: 0"), "{status}");

    let gc = isolated_lev(&cache, &data)
        .args(["cache", "gc"])
        .output()
        .unwrap();
    assert_success(&gc);
    assert!(
        String::from_utf8_lossy(&gc.stdout).contains("No unreferenced cache entries"),
        "{}",
        String::from_utf8_lossy(&gc.stdout)
    );
    assert!(
        !cache.exists(),
        "a dry cache inspection should not create state"
    );
}

#[test]
fn self_uninstall_removes_a_copied_binary_and_keeps_user_state() {
    let temporary = TempDir::new().unwrap();
    let installation = temporary.path().join("standalone install with spaces");
    fs::create_dir(&installation).unwrap();
    let executable = installation.join(format!("lev{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_lev"), &executable).unwrap();

    let cache = temporary.path().join("cache");
    let data = temporary.path().join("data");
    fs::create_dir_all(&cache).unwrap();
    fs::create_dir_all(&data).unwrap();
    fs::write(cache.join("keep"), "cache").unwrap();
    fs::write(data.join("keep"), "data").unwrap();

    let preview = Command::new(&executable)
        .args(["--cache-dir"])
        .arg(&cache)
        .args(["--data-dir"])
        .arg(&data)
        .args(["self", "uninstall", "--dry-run"])
        .output()
        .unwrap();
    assert_success(&preview);
    assert!(executable.is_file());
    let preview = String::from_utf8_lossy(&preview.stdout);
    assert!(
        preview.contains("delete this standalone executable"),
        "{preview}"
    );
    assert!(preview.contains("cache retained:"), "{preview}");
    assert!(preview.contains("data retained:"), "{preview}");

    let uninstall = Command::new(&executable)
        .args(["--cache-dir"])
        .arg(&cache)
        .args(["--data-dir"])
        .arg(&data)
        .args(["self", "uninstall"])
        .output()
        .unwrap();
    assert_success(&uninstall);

    let deadline = Instant::now() + Duration::from_secs(10);
    while executable.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !executable.exists(),
        "uninstall left {} behind",
        executable.display()
    );
    assert_eq!(fs::read_to_string(cache.join("keep")).unwrap(), "cache");
    assert_eq!(fs::read_to_string(data.join("keep")).unwrap(), "data");
}

#[cfg(unix)]
#[test]
fn self_uninstall_delegates_a_cargo_install_back_to_cargo() {
    use std::os::unix::fs::PermissionsExt;

    let built_lev = Path::new(env!("CARGO_BIN_EXE_lev"));
    let temporary = tempfile::Builder::new()
        .prefix("lev-cargo-uninstall-")
        .tempdir_in(built_lev.parent().unwrap())
        .unwrap();
    let cargo_home = temporary.path().join("cargo-home");
    let bin = cargo_home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(cargo_home.join(".crates2.json"), "{}").unwrap();

    let executable = bin.join("lev");
    // A hard link behaves like Cargo's installed executable without creating
    // a freshly written program file, which some Linux filesystems can reject
    // with ETXTBSY when it is executed immediately.
    fs::hard_link(built_lev, &executable).unwrap();
    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CARGO_HOME/args\"\nrm -f \"$CARGO_HOME/bin/lev\"\n",
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(&executable)
        .env("CARGO_HOME", &cargo_home)
        .args(["self", "uninstall"])
        .output()
        .unwrap();
    assert_success(&output);
    assert!(!executable.exists());
    assert_eq!(
        fs::read_to_string(cargo_home.join("args")).unwrap(),
        format!(
            "uninstall\n--root\n{}\nlev-cli\n",
            cargo_home.to_string_lossy()
        )
    );
}

fn isolated_lev(cache: &Path, data: &Path) -> Command {
    let mut command = lev();
    command
        .arg("--cache-dir")
        .arg(cache)
        .arg("--data-dir")
        .arg(data);
    command
}
