#![cfg(unix)]

//! Process-level tests for lev's public command contract.
//!
//! These cases intentionally share one integration-test crate: the fake
//! Lean/Lake/elan fixture and local HTTP server are substantial, while each
//! test still receives an independent temporary root. Keeping one binary
//! avoids recompiling and relinking the same harness for every command family.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const TEST_TOOLCHAIN: &str = "leanprover/lean4:v4.test";
const TEST_TOOLCHAIN_A: &str = "leanprover/lean4:v4.fixture-a";
const TEST_TOOLCHAIN_B: &str = "leanprover/lean4:v4.fixture-b";
const TEST_TOOLCHAIN_C: &str = "leanprover/lean4:v4.fixture-c";
const TEST_TOOLCHAIN_D: &str = "leanprover/lean4:v4.fixture-d";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    remote: PathBuf,
    project: PathBuf,
    cache: PathBuf,
    data: PathBuf,
    fake_elan: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let remote = root.join("remote");
        let project = root.join("project");
        let cache = root.join("cache");
        let data = root.join("data");
        let fake_elan = root.join("fake-elan");
        let toolchain_root = root.join("official-toolchain");
        let elan_state = root.join("elan-state");
        let log = root.join("elan.log");

        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&elan_state).unwrap();
        fs::create_dir_all(toolchain_root.join("bin")).unwrap();
        fs::create_dir_all(toolchain_root.join("lib")).unwrap();
        fs::write(toolchain_root.join("bin/lean"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(toolchain_root.join("lib/shared"), "shared bytes").unwrap();
        fs::write(toolchain_root.join("lib/shared-copy"), "shared bytes").unwrap();
        let mut lean_permissions = fs::metadata(toolchain_root.join("bin/lean"))
            .unwrap()
            .permissions();
        lean_permissions.set_mode(0o755);
        fs::set_permissions(toolchain_root.join("bin/lean"), lean_permissions).unwrap();
        git(&remote, &["init", "--initial-branch=main"]);
        git(&remote, &["config", "user.email", "lev@example.invalid"]);
        git(&remote, &["config", "user.name", "lev test"]);

        fs::write(&fake_elan, include_str!("fixtures/fake-elan.sh")).unwrap();
        let mut permissions = fs::metadata(&fake_elan).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_elan, permissions).unwrap();

        fs::write(
            project.join("lean-toolchain"),
            format!("{TEST_TOOLCHAIN}\n"),
        )
        .unwrap();
        fs::write(
            elan_state.join("installed"),
            [
                TEST_TOOLCHAIN,
                TEST_TOOLCHAIN_A,
                TEST_TOOLCHAIN_B,
                TEST_TOOLCHAIN_C,
                TEST_TOOLCHAIN_D,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        fs::write(
            project.join("lakefile.toml"),
            "[package]\nname = \"root\"\n",
        )
        .unwrap();

        Self {
            _temp: temp,
            root,
            remote,
            project,
            cache,
            data,
            fake_elan,
            log,
        }
    }

    fn commit(&self, contents: &str, message: &str) -> String {
        fs::write(self.remote.join("Dep.lean"), contents).unwrap();
        git(&self.remote, &["add", "Dep.lean"]);
        git(&self.remote, &["commit", "-m", message]);
        git_output(&self.remote, &["rev-parse", "HEAD"])
    }

    fn write_manifest(&self, revision: &str) {
        let manifest = json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "url": self.remote.to_string_lossy(),
                "type": "git",
                "rev": revision,
                "name": "dep"
            }],
            "name": "root"
        });
        fs::write(
            self.project.join("lake-manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn registry_manifest(&self, revision: &str, input_revision: &str) -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "url": self.remote.to_string_lossy(),
                "type": "git",
                "rev": revision,
                "inputRev": input_revision,
                "scope": "test-owner",
                "name": "dep"
            }],
            "name": "root"
        }))
        .unwrap()
    }

    fn lev(&self) -> Command {
        self.lev_at(&self.project)
    }

    fn lev_at(&self, project: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lev"));
        command
            .arg("--project")
            .arg(project)
            .env("LEV_ELAN", &self.fake_elan)
            .env("LEV_CACHE_DIR", &self.cache)
            .env("LEV_DATA_DIR", &self.data)
            .env("LEV_TEST_ELAN_STATE", self.root.join("elan-state"))
            .env(
                "LEV_TEST_TOOLCHAIN_ROOT",
                self.root.join("official-toolchain"),
            )
            .env("LEV_TEST_LOG", &self.log);
        command
    }
}

