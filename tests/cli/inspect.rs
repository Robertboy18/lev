use super::*;

#[test]
fn bare_inspect_reports_project_state_in_text_and_json() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");

    let text = fixture.lev().arg("inspect").output().unwrap();
    assert_success_ref(&text);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("Lev 1.0.0 diagnosis"), "{text}");
    assert!(text.contains("Project\n"), "{text}");
    assert!(text.contains("WARN  lock"), "{text}");
    assert!(text.contains("lev inspect build"), "{text}");

    let json = fixture.lev().args(["inspect", "--json"]).output().unwrap();
    assert_success_ref(&json);
    let report = json_data(&json, "lev.cli.inspect/v1");
    assert_eq!(
        PathBuf::from(report["project"]["root"].as_str().unwrap()),
        fixture.project
    );
    assert_eq!(report["project"]["manifest_packages"], 0);
    assert_eq!(report["project"]["runtime_source"], "elan");
    assert_eq!(report["summary"]["errors"], 0);
    assert_eq!(report["summary"]["warnings"], 1);

    let destination = fixture.root.join("reports/inspect.json");
    let written = fixture
        .lev()
        .args(["inspect", "--json", "--output"])
        .arg(&destination)
        .output()
        .unwrap();
    assert_success_ref(&written);
    assert!(written.stdout.is_empty());
    let envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(destination).unwrap()).unwrap();
    assert_eq!(envelope["schema"], "lev.cli.inspect/v1");
}

#[test]
fn bare_inspect_is_useful_outside_a_project() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();

    let output = fixture
        .lev_at(&outside)
        .args(["inspect", "--json"])
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect/v1");
    assert!(report["project"].is_null());
    assert_eq!(report["summary"]["warnings"], 1);
    assert_eq!(report["summary"]["errors"], 0);
    assert_eq!(report["commands"][0], "lev init PATH --lean TOOLCHAIN");
}

#[test]
fn bare_inspect_reports_project_errors_without_becoming_a_policy_gate() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(
        fixture.project.join("lakefile.toml"),
        "[package\nname = \"broken\"\n",
    )
    .unwrap();

    let output = fixture.lev().args(["inspect", "--json"]).output().unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect/v1");
    assert_eq!(report["summary"]["errors"], 1);
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "lakefile" && check["status"] == "error")
    );
}

#[test]
fn inspect_environment_reports_bounded_local_state_and_writes_json() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");

    let help = fixture.lev().args(["inspect", "--help"]).output().unwrap();
    assert_success_ref(&help);
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("environment"),
        "{}",
        String::from_utf8_lossy(&help.stdout)
    );

    let output = fixture
        .lev()
        .args(["inspect", "environment", "--json"])
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect.environment/v1");
    assert_eq!(
        PathBuf::from(report["project"]["root"].as_str().unwrap()),
        fixture.project
    );
    assert_eq!(report["runtime"]["source"], "elan");
    assert_eq!(report["versions"]["elan"], "elan fake");
    assert_eq!(
        report["versions"]["lean"],
        format!("Lean fake ({TEST_TOOLCHAIN})")
    );
    assert_eq!(
        report["versions"]["lake"],
        format!("Lake fake ({TEST_TOOLCHAIN})")
    );
    assert_eq!(report["artifact_cache"]["present"], false);
    assert_eq!(report["registry"]["parsed"], true);
    assert_eq!(report["registry"]["records"], 0);
    assert_eq!(report["workspace"]["configured"], false);
    assert_eq!(report["managed_local_workspace"], false);
    assert_eq!(report["summary"]["policy_failures"], 0);

    let runtime_finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == "environment.runtime.selected")
        .unwrap();
    assert_eq!(runtime_finding["category"], "environment");
    assert_eq!(runtime_finding["state"], "passed");
    assert_eq!(runtime_finding["severity"], "info");
    assert_eq!(runtime_finding["policy"], "observe");
    assert_eq!(runtime_finding["evidence"]["source"], "elan");
    assert!(runtime_finding["title"].is_string());
    assert!(runtime_finding["detail"].is_string());

    let destination = fixture.root.join("reports/environment.json");
    let written = fixture
        .lev()
        .args(["inspect", "environment", "--json", "--output"])
        .arg(&destination)
        .output()
        .unwrap();
    assert_success_ref(&written);
    assert!(written.stdout.is_empty());
    let envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(destination).unwrap()).unwrap();
    assert_eq!(envelope["schema"], "lev.cli.inspect.environment/v1");

    let doctor = fixture.lev().arg("doctor").output().unwrap();
    assert_success_ref(&doctor);
    let doctor = String::from_utf8(doctor.stdout).unwrap();
    assert!(doctor.contains("Lev 1.0.0 environment"), "{doctor}");
    assert!(doctor.contains("Git: git version"), "{doctor}");
    assert!(doctor.contains("Lean: Lean fake"), "{doctor}");
    assert!(doctor.contains("Lake: Lake fake"), "{doctor}");

    let doctor_json = fixture.lev().args(["doctor", "--json"]).output().unwrap();
    assert_success_ref(&doctor_json);
    let doctor_report = json_data(&doctor_json, "lev.cli.inspect.environment/v1");
    assert_eq!(
        PathBuf::from(doctor_report["project"]["root"].as_str().unwrap()),
        fixture.project
    );

    let doctor_destination = fixture.root.join("reports/doctor.json");
    let doctor_written = fixture
        .lev()
        .args(["doctor", "--json", "--output"])
        .arg(&doctor_destination)
        .output()
        .unwrap();
    assert_success_ref(&doctor_written);
    assert!(doctor_written.stdout.is_empty());
    let doctor_envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(doctor_destination).unwrap()).unwrap();
    assert_eq!(doctor_envelope["schema"], "lev.cli.inspect.environment/v1");
}

