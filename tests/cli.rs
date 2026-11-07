#![cfg(unix)]

//! Process-level tests for lev's commands.
//!
//! A shell script stands in for elan and Lake, so these tests are Unix-only.
//! Each test gets its own temporary project, cache, and fake toolchain; the
//! scenarios share one test binary to avoid relinking the driver per family.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

const TEST_TOOLCHAIN: &str = "leanprover/lean4:v4.test";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    project: PathBuf,
    cache: PathBuf,
    toolchain: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        let toolchain = root.join("toolchain");
        let elan_state = root.join("elan-state");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(toolchain.join("bin")).unwrap();
        fs::create_dir_all(&elan_state).unwrap();
        executable(&toolchain.join("bin/lean"), "#!/bin/sh\nexit 0\n");
        executable(
            &root.join("fake-elan"),
            include_str!("fixtures/fake-elan.sh"),
        );
        fs::write(elan_state.join("installed"), format!("{TEST_TOOLCHAIN}\n")).unwrap();
        write_project(&project, "root");

        Self {
            _temp: temp,
            cache: root.join("cache"),
            log: root.join("elan.log"),
            root,
            project,
            toolchain,
        }
    }

    fn lev(&self) -> Command {
        self.lev_at(&self.project)
    }

    fn lev_at(&self, project: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lev"));
        command
            .arg("--project")
            .arg(project)
            .env("LEV_ELAN", self.root.join("fake-elan"))
            .env("LEV_CACHE_DIR", &self.cache)
            .env("LEV_TEST_ELAN_STATE", self.root.join("elan-state"))
            .env("LEV_TEST_TOOLCHAIN_ROOT", &self.toolchain)
            .env("LEV_TEST_LOG", &self.log)
            .env_remove("LAKE_CACHE_DIR");
        command
    }

    /// Where Lake keeps the artifact cache of the fake toolchain.
    fn lake_cache(&self) -> PathBuf {
        self.toolchain.join("lake/cache")
    }

    /// Write `text` to a file under the fixture root and return its path.
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_project(root: &Path, name: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("lean-toolchain"), format!("{TEST_TOOLCHAIN}\n")).unwrap();
    fs::write(
        root.join("lakefile.toml"),
        format!("[package]\nname = \"{name}\"\n"),
    )
    .unwrap();
    fs::write(
        root.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [],
            "name": name
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Write one trace file below `root`.
fn write_trace(root: &Path, relative: &str, trace: &Value) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(trace).unwrap()).unwrap();
}

/// A module trace with a source hash, some imports, and a public olean.
fn module_trace(
    dep_hash: &str,
    source: (&Path, &str),
    imports: &[(&str, &str)],
    olean: &str,
) -> Value {
    let imports = imports
        .iter()
        .flat_map(|(module, hash)| {
            [
                json!([format!("{module}:importArts"), hash]),
                json!([format!("{module} transitive imports (public)"), hash]),
            ]
        })
        .collect::<Vec<_>>();
    json!({
        "schemaVersion": "2025-09-10",
        "depHash": dep_hash,
        "inputs": [
            ["deps", [["imports", imports]]],
            ["Lean 4.x, commit fixture", "0000000000000001"],
            [source.0.to_string_lossy(), source.1],
            ["options", "00000000000006bb"],
            ["isModule: true", "0000000000000002"]
        ],
        "outputs": {"o": [format!("{olean}.olean")], "i": format!("{olean}.ilean"), "m": true}
    })
}

/// A trace as a cache download leaves it: no inputs.
fn cached_trace(dep_hash: &str, olean: &str) -> Value {
    json!({
        "schemaVersion": "2025-09-10",
        "depHash": dep_hash,
        "outputs": {"o": [format!("{olean}.olean")], "m": true}
    })
}

/// Parse one versioned CLI envelope and return its payload.
fn json_data(output: &Output, expected_schema: &str) -> Value {
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON output: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(envelope["schema"], expected_schema);
    envelope
        .get("data")
        .unwrap_or_else(|| panic!("JSON envelope has no data field: {envelope}"))
        .clone()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[path = "cli/cache.rs"]
mod cache;
#[path = "cli/core.rs"]
mod core;
#[path = "cli/doctor.rs"]
mod doctor;
#[path = "cli/inspect.rs"]
mod inspect;