/// A direct indexed package plus one transitive package whose release metadata
/// and committed Lake manifest describe the same immutable graph.
///
/// Native-resolution tests use this fixture to distinguish a graph lev can
/// prove completely from nearby cases that must be delegated to Lake.
struct CompleteRegistryGraph {
    old_revision: String,
    new_revision: String,
    child_remote: PathBuf,
    child_revision: String,
    second_remote: PathBuf,
    second_old_revision: String,
    second_new_revision: String,
    second_child_input_revision: String,
    target_toolchain: &'static str,
}

fn complete_registry_graph(fixture: &Fixture) -> CompleteRegistryGraph {
    let old_revision = fixture.commit("def answer := 1\n", "old");
    let target_toolchain = "leanprover/lean4:v4.fixture-d";

    let child_remote = fixture.root.join("child-remote");
    fs::create_dir_all(&child_remote).unwrap();
    git(&child_remote, &["init", "--initial-branch=main"]);
    git(
        &child_remote,
        &["config", "user.email", "lev@example.invalid"],
    );
    git(&child_remote, &["config", "user.name", "lev test"]);
    fs::write(child_remote.join("Child.lean"), "def child := 3\n").unwrap();
    fs::write(
        child_remote.join("lean-toolchain"),
        format!("{target_toolchain}\n"),
    )
    .unwrap();
    fs::write(
        child_remote.join("lakefile.toml"),
        "[package]\nname = \"child\"\n",
    )
    .unwrap();
    fs::write(
        child_remote.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [],
            "name": "child"
        }))
        .unwrap(),
    )
    .unwrap();
    git(&child_remote, &["add", "."]);
    git(&child_remote, &["commit", "-m", "child"]);
    let child_revision = git_output(&child_remote, &["rev-parse", "HEAD"]);

    fs::write(fixture.remote.join("Dep.lean"), "def answer := 2\n").unwrap();
    fs::write(
        fixture.remote.join("lean-toolchain"),
        format!("{target_toolchain}\n"),
    )
    .unwrap();
    fs::write(
        fixture.remote.join("lakefile.toml"),
        "[package]\nname = \"dep\"\n",
    )
    .unwrap();
    fs::write(
        fixture.remote.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "url": child_remote.to_string_lossy(),
                "type": "git",
                "subDir": null,
                "scope": "test-owner",
                "rev": child_revision,
                "name": "child",
                "manifestFile": "lake-manifest.json",
                "inputRev": "main",
                "inherited": false,
                "configFile": "lakefile.toml"
            }],
            "name": "dep"
        }))
        .unwrap(),
    )
    .unwrap();
    git(&fixture.remote, &["add", "."]);
    git(&fixture.remote, &["commit", "-m", "new release"]);
    let new_revision = git_output(&fixture.remote, &["rev-parse", "HEAD"]);
    git(&fixture.remote, &["tag", "v2.0.0"]);

    let second_remote = fixture.root.join("second-root-remote");
    fs::create_dir_all(&second_remote).unwrap();
    git(&second_remote, &["init", "--initial-branch=main"]);
    git(
        &second_remote,
        &["config", "user.email", "lev@example.invalid"],
    );
    git(&second_remote, &["config", "user.name", "lev test"]);
    fs::write(second_remote.join("Second.lean"), "def second := 1\n").unwrap();
    fs::write(
        second_remote.join("lean-toolchain"),
        "leanprover/lean4:v4.test\n",
    )
    .unwrap();
    fs::write(
        second_remote.join("lakefile.toml"),
        "[package]\nname = \"second-root\"\n",
    )
    .unwrap();
    fs::write(
        second_remote.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [],
            "name": "second-root"
        }))
        .unwrap(),
    )
    .unwrap();
    git(&second_remote, &["add", "."]);
    git(&second_remote, &["commit", "-m", "old second release"]);
    let second_old_revision = git_output(&second_remote, &["rev-parse", "HEAD"]);
    git(&second_remote, &["tag", "v1.0.0"]);

    fs::write(second_remote.join("Second.lean"), "def second := 2\n").unwrap();
    fs::write(
        second_remote.join("lean-toolchain"),
        format!("{target_toolchain}\n"),
    )
    .unwrap();
    fs::write(
        second_remote.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "url": child_remote.to_string_lossy(),
                "type": "git",
                "subDir": null,
                "scope": "test-owner",
                "rev": child_revision,
                "name": "child",
                "manifestFile": "lake-manifest.json",
                "inputRev": "main",
                "inherited": false,
                "configFile": "lakefile.toml"
            }],
            "name": "second-root"
        }))
        .unwrap(),
    )
    .unwrap();
    git(&second_remote, &["add", "."]);
    git(&second_remote, &["commit", "-m", "new second release"]);
    let second_new_revision = git_output(&second_remote, &["rev-parse", "HEAD"]);
    git(&second_remote, &["tag", "v2.0.0"]);

    CompleteRegistryGraph {
        old_revision,
        new_revision,
        child_remote,
        child_revision,
        second_remote,
        second_old_revision,
        second_new_revision,
        second_child_input_revision: "main".to_owned(),
        target_toolchain,
    }
}