#[test]
fn inspect_environment_reports_workspace_lock_and_stale_registry_records() {
    let fixture = Fixture::new();
    let workspace = fixture.root.join("monorepo");
    let member = workspace.join("packages/alpha");
    write_empty_project(&member, "alpha");
    fs::write(
        workspace.join("lev.toml"),
        "[workspace]\nmembers = [\"packages/*\"]\n",
    )
    .unwrap();

    let registry = fixture.cache.join("projects-v1");
    fs::create_dir_all(&registry).unwrap();
    fs::write(
        registry.join("stale.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "root": fixture.root.join("deleted-project"),
            "toolchain": TEST_TOOLCHAIN,
            "mirror_keys": [],
            "manifest_digest": null,
            "last_used": 0
        }))
        .unwrap(),
    )
    .unwrap();

    let output = fixture
        .lev_at(&member)
        .args(["inspect", "environment", "--json"])
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect.environment/v1");
    assert_eq!(report["registry"]["records"], 1);
    assert_eq!(report["registry"]["stale_records"], 1);
    assert_eq!(report["workspace"]["configured"], true);
    assert_eq!(report["workspace"]["valid"], true);
    assert_eq!(report["workspace"]["locked"], false);
    assert_eq!(report["workspace"]["members"], 1);
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["id"] == "cache.registry.staleness"
                    && finding["state"] == "raised"
                    && finding["severity"] == "warning"
            })
    );
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["id"] == "workspace.lock"
                    && finding["state"] == "raised"
                    && finding["severity"] == "warning"
            })
    );
}

#[test]
fn inspect_environment_keeps_registry_parse_failures_observational() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    let registry = fixture.cache.join("projects-v1");
    fs::create_dir_all(&registry).unwrap();
    fs::write(registry.join("broken.json"), b"{not json").unwrap();

    let output = fixture
        .lev()
        .args(["inspect", "environment", "--json"])
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect.environment/v1");
    assert_eq!(report["registry"]["parsed"], false);
    assert!(report["registry"].get("records").is_none());
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == "cache.registry.records")
        .unwrap();
    assert_eq!(finding["state"], "raised");
    assert_eq!(finding["severity"], "error");
    assert_eq!(finding["policy"], "observe");
    assert_eq!(report["summary"]["policy_failures"], 0);
}