fn complete_registry_server(graph: &CompleteRegistryGraph) -> (String, thread::JoinHandle<()>) {
    reservoir_server_with_graph(
        &graph.old_revision,
        &graph.new_revision,
        graph.target_toolchain,
        &graph.child_remote,
        &graph.child_revision,
    )
}

fn complete_multi_registry_server(
    graph: &CompleteRegistryGraph,
) -> (String, thread::JoinHandle<()>) {
    let (listener, api) = local_http_listener();
    let dep_body = registry_graph_response(
        &graph.old_revision,
        &graph.new_revision,
        graph.target_toolchain,
        &graph.child_remote,
        &graph.child_revision,
        "main",
    );
    let second_body = registry_graph_response(
        &graph.second_old_revision,
        &graph.second_new_revision,
        graph.target_toolchain,
        &graph.child_remote,
        &graph.child_revision,
        &graph.second_child_input_revision,
    );
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut served = 0;
        while served < 2 {
            let (mut stream, request) = match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut request = [0_u8; 4096];
                    let bytes = stream.read(&mut request).unwrap();
                    (
                        stream,
                        String::from_utf8_lossy(&request[..bytes]).into_owned(),
                    )
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for Reservoir request"
                    );
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("failed to accept Reservoir request: {error}"),
            };
            let body = if request.contains("/packages/test-owner/dep/versions") {
                &dep_body
            } else if request.contains("/packages/test-owner/second-root/versions") {
                &second_body
            } else {
                panic!("unexpected Reservoir request: {request}");
            };
            write_http_response(&mut stream, "application/json", body);
            served += 1;
        }
    });
    (api, server)
}

fn registry_graph_response(
    old_revision: &str,
    new_revision: &str,
    toolchain: &str,
    child_remote: &Path,
    child_revision: &str,
    child_input_revision: &str,
) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": "1.2.0",
        "data": [
            {
                "version": "2.0.0",
                "revision": new_revision,
                "date": "2026-02-01T00:00:00Z",
                "tag": "v2.0.0",
                "toolchain": toolchain,
                "dependencies": [{
                    "type": "git",
                    "name": "child",
                    "scope": "test-owner",
                    "rev": child_revision,
                    "inputRev": child_input_revision,
                    "url": child_remote.to_string_lossy(),
                    "transitive": false
                }]
            },
            {
                "version": "1.0.0",
                "revision": old_revision,
                "date": "2026-01-01T00:00:00Z",
                "tag": "v1.0.0",
                "toolchain": toolchain,
                "dependencies": []
            }
        ]
    }))
    .unwrap()
}

fn registry_manifest_with_extra_root(
    fixture: &Fixture,
    graph: &CompleteRegistryGraph,
    name: &str,
) -> Vec<u8> {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fixture.registry_manifest(&graph.old_revision, "v1.0.0")).unwrap();
    manifest["packages"].as_array_mut().unwrap().push(json!({
        "url": fixture.remote.to_string_lossy(),
        "type": "git",
        "rev": graph.old_revision,
        "inputRev": graph.old_revision,
        "scope": "test-owner",
        "name": name
    }));
    serde_json::to_vec_pretty(&manifest).unwrap()
}

fn multi_registry_manifest(fixture: &Fixture, graph: &CompleteRegistryGraph) -> Vec<u8> {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fixture.registry_manifest(&graph.old_revision, "v1.0.0")).unwrap();
    manifest["packages"].as_array_mut().unwrap().push(json!({
        "url": graph.second_remote.to_string_lossy(),
        "type": "git",
        "rev": graph.second_old_revision,
        "inputRev": "v1.0.0",
        "scope": "test-owner",
        "name": "second-root"
    }));
    serde_json::to_vec_pretty(&manifest).unwrap()
}

fn make_second_root_manifest_conflict(graph: &mut CompleteRegistryGraph) {
    let path = graph.second_remote.join("lake-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["packages"][0]["inputRev"] = json!("different-branch");
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    git(&graph.second_remote, &["add", "lake-manifest.json"]);
    git(
        &graph.second_remote,
        &["commit", "-m", "conflicting child selector"],
    );
    graph.second_new_revision = git_output(&graph.second_remote, &["rev-parse", "HEAD"]);
    graph.second_child_input_revision = "different-branch".to_owned();
    git(
        &graph.second_remote,
        &["tag", "--force", "v2.0.0", &graph.second_new_revision],
    );
}

fn move_registry_root_into_subdirectory(fixture: &Fixture, graph: &mut CompleteRegistryGraph) {
    let package_root = fixture.remote.join("packages/dep");
    fs::create_dir_all(&package_root).unwrap();
    for file in [
        "Dep.lean",
        "lean-toolchain",
        "lakefile.toml",
        "lake-manifest.json",
    ] {
        fs::rename(fixture.remote.join(file), package_root.join(file)).unwrap();
    }
    git(&fixture.remote, &["add", "--all"]);
    git(
        &fixture.remote,
        &["commit", "-m", "move package into monorepo"],
    );
    graph.old_revision = git_output(&fixture.remote, &["rev-parse", "HEAD"]);
    git(
        &fixture.remote,
        &["tag", "--force", "v1.0.0", &graph.old_revision],
    );

    fs::write(package_root.join("Dep.lean"), "def answer := 3\n").unwrap();
    git(&fixture.remote, &["add", "packages/dep/Dep.lean"]);
    git(
        &fixture.remote,
        &["commit", "-m", "publish monorepo package release"],
    );
    graph.new_revision = git_output(&fixture.remote, &["rev-parse", "HEAD"]);
    git(
        &fixture.remote,
        &["tag", "--force", "v2.0.0", &graph.new_revision],
    );
}

fn write_multi_registry_project(fixture: &Fixture, graph: &CompleteRegistryGraph) -> Vec<u8> {
    let manifest = multi_registry_manifest(fixture, graph);
    fs::write(fixture.project.join("lake-manifest.json"), &manifest).unwrap();
    fs::write(
        fixture.project.join("lakefile.toml"),
        r#"[package]
name = "root"

[[require]]
name = "dep"
scope = "test-owner"
rev = "v1.0.0"

[[require]]
name = "second-root"
scope = "test-owner"
rev = "v1.0.0"
"#,
    )
    .unwrap();
    manifest
}

fn write_mixed_registry_project(
    fixture: &Fixture,
    graph: &CompleteRegistryGraph,
    selector: &str,
) -> Vec<u8> {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fixture.registry_manifest(&graph.old_revision, "v1.0.0")).unwrap();
    manifest["packages"].as_array_mut().unwrap().push(json!({
        "url": graph.second_remote.to_string_lossy(),
        "type": "git",
        "rev": graph.second_new_revision,
        "inputRev": selector,
        "name": "second-root"
    }));
    let manifest = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::write(fixture.project.join("lake-manifest.json"), &manifest).unwrap();
    fs::write(
        fixture.project.join("lakefile.toml"),
        format!(
            r#"[package]
name = "root"

[[require]]
name = "dep"
scope = "test-owner"
rev = "v1.0.0"

[[require]]
name = "second-root"
git = "{}"
rev = "{}"
"#,
            graph.second_remote.display(),
            selector
        ),
    )
    .unwrap();
    manifest
}