#[test]
fn inspect_build_reports_lake_events_and_changed_trace_inputs() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(fixture.project.join("Root.lean"), "def root := 2\n").unwrap();
    fs::write(
        fixture.project.join("UsesRoot.lean"),
        "import Root\ndef usesRoot := root\n",
    )
    .unwrap();
    let trace = fixture.project.join(".lake/build/lib/lean/Root.trace");
    fs::create_dir_all(trace.parent().unwrap()).unwrap();
    fs::write(
        &trace,
        serde_json::to_vec(&json!({
            "depHash": "before",
            "inputs": [
                ["Root.lean", "old-source"],
                ["Module.name: Root", "root-module"]
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let replacement = fixture.root.join("Root-after.trace");
    fs::write(
        &replacement,
        serde_json::to_vec(&json!({
            "depHash": "after",
            "schemaVersion": 3,
            "synthetic": false,
            "inputs": [
                ["Root.lean", "new-source"],
                ["Module.name: Root", "root-module"]
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["inspect", "build", "--no-sync", "--offline", "--json"])
        .env("LEV_TEST_LAKE_TRACE_AFTER", &replacement)
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect.build/v1");
    assert_eq!(report["status"], "passed");
    assert_eq!(report["built"], 1);
    assert_eq!(report["replayed"], 1);
    assert_eq!(report["ran"], 3);
    assert_eq!(report["reused"], 0);
    assert_eq!(report["fetched"], 0);
    assert_eq!(report["unpacked"], 0);
    assert_eq!(report["trace_changes"].as_array().unwrap().len(), 1);
    assert_eq!(report["trace_changes"][0]["target"], "Root");
    assert_eq!(report["trace_changes"][0]["inputs"][0]["kind"], "source");
    assert_eq!(report["unattributed_work"].as_array().unwrap().len(), 0);
    assert_eq!(report["causality"]["source_graph_available"], true);
    assert_eq!(
        report["causality"]["changed_project_modules"],
        json!(["Root"])
    );
    assert_eq!(
        report["causality"]["observed_rebuilt_modules"],
        json!(["Root"])
    );
    assert_eq!(
        report["causality"]["potential_affected_modules"],
        json!(["Root", "UsesRoot"])
    );
    assert_eq!(
        report["causality"]["potential_impacts"][1]["shortest_path"],
        json!(["Root", "UsesRoot"])
    );
    assert_eq!(
        report["causality"]["observed_traces"][0]["evidence"],
        "observed_trace"
    );
    assert_eq!(
        report["causality"]["observed_traces"][0]["current_metadata"]["schema_version"],
        "3"
    );
}

#[test]
fn inspect_build_reports_deduplicated_root_causes_and_failed_targets() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(fixture.project.join("Unreadable.lean"), [0xff]).unwrap();

    let output = fixture
        .lev()
        .args(["inspect", "build", "--no-sync", "--offline", "--json"])
        .env("LEV_TEST_LAKE_VERBOSE_FAIL", "1")
        .env(
            "LEV_TEST_LAKE_VERBOSE_ERROR",
            "Root.lean:7:3: error: unknown identifier 'missing'",
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    let report = json_data(&output, "lev.cli.inspect.build/v1");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["root_causes"].as_array().unwrap().len(), 1);
    assert_eq!(report["root_causes"][0]["kind"], "lean_diagnostic");
    assert_eq!(report["root_causes"][0]["file"], "Root.lean");
    assert_eq!(report["root_causes"][0]["line"], 7);
    assert_eq!(report["root_causes"][0]["column"], 3);
    assert_eq!(
        report["root_causes"][0]["message"],
        "unknown identifier 'missing'"
    );
    assert_eq!(report["failed_targets"], json!(["Root"]));
    assert_eq!(report["causality"]["source_graph_available"], false);
    assert!(
        report["causality"]["warnings"][0]
            .as_str()
            .unwrap()
            .contains("could not infer source-level build impact")
    );
    assert!(
        !report["root_causes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cause| cause["message"] == "build failed")
    );
}

#[test]
fn inspect_build_runs_when_pre_build_trace_capture_fails() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(fixture.project.join("Root.lean"), "def root := 1\n").unwrap();
    let build_root = fixture.project.join(".lake/build");
    fs::create_dir_all(&build_root).unwrap();
    let mut permissions = fs::metadata(&build_root).unwrap().permissions();
    permissions.set_mode(0o000);
    fs::set_permissions(&build_root, permissions).unwrap();

    let output = fixture
        .lev()
        .args(["inspect", "build", "--no-sync", "--offline", "--json"])
        .output()
        .unwrap();

    let mut permissions = fs::metadata(&build_root).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&build_root, permissions).unwrap();

    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect.build/v1");
    assert!(report["trace_changes"].as_array().unwrap().is_empty());
    assert!(report["unattributed_work"].as_array().unwrap().is_empty());
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("before the build"))
    );
}

#[test]
fn inspect_build_preserves_lake_failure_when_post_build_trace_capture_fails() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(fixture.project.join("Root.lean"), "def root := 1\n").unwrap();

    let trace = fixture.root.join("Root-after.trace");
    fs::write(
        &trace,
        serde_json::to_vec(&json!({
            "depHash": "after",
            "inputs": [
                ["Root.lean", "source"],
                ["Module.name: Root", "root-module"]
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let script = fs::read_to_string(&fixture.fake_elan).unwrap();
    let build_failure =
        "      printf 'error: build failed\\n' >&2\n      exit \"${LEV_TEST_LAKE_BUILD_EXIT:-37}\"";
    assert_eq!(script.matches(build_failure).count(), 1);
    fs::write(
        &fixture.fake_elan,
        script.replacen(
            build_failure,
            "      printf 'error: build failed\\n' >&2\n      chmod 000 .lake/build\n      exit \"${LEV_TEST_LAKE_BUILD_EXIT:-37}\"",
            1,
        ),
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["inspect", "build", "--no-sync", "--offline", "--json"])
        .env("LEV_TEST_LAKE_TRACE_AFTER", &trace)
        .env("LEV_TEST_LAKE_VERBOSE_FAIL", "1")
        .env("LEV_TEST_LAKE_BUILD_EXIT", "37")
        .output()
        .unwrap();

    let build_root = fixture.project.join(".lake/build");
    let mut permissions = fs::metadata(&build_root).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&build_root, permissions).unwrap();

    assert_eq!(
        output.status.code(),
        Some(37),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_data(&output, "lev.cli.inspect.build/v1");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["exit_code"], 37);
    assert_eq!(report["built"], 1);
    assert!(report["trace_changes"].as_array().unwrap().is_empty());
    assert!(report["unattributed_work"].as_array().unwrap().is_empty());
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("trace-based causality evidence is unavailable"))
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Built Root"), "{stderr}");
    assert!(stderr.contains("error: build failed"), "{stderr}");
}

#[test]
fn inspect_dependencies_classifies_pins_and_writes_the_json_contract() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    let selected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let requested = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let third = "cccccccccccccccccccccccccccccccccccccccc";
    write_pin_project(
        &fixture,
        json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "name": "root",
            "packages": [
                pin_path_package("parent-a", "deps/parent-a", selected, false),
                pin_path_package("parent-b", "deps/parent-b", selected, false),
                pin_path_package("conflict", "deps/conflict", selected, true),
                pin_path_package("override", "deps/override", selected, false),
                pin_path_package("stale", "deps/stale", selected, true),
                {
                    "name": "unknown",
                    "type": "path",
                    "dir": "deps/unknown",
                    "inherited": false
                }
            ]
        }),
        &[
            (
                "deps/parent-a",
                json!({
                    "name": "parent-a",
                    "packages": [
                        pin_path_package("conflict", "deps/conflict", selected, false),
                        pin_path_package("override", "deps/override", requested, false),
                        pin_path_package("stale", "deps/stale", requested, false)
                    ]
                }),
            ),
            (
                "deps/parent-b",
                json!({
                    "name": "parent-b",
                    "packages": [
                        pin_path_package("conflict", "deps/conflict", requested, false),
                        pin_path_package("stale", "deps/stale", third, false)
                    ]
                }),
            ),
            ("deps/conflict", json!({"name": "conflict", "packages": []})),
            ("deps/override", json!({"name": "override", "packages": []})),
            ("deps/stale", json!({"name": "stale", "packages": []})),
            ("deps/unknown", json!({"name": "unknown", "packages": []})),
        ],
    );

    let plain = fixture
        .lev()
        .args(["inspect", "dependencies"])
        .output()
        .unwrap();
    assert_success_ref(&plain);
    let plain = String::from_utf8_lossy(&plain.stdout);
    assert!(
        plain.contains(
            "dependency pins for root: 2 aligned, 1 conflict, 1 root override, 1 stale, 1 unknown"
        ),
        "{plain}"
    );
    assert!(plain.contains("UNKNOWN  -            unknown"), "{plain}");

    let stdout = fixture
        .lev()
        .args(["inspect", "dependencies", "--json"])
        .output()
        .unwrap();
    assert_success_ref(&stdout);
    let report = json_data(&stdout, "lev.cli.inspect.dependencies/v1");
    assert_eq!(report["complete"], true);
    let statuses = report["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| {
            (
                package["name"].as_str().unwrap(),
                package["status"].as_str().unwrap(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(statuses["parent-a"], "aligned");
    assert_eq!(statuses["conflict"], "conflict");
    assert_eq!(statuses["override"], "root_override");
    assert_eq!(statuses["stale"], "stale");
    assert_eq!(statuses["unknown"], "unknown");

    let destination = fixture.root.join("reports/dependencies.json");
    let output = fixture
        .lev()
        .args(["inspect", "dependencies", "--json", "--output"])
        .arg(&destination)
        .output()
        .unwrap();
    assert_success_ref(&output);
    assert!(output.stdout.is_empty());

    let envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(destination).unwrap()).unwrap();
    assert_eq!(envelope["schema"], "lev.cli.inspect.dependencies/v1");
    assert_eq!(envelope["data"]["complete"], true);
}

#[test]
fn inspect_declaration_recovers_location_import_and_lean_checked_type() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(fixture.project.join("Root.lean"), "def hello := 1\n").unwrap();
    let index = fixture.project.join(".lake/build/lib/lean/Root.ilean");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::write(
        &index,
        serde_json::to_vec(&json!({
            "module": "Root",
            "decls": {
                "hello": [0, 4, 0, 9, "hello"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let lean_output = fixture.root.join("lean-check.jsonl");
    fs::write(
        &lean_output,
        "{\"data\":\"hello : (n : Nat) → n = n\",\"severity\":\"information\"}\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["inspect", "declaration", "hello", "--offline", "--json"])
        .env("LEV_TEST_LEAN_JSON_FILE", &lean_output)
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect.declaration/v1");
    assert_eq!(report["name"], "hello");
    assert_eq!(report["type"], "(n : Nat) → n = n");
    assert_eq!(report["module"], "Root");
    assert_eq!(report["import"], "import Root");
    assert_eq!(report["package"], "root");
    assert_eq!(report["range"]["start_line"], 1);
    assert_eq!(report["range"]["start_column"], 5);
    assert_eq!(
        PathBuf::from(report["source"].as_str().unwrap()),
        fixture.project.join("Root.lean")
    );
    assert!(fs::read_dir(&fixture.project).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".lev-inspect-")
    }));
}

#[test]
fn inspect_trust_uses_lean_results_and_enforces_explicit_policy() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(
        fixture.project.join("Root.lean"),
        "def clean := 1\n theorem admitted : True := by sorry\nunsafe def unchecked := 2\n",
    )
    .unwrap();
    let index = fixture.project.join(".lake/build/lib/Root.ilean");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::write(
        &index,
        serde_json::to_vec(&json!({
            "version": 3,
            "module": "Root",
            "decls": {
                "admitted": [0, 9, 0, 17, "admitted", 0, 0, 0, 21, 0, 9, 0, 17]
            },
            "references": {
                "{\"c\":{\"m\":\"Root\",\"n\":\"clean\"}}": {
                    "definition": [0, 4, 0, 9],
                    "usages": []
                },
                "{\"c\":{\"m\":\"Root\",\"n\":\"unchecked\"}}": {
                    "definition": [2, 11, 2, 20],
                    "usages": []
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let lean_output = fixture.root.join("lean-trust.jsonl");
    fs::write(
        &lean_output,
        concat!(
            "LEV_TRUST_JSON:{\"name\":\"admitted\",\"found\":true,\"unsafe\":false,",
            "\"axioms\":[\"sorryAx\"]}\n",
            "LEV_TRUST_JSON:{\"name\":\"clean\",\"found\":true,\"unsafe\":false,",
            "\"axioms\":[]}\n",
            "LEV_TRUST_JSON:{\"name\":\"unchecked\",\"found\":true,\"unsafe\":true,",
            "\"axioms\":[]}\n",
        ),
    )
    .unwrap();

    let output = fixture
        .lev()
        .args([
            "inspect",
            "trust",
            "--no-sync",
            "--offline",
            "--deny",
            "sorryAx",
            "--deny-unsafe",
            "--json",
        ])
        .env("LEV_TEST_LEAN_JSON_FILE", &lean_output)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_data(&output, "lev.cli.inspect.trust/v1");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["declarations_indexed"], 3);
    assert_eq!(report["declarations_scanned"], 3);
    assert_eq!(report["declarations_with_axioms"], 1);
    assert_eq!(report["unsafe_declarations"], 1);
    assert_eq!(report["policy_violations"], 2);
    assert_eq!(report["findings"][0]["name"], "admitted");
    assert_eq!(report["findings"][0]["range"]["start_line"], 1);
    assert_eq!(report["findings"][0]["range"]["start_column"], 10);
    assert_eq!(report["findings"][0]["denied_axioms"][0], "sorryAx");
    assert_eq!(report["findings"][1]["name"], "unchecked");
    assert_eq!(report["findings"][1]["unsafe_violation"], true);
    assert!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .contains("lake-build")
    );
    assert!(fs::read_dir(&fixture.project).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".lev-trust-")
    }));
}

#[test]
fn inspect_trust_reports_index_parse_failures_and_contextual_advice() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    let index = fixture.project.join(".lake/build/lib/lean/Root.ilean");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::write(&index, b"{not valid JSON").unwrap();

    let without_build = fixture
        .lev()
        .args(["inspect", "trust", "--no-build", "--no-sync", "--offline"])
        .output()
        .unwrap();
    assert!(!without_build.status.success());
    let stderr = String::from_utf8_lossy(&without_build.stderr);
    assert!(stderr.contains("could not parse"), "{stderr}");
    assert!(stderr.contains("rerun without `--no-build`"), "{stderr}");

    let after_build = fixture
        .lev()
        .args(["inspect", "trust", "--no-sync", "--offline"])
        .output()
        .unwrap();
    assert!(!after_build.status.success());
    let stderr = String::from_utf8_lossy(&after_build.stderr);
    assert!(stderr.contains("could not parse"), "{stderr}");
    assert!(!stderr.contains("rerun without `--no-build`"), "{stderr}");
    assert!(stderr.contains("`lev project clean`"), "{stderr}");
}

#[test]
fn inspect_trust_fails_when_lean_does_not_return_every_indexed_declaration() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(
        fixture.project.join("Root.lean"),
        "def clean := 1\ndef absent := 2\ndef missing := 3\n",
    )
    .unwrap();
    let index = fixture.project.join(".lake/build/lib/lean/Root.ilean");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::write(
        &index,
        serde_json::to_vec(&json!({
            "module": "Root",
            "decls": {
                "absent": [1, 4, 1, 10],
                "clean": [0, 4, 0, 9],
                "missing": [2, 4, 2, 11]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let lean_output = fixture.root.join("lean-trust-incomplete.jsonl");
    fs::write(
        &lean_output,
        concat!(
            "LEV_TRUST_JSON:{\"name\":\"absent\",\"found\":false,\"unsafe\":false,",
            "\"axioms\":[]}\n",
            "LEV_TRUST_JSON:{\"name\":\"clean\",\"found\":true,\"unsafe\":false,",
            "\"axioms\":[]}\n",
        ),
    )
    .unwrap();

    let output = fixture
        .lev()
        .args([
            "inspect",
            "trust",
            "--no-build",
            "--no-sync",
            "--offline",
            "--json",
        ])
        .env("LEV_TEST_LEAN_JSON_FILE", &lean_output)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_data(&output, "lev.cli.inspect.trust/v1");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["declarations_indexed"], 3);
    assert_eq!(report["declarations_scanned"], 1);
    assert_eq!(report["scan_failures"], 2);
    assert_eq!(report["policy_violations"], 0);
    let warnings = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|warning| warning.as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("absent") && warning.contains("not present"))
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("no trust record for missing"))
    );
}

#[test]
fn inspect_trust_batches_independent_modules_with_the_same_declaration_name() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(fixture.project.join("Root.lean"), "def main := 1\n").unwrap();
    fs::write(fixture.project.join("Tool.lean"), "def main := 2\n").unwrap();
    let index = fixture.project.join(".lake/build/lib/lean");
    fs::create_dir_all(&index).unwrap();
    for module in ["Root", "Tool"] {
        fs::write(
            index.join(format!("{module}.ilean")),
            serde_json::to_vec(&json!({
                "module": module,
                "decls": {
                    "main": [0, 4, 0, 8]
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let lean_output = fixture.root.join("lean-trust.jsonl");
    fs::write(
        &lean_output,
        "LEV_TRUST_JSON:{\"name\":\"main\",\"found\":true,\"unsafe\":false,\"axioms\":[]}\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .args([
            "inspect",
            "trust",
            "--no-build",
            "--no-sync",
            "--offline",
            "--json",
        ])
        .env("LEV_TEST_LEAN_JSON_FILE", &lean_output)
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.inspect.trust/v1");
    assert_eq!(report["declarations_indexed"], 2);
    assert_eq!(report["declarations_scanned"], 2);
    assert_eq!(report["policy_violations"], 0);
    assert_eq!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .matches("lake-script")
            .count(),
        2
    );
}

#[test]
fn check_selects_project_verification_or_single_file_elaboration() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    fs::write(
        fixture.project.join("lev.toml"),
        "[tasks]\ntest = [\"lake\", \"test\"]\n",
    )
    .unwrap();
    assert_success(fixture.lev().arg("sync").arg("--offline").output().unwrap());

    let project = fixture
        .lev()
        .args(["check", "--offline", "--json"])
        .output()
        .unwrap();
    assert_success_ref(&project);
    let report = json_data(&project, "lev.cli.check/v1");
    let phases = report["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|phase| phase["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        phases,
        [
            "configuration",
            "lock",
            "dependencies",
            "audit",
            "toolchain",
            "build"
        ]
    );

    let source = fixture.project.join("Root.lean");
    fs::write(&source, "def root := 1\n").unwrap();
    let file = fixture
        .lev()
        .args(["check", "--file"])
        .arg(&source)
        .args(["--no-sync", "--offline"])
        .output()
        .unwrap();
    assert_success_ref(&file);
    assert!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .contains("lake-lean")
    );

    let legacy_file = fixture
        .lev()
        .arg("check")
        .arg(&source)
        .args(["--no-sync", "--offline"])
        .output()
        .unwrap();
    assert_success_ref(&legacy_file);

    let legacy_with_lean_flag = fixture
        .lev()
        .arg("check")
        .arg(&source)
        .args(["--no-sync", "--offline", "-Dpp.universes=true"])
        .output()
        .unwrap();
    assert_success_ref(&legacy_with_lean_flag);
    assert!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("lake-lean\t") && line.ends_with("-Dpp.universes=true"))
    );

    let target = fixture
        .lev()
        .args([
            "check",
            "MyLibrary",
            "--task",
            "test",
            "--offline",
            "--json",
        ])
        .output()
        .unwrap();
    assert_success_ref(&target);
    let report = json_data(&target, "lev.cli.check/v1");
    assert_eq!(
        report["phases"].as_array().unwrap().last().unwrap()["name"],
        "task:test"
    );
    assert!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .lines()
            .any(
                |line| line.starts_with("lake-build\t") && line.ends_with("\tlake build MyLibrary")
            )
    );

    let passthrough = fixture
        .lev()
        .args(["check", "MyLibrary", "--offline", "--", "--some-lake-flag"])
        .output()
        .unwrap();
    assert_success_ref(&passthrough);
    assert!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .lines()
            .any(|line| {
                line.starts_with("lake-build\t")
                    && line.ends_with("\tlake build MyLibrary --some-lake-flag")
            })
    );
}

#[test]
fn check_rejects_provably_stale_transitive_dependency_selections() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    let selected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let requested = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let third = "cccccccccccccccccccccccccccccccccccccccc";
    write_pin_project(
        &fixture,
        json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "name": "root",
            "packages": [
                pin_path_package("parent-a", "deps/parent-a", selected, false),
                pin_path_package("parent-b", "deps/parent-b", requested, false),
                pin_path_package("stale", "deps/stale", selected, true)
            ]
        }),
        &[
            (
                "deps/parent-a",
                json!({
                    "name": "parent-a",
                    "packages": [
                        pin_path_package("stale", "deps/stale", requested, false)
                    ]
                }),
            ),
            (
                "deps/parent-b",
                json!({
                    "name": "parent-b",
                    "packages": [
                        pin_path_package("stale", "deps/stale", third, false)
                    ]
                }),
            ),
            ("deps/stale", json!({"name": "stale", "packages": []})),
        ],
    );
    assert_success(fixture.lev().arg("lock").output().unwrap());

    let output = fixture
        .lev()
        .args(["check", "--offline", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report = json_data(&output, "lev.cli.check/v1");
    let phase = report["phases"].as_array().unwrap().last().unwrap();
    assert_eq!(phase["name"], "dependencies");
    assert_eq!(phase["status"], "failed");
    assert!(
        phase["detail"]
            .as_str()
            .unwrap()
            .contains("1 stale, 0 conflicts, 0 root overrides, 0 unknown"),
        "{phase}"
    );
    assert!(
        phase["detail"].as_str().unwrap().contains("stale"),
        "{phase}"
    );
    assert!(
        !fs::read_to_string(&fixture.log)
            .unwrap_or_default()
            .contains("lake-build"),
        "a stale dependency selection should stop before the build"
    );
}

#[test]
fn check_reports_dependency_conflicts_and_incomplete_evidence_without_guessing() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    let selected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let requested = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    write_pin_project(
        &fixture,
        json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "name": "root",
            "packages": [
                pin_path_package("parent-a", "deps/parent-a", selected, false),
                pin_path_package("parent-b", "deps/parent-b", requested, false),
                pin_path_package("conflict", "deps/conflict", selected, true),
                pin_path_package("override", "deps/override", selected, false),
                pin_path_package("unknown", "deps/unknown", selected, true),
                pin_path_package("missing-parent", "deps/missing-parent", selected, false)
            ]
        }),
        &[
            (
                "deps/parent-a",
                json!({
                    "name": "parent-a",
                    "packages": [
                        pin_path_package("conflict", "deps/conflict", selected, false),
                        pin_path_package("override", "deps/override", requested, false),
                        pin_path_package("unknown", "deps/unknown", selected, false)
                    ]
                }),
            ),
            (
                "deps/parent-b",
                json!({
                    "name": "parent-b",
                    "packages": [
                        pin_path_package("conflict", "deps/conflict", requested, false)
                    ]
                }),
            ),
            ("deps/conflict", json!({"name": "conflict", "packages": []})),
            ("deps/override", json!({"name": "override", "packages": []})),
            ("deps/unknown", json!({"name": "unknown", "packages": []})),
            ("deps/missing-parent", serde_json::Value::Null),
        ],
    );
    assert_success(fixture.lev().arg("lock").output().unwrap());

    let output = fixture
        .lev()
        .args(["check", "--offline", "--json"])
        .output()
        .unwrap();
    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.check/v1");
    let phase = report["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|phase| phase["name"] == "dependencies")
        .unwrap();
    assert_eq!(phase["status"], "passed");
    let detail = phase["detail"].as_str().unwrap();
    assert!(detail.contains("0 stale"), "{detail}");
    assert!(detail.contains("1 conflict"), "{detail}");
    assert!(detail.contains("1 root override"), "{detail}");
    assert!(!detail.contains("0 unknown"), "{detail}");
}

fn pin_path_package(
    name: &str,
    directory: &str,
    revision: &str,
    inherited: bool,
) -> serde_json::Value {
    json!({
        "name": name,
        "type": "path",
        "dir": directory,
        "rev": revision,
        "inherited": inherited
    })
}

fn write_pin_project(
    fixture: &Fixture,
    manifest: serde_json::Value,
    children: &[(&str, serde_json::Value)],
) {
    fs::write(
        fixture.project.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    for (relative, child) in children {
        let directory = fixture.project.join(relative);
        fs::create_dir_all(&directory).unwrap();
        if !child.is_null() {
            fs::write(
                directory.join("lake-manifest.json"),
                serde_json::to_vec_pretty(child).unwrap(),
            )
            .unwrap();
        }
    }
}

#[test]
fn check_rejects_flags_from_the_other_mode() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();

    let file = fixture
        .lev()
        .args(["check", "--file", "Root.lean", "--task", "test"])
        .output()
        .unwrap();
    assert_eq!(file.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&file.stderr).contains("cannot be used with"));

    for args in [
        &["check", "MyLibrary", "--no-sync"][..],
        &["check", "--lean", "stable"][..],
    ] {
        let project = fixture.lev().args(args).output().unwrap();
        assert_eq!(project.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&project.stderr)
                .contains("require `--file PATH` or a positional Lean source file")
        );
    }

    let positional_file = fixture
        .lev_at(&outside)
        .args(["check", "Root.lean", "--json"])
        .output()
        .unwrap();
    assert_eq!(positional_file.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&positional_file.stderr);
    assert!(stderr.contains("cannot be used with project-check options"));
    assert!(stderr.contains("Usage: lev check"));

    let file_only_flag = fixture
        .lev_at(&outside)
        .args(["check", "MyLibrary", "--no-sync"])
        .output()
        .unwrap();
    assert_eq!(file_only_flag.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&file_only_flag.stderr);
    assert!(stderr.contains("require `--file PATH` or a positional Lean source file"));
    assert!(stderr.contains("Usage: lev check"));
}