/// Assert that an almost-eligible graph reaches Lake's general resolver.
///
/// The fake Lake process fails deliberately. Success would therefore mean lev
/// incorrectly accepted the native fast path, while the logged update proves
/// the graph was delegated instead of rejected before resolution.
fn assert_versioned_resolution_delegates_to_lake(fixture: &Fixture, graph: &CompleteRegistryGraph) {
    let (api, server) = complete_registry_server(graph);
    let output = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .env("LEV_TEST_LAKE_UPDATE_FAIL", "1")
        .arg("--verbose")
        .arg("lock")
        .arg("--lean")
        .arg("4.fixture-d")
        .output()
        .unwrap();
    server.join().unwrap();
    assert_failed_resolution_reached_lake(fixture, &output);
}

fn assert_failed_resolution_reached_lake(fixture: &Fixture, output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(
        !output.status.success(),
        "stderr:\n{stderr}\nprocess log:\n{log}"
    );
    assert!(stderr.contains("simulated Lake update failure"), "{stderr}");
    assert!(
        !stderr.contains("resolved exact dependency graph without a Lake update"),
        "{stderr}"
    );
    assert!(log.contains("lake-update"), "{log}");
}

fn write_empty_project(root: &Path, name: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.test\n").unwrap();
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

/// Parse one versioned CLI envelope and return its command-specific payload.
///
/// Keeping this assertion in one helper makes every JSON integration test
/// verify the compatibility boundary, not merely fields inside the report.
fn json_data(output: &Output, expected_schema: &str) -> serde_json::Value {
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON output: {error}\nstdout:\n{}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(envelope["schema"], expected_schema);
    envelope
        .get("data")
        .unwrap_or_else(|| panic!("JSON envelope has no data field: {envelope}"))
        .clone()
}

#[path = "cli/automation.rs"]
mod automation;
#[path = "cli/cache.rs"]
mod cache;
#[path = "cli/core.rs"]
mod core;
#[path = "cli/dependencies.rs"]
mod dependencies;
#[path = "cli/environments.rs"]
mod environments;
#[path = "cli/toolchains.rs"]
mod toolchains;

fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap();
    assert_success(output);
}

fn tagged_log_paths(log: &Path, tag: &str) -> Vec<PathBuf> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (candidate, rest) = line.split_once('\t')?;
            (candidate == tag).then(|| PathBuf::from(rest.split('\t').next().unwrap()))
        })
        .collect()
}

fn workspace_projects(cache: &Path) -> Vec<PathBuf> {
    let root = cache.join("workspaces-v1");
    let mut projects = Vec::new();
    if !root.is_dir() {
        return projects;
    }
    for source in fs::read_dir(root).unwrap() {
        let source = source.unwrap();
        if !source.file_type().unwrap().is_dir() {
            continue;
        }
        for toolchain in fs::read_dir(source.path()).unwrap() {
            let project = toolchain.unwrap().path().join("project");
            if project.is_dir() {
                projects.push(project);
            }
        }
    }
    projects.sort();
    projects
}

fn lake_cache_path(cache: &Path, toolchain: &str) -> PathBuf {
    let hash = format!("{:x}", Sha256::digest(toolchain.as_bytes()));
    cache.join("lake-v1").join(&hash[..16])
}

fn git_output(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap();
    assert_success_ref(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn compressed_toolchain_archive() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        append_tar_directory(&mut builder, "lean-4.fixture-release-linux/");
        append_tar_directory(&mut builder, "lean-4.fixture-release-linux/bin/");
        append_tar_file(
            &mut builder,
            "lean-4.fixture-release-linux/bin/lean",
            b"#!/bin/sh\nexit 0\n",
            0o755,
        );
        append_tar_file(
            &mut builder,
            "lean-4.fixture-release-linux/bin/lake",
            b"#!/bin/sh\nexit 0\n",
            0o755,
        );
        builder.finish().unwrap();
    }
    zstd::stream::encode_all(Cursor::new(bytes), 1).unwrap()
}

fn append_tar_directory(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::dir());
    header.set_mode(0o755);
    header.set_size(0);
    header.set_cksum();
    builder
        .append_data(&mut header, path, Cursor::new([]))
        .unwrap();
}

fn append_tar_file(
    builder: &mut tar::Builder<&mut Vec<u8>>,
    path: &str,
    contents: &[u8],
    mode: u32,
) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::file());
    header.set_mode(mode);
    header.set_size(contents.len() as u64);
    header.set_cksum();
    builder
        .append_data(&mut header, path, Cursor::new(contents))
        .unwrap();
}

fn release_server(archive: Vec<u8>, sha256: &str) -> (String, thread::JoinHandle<()>) {
    let (listener, api) = local_http_listener();
    let metadata = serde_json::to_vec(&json!({
        "tag_name": "v4.fixture-release",
        "assets": [{
            "name": format!("lean-4.fixture-release-{}.tar.zst", test_release_platform()),
            "browser_download_url": format!("{api}/asset"),
            "size": archive.len(),
            "digest": format!("sha256:{sha256}")
        }]
    }))
    .unwrap();
    let server = thread::spawn(move || {
        for (content_type, body) in [
            ("application/json", metadata),
            ("application/octet-stream", archive),
        ] {
            let (mut stream, _) = accept_http_request(&listener);
            write_http_response(&mut stream, content_type, &body);
        }
    });
    (api, server)
}

fn self_update_server(tag: &str) -> (String, thread::JoinHandle<()>) {
    let (listener, api) = local_http_listener();
    let body = serde_json::to_vec(&json!({
        "tag_name": tag,
        "assets": []
    }))
    .unwrap();
    let server = thread::spawn(move || {
        let (mut stream, request) = accept_http_request(&listener);
        assert!(
            request.contains("/repos/example/lev/releases/latest"),
            "{request}"
        );
        write_http_response(&mut stream, "application/json", &body);
    });
    (api, server)
}

fn reservoir_server(old_revision: &str, new_revision: &str) -> (String, thread::JoinHandle<()>) {
    reservoir_server_for_toolchain(old_revision, new_revision, "leanprover/lean4:v4.test")
}

fn reservoir_server_for_toolchain(
    old_revision: &str,
    new_revision: &str,
    toolchain: &str,
) -> (String, thread::JoinHandle<()>) {
    let (listener, api) = local_http_listener();
    let body = reservoir_versions_response(old_revision, new_revision, toolchain);
    let server = thread::spawn(move || {
        let (mut stream, request) = accept_http_request(&listener);
        assert!(
            request.contains("/packages/test-owner/dep/versions"),
            "{request}"
        );
        write_http_response(&mut stream, "application/json", &body);
    });
    (api, server)
}

fn authenticated_reservoir_server(
    old_revision: &str,
    new_revision: &str,
    expected_token: &str,
) -> (String, thread::JoinHandle<()>) {
    let (listener, api) = local_http_listener();
    let expected_authorization = format!("authorization: bearer {expected_token}");
    let body = reservoir_versions_response(old_revision, new_revision, "leanprover/lean4:v4.test");
    let server = thread::spawn(move || {
        let (mut stream, request) = accept_http_request(&listener);
        let request = request.to_ascii_lowercase();
        assert!(
            request.contains("/packages/test-owner/dep/versions"),
            "{request}"
        );
        assert!(request.contains(&expected_authorization), "{request}");
        write_http_response(&mut stream, "application/json", &body);
    });
    (api, server)
}

/// Serve one indexed release whose Reservoir record contains a complete,
/// immutable dependency snapshot. Unlike the older compatibility fixtures,
/// this response deliberately includes the `dependencies` field so lev's
/// pre-resolution fetch path is exercised.
fn reservoir_server_with_graph(
    old_revision: &str,
    new_revision: &str,
    toolchain: &str,
    child_remote: &Path,
    child_revision: &str,
) -> (String, thread::JoinHandle<()>) {
    let (listener, api) = local_http_listener();
    let body = registry_graph_response(
        old_revision,
        new_revision,
        toolchain,
        child_remote,
        child_revision,
        "main",
    );
    let server = thread::spawn(move || {
        let (mut stream, request) = accept_http_request(&listener);
        assert!(
            request.contains("/packages/test-owner/dep/versions"),
            "{request}"
        );
        write_http_response(&mut stream, "application/json", &body);
    });
    (api, server)
}

fn reservoir_versions_response(old_revision: &str, new_revision: &str, toolchain: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": "1.2.0",
        "data": [
            {
                "version": "2.0.0",
                "revision": new_revision,
                "date": "2026-02-01T00:00:00Z",
                "tag": "v2.0.0",
                "toolchain": toolchain
            },
            {
                "version": "1.0.0",
                "revision": old_revision,
                "date": "2026-01-01T00:00:00Z",
                "tag": "v1.0.0",
                "toolchain": toolchain
            }
        ]
    }))
    .unwrap()
}

fn local_http_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    (listener, api)
}

fn accept_http_request(listener: &TcpListener) -> (std::net::TcpStream, String) {
    let (mut stream, _) = listener.accept().unwrap();
    let mut request = [0_u8; 4096];
    let bytes = stream.read(&mut request).unwrap();
    (
        stream,
        String::from_utf8_lossy(&request[..bytes]).into_owned(),
    )
}

fn write_http_response(stream: &mut std::net::TcpStream, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn test_release_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux",
        ("linux", "aarch64") => "linux_aarch64",
        ("linux", "x86") => "linux_x86",
        ("macos", "x86_64") => "darwin",
        ("macos", "aarch64") => "darwin_aarch64",
        platform => panic!("unsupported process-test platform: {platform:?}"),
    }
}

#[derive(Clone)]
struct RecordedRequest {
    method: String,
    authorization: Option<String>,
}

/// Minimal create-only HTTP object store used to exercise ureq through the
/// compiled CLI. It intentionally implements only the protocol lev requires:
/// GET, PUT, `If-None-Match: *`, fixed-length bodies, and bearer auth.
struct TestObjectServer {
    base: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestObjectServer {
    fn start(expected_token: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = mpsc::channel();
        let objects = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_objects = Arc::clone(&objects);
        let server_requests = Arc::clone(&requests);
        let thread = thread::spawn(move || {
            loop {
                if stopped.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        serve_object_request(
                            &mut stream,
                            expected_token,
                            &server_objects,
                            &server_requests,
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("object server accept failed: {error}"),
                }
            }
        });
        Self {
            base: format!("http://{address}/objects"),
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for TestObjectServer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("object server thread panicked");
            }
        }
    }
}

fn serve_object_request(
    stream: &mut std::net::TcpStream,
    expected_token: &str,
    objects: &Mutex<BTreeMap<String, Vec<u8>>>,
    requests: &Mutex<Vec<RecordedRequest>>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "connection closed before HTTP headers");
        request.extend_from_slice(&chunk[..read]);
        assert!(request.len() <= 64 * 1024, "HTTP test header too large");
        if let Some(offset) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().unwrap();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap().to_owned();
    let path = request_parts.next().unwrap().to_owned();
    let mut content_length = 0_usize;
    let mut authorization = None;
    let mut create_only = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').unwrap();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap(),
            "authorization" => authorization = Some(value.trim().to_owned()),
            "if-none-match" => create_only = value.trim() == "*",
            _ => {}
        }
    }
    let expected_authorization = format!("Bearer {expected_token}");
    assert_eq!(
        authorization.as_deref(),
        Some(expected_authorization.as_str())
    );
    while request.len() - header_end < content_length {
        let mut chunk = [0_u8; 8192];
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "connection closed before HTTP body");
        request.extend_from_slice(&chunk[..read]);
    }
    let body = request[header_end..header_end + content_length].to_vec();
    requests.lock().unwrap().push(RecordedRequest {
        method: method.clone(),
        authorization,
    });

    let (status, response_body) = match method.as_str() {
        "GET" => match objects.lock().unwrap().get(&path).cloned() {
            Some(body) => ("200 OK", body),
            None => ("404 Not Found", Vec::new()),
        },
        "PUT" => {
            let mut objects = objects.lock().unwrap();
            if create_only && objects.contains_key(&path) {
                ("412 Precondition Failed", Vec::new())
            } else {
                objects.insert(path, body);
                ("201 Created", Vec::new())
            }
        }
        _ => ("405 Method Not Allowed", Vec::new()),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
    )
    .unwrap();
    stream.write_all(&response_body).unwrap();
}

fn assert_success(output: Output) {
    assert_success_ref(&output);
}

fn assert_success_ref(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
