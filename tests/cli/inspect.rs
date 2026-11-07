use super::*;

/// Lay out a rebuild with one edit, a direct importer, an importer whose
/// previous trace came from a cache download, and a dependency whose options
/// changed. Returns the environment that makes the fake Lake perform it.
fn rebuild(fixture: &Fixture) -> Vec<(&'static str, PathBuf)> {
    let project = &fixture.project;
    fs::write(
        project.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "name": "root",
            "packagesDir": ".lake/packages",
            "packages": [{"name": "dep", "type": "git", "url": "https://example.invalid/dep"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let source = |module: &str| project.join(format!("Root/{module}.lean"));
    let library = ".lake/build/lib/lean";
    let dependency = ".lake/packages/dep/.lake/build/lib/lean/Dep.trace";

    write_trace(
        project,
        &format!("{library}/Root/A.trace"),
        &module_trace(
            "a000000000000001",
            (&source("A"), "1"),
            &[],
            "aaaa000000000000",
        ),
    );
    write_trace(
        project,
        &format!("{library}/Root/B.trace"),
        &module_trace(
            "b000000000000001",
            (&source("B"), "1"),
            &[("Root.A", "1")],
            "bbbb000000000000",
        ),
    );
    write_trace(
        project,
        &format!("{library}/Root/C.trace"),
        &cached_trace("c000000000000001", "cccc000000000000"),
    );
    write_trace(
        project,
        &format!("{library}/Root/Same.trace"),
        &module_trace(
            "d000000000000001",
            (&source("Same"), "1"),
            &[],
            "dddd000000000000",
        ),
    );
    write_trace(
        project,
        dependency,
        &json!({"depHash": "e000000000000001", "inputs": [["options", "1"]]}),
    );

    let after = fixture.root.join("after");
    write_trace(
        &after,
        &format!("{library}/Root/A.trace"),
        &module_trace(
            "a000000000000002",
            (&source("A"), "2"),
            &[],
            "aaaa111111111111",
        ),
    );
    write_trace(
        &after,
        &format!("{library}/Root/B.trace"),
        &module_trace(
            "b000000000000002",
            (&source("B"), "1"),
            &[("Root.A", "2")],
            "bbbb000000000000",
        ),
    );
    write_trace(
        &after,
        &format!("{library}/Root/C.trace"),
        &module_trace(
            "c000000000000002",
            (&source("C"), "1"),
            &[("Root.B", "2")],
            "cccc000000000000",
        ),
    );
    write_trace(
        &after,
        dependency,
        &json!({"depHash": "e000000000000002", "inputs": [["options", "2"]]}),
    );
    let output = fixture.file(
        "lake-stdout",
        concat!(
            "✔ [1/5] Built Root.A (12ms)\n",
            "✔ [2/5] Built Root.B (30ms)\n",
            "✔ [3/5] Built Root.C (1.5s)\n",
            "✔ [4/5] Built Dep (5ms)\n",
            "ℹ [5/5] Replayed Root.Same\n",
        ),
    );
    vec![
        ("LEV_TEST_LAKE_TREE", after),
        ("LEV_TEST_LAKE_STDOUT", output),
    ]
}

#[test]
fn inspect_build_reads_rebuild_causes_from_every_package_trace() {
    let fixture = Fixture::new();
    let output = fixture
        .lev()
        .args([
            "inspect", "build", "--json", "--why", "Root.C", "--", "Root",
        ])
        .envs(rebuild(&fixture))
        .output()
        .unwrap();
    assert_success(&output);
    let report = json_data(&output, "lev.cli.inspect.build/v2");

    assert_eq!(report["status"], "passed");
    assert_eq!(report["traces_read"], true);
    assert_eq!(report["command"], "lake --verbose build Root");
    assert_eq!(report["toolchain"], TEST_TOOLCHAIN);
    assert_eq!(report["actions"]["built"], 4);
    assert_eq!(report["actions"]["replayed"], 1);
    assert_eq!(report["events"].as_array().unwrap().len(), 5);

    let modules = report["modules"].as_array().unwrap();
    let names = modules
        .iter()
        .map(|module| module["module"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Dep", "Root.A", "Root.B", "Root.C"]);
    assert_eq!(modules[0]["package"], "dep");
    assert_eq!(
        modules[0]["causes"],
        json!([{"kind": "options", "input": "options"}])
    );
    assert_eq!(
        modules[1]["causes"],
        json!([{"kind": "source", "input": fixture.project.join("Root/A.lean")}])
    );
    assert_eq!(modules[1]["action"], "built");
    assert_eq!(modules[1]["elapsed_ms"], 12.0);
    assert_eq!(
        modules[2]["causes"],
        json!([{"kind": "import", "input": "Root.A"}])
    );
    assert_eq!(modules[2]["inferred"], false);
    // Root.C's previous trace had no inputs; its cause comes from Root.B.
    assert_eq!(modules[3]["inferred"], true);
    assert_eq!(
        modules[3]["causes"],
        json!([{"kind": "import", "input": "Root.B"}])
    );
    assert_eq!(modules[3]["elapsed_ms"], 1500.0);

    assert_eq!(report["edited"], json!(["Root.A"]));
    assert_eq!(report["unexplained"], json!([]));
    assert_eq!(report["removed"], json!([]));
    assert_eq!(report["why"], json!(["Root.C", "Root.B", "Root.A"]));
    assert!(
        stderr(&output).contains("Built Root.A (12ms)"),
        "{}",
        stderr(&output)
    );

    let records = fs::read_dir(fixture.cache.join("projects-v2"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
    let record: Value = serde_json::from_slice(&fs::read(&records[0]).unwrap()).unwrap();
    assert_eq!(
        PathBuf::from(record["lake_cache"].as_str().unwrap()),
        fixture.lake_cache()
    );
}

#[test]
fn inspect_build_text_report_summarizes_and_quiet_hides_lake_output() {
    let fixture = Fixture::new();
    let output = fixture
        .lev()
        .args(["-q", "inspect", "build", "--why", "Root.C"])
        .envs(rebuild(&fixture))
        .output()
        .unwrap();
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.starts_with("build passed in "), "{text}");
    assert!(text.contains("4 built"), "{text}");
    assert!(
        text.contains("traces: 4 changed, 0 new, 0 removed"),
        "{text}"
    );
    assert!(
        text.contains("causes: 1 source, 2 import, 1 options"),
        "{text}"
    );
    assert!(text.contains("edited: Root.A\n"), "{text}");
    assert!(
        text.contains("slowest:\n        1500 ms  Root.C\n"),
        "{text}"
    );
    assert!(text.contains("  Root.C: import Root.B\n"), "{text}");
    assert!(text.contains("  Root.A: source "), "{text}");
    assert!(!stderr(&output).contains("Built"), "{}", stderr(&output));
}

#[test]
fn inspect_build_keeps_lake_failures_and_their_root_causes() {
    let fixture = Fixture::new();
    let failure = fixture.file(
        "lake-stderr",
        concat!(
            "✖ [1/1] Building Root (15ms)\n",
            "error: Root.lean:7:3: unknown identifier 'missing'\n",
            "Some required targets logged failures:\n",
            "- Root\n",
            "error: build failed\n",
        ),
    );
    let output = fixture
        .lev()
        .args(["inspect", "build", "--json"])
        .env("LEV_TEST_LAKE_STDERR", failure)
        .env("LEV_TEST_LAKE_EXIT", "37")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37), "{}", stderr(&output));
    let report = json_data(&output, "lev.cli.inspect.build/v2");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["exit_code"], 37);
    assert_eq!(
        report["root_causes"],
        json!([{
            "kind": "lean_diagnostic",
            "file": "Root.lean",
            "line": 7,
            "column": 3,
            "message": "unknown identifier 'missing'"
        }])
    );
    assert_eq!(report["failed_targets"], json!(["Root"]));
    assert_eq!(report["events"][0]["failed"], true);
    assert!(stderr(&output).contains("error: build failed"));
}

#[test]
fn inspect_build_still_builds_when_traces_cannot_be_read() {
    let fixture = Fixture::new();
    let library = fixture.project.join(".lake/build/lib/lean");
    fs::create_dir_all(&library).unwrap();
    fs::set_permissions(&library, fs::Permissions::from_mode(0o000)).unwrap();

    let output = fixture
        .lev()
        .args(["inspect", "build", "--json"])
        .output()
        .unwrap();
    fs::set_permissions(&library, fs::Permissions::from_mode(0o755)).unwrap();

    assert_success(&output);
    let report = json_data(&output, "lev.cli.inspect.build/v2");
    assert_eq!(report["traces_read"], false);
    assert_eq!(report["modules"], json!([]));
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("before the build")),
        "{report}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(log.contains("lake-build\t"), "{log}");
}

#[test]
fn inspect_build_reports_a_first_build_as_new_modules() {
    let fixture = Fixture::new();
    let after = fixture.root.join("after");
    write_trace(
        &after,
        ".lake/build/lib/lean/Root.trace",
        &module_trace(
            "a000000000000001",
            (&fixture.project.join("Root.lean"), "1"),
            &[],
            "aaaa000000000000",
        ),
    );
    let output = fixture
        .lev()
        .args(["inspect", "build", "--json", "--why", "Missing"])
        .env("LEV_TEST_LAKE_TREE", &after)
        .output()
        .unwrap();
    assert_success(&output);
    let report = json_data(&output, "lev.cli.inspect.build/v2");
    assert_eq!(report["modules"][0]["module"], "Root");
    assert_eq!(report["modules"][0]["status"], "new");
    assert_eq!(report["edited"], json!([]));
    assert_eq!(report["unexplained"], json!([]));
    assert_eq!(report["why"], json!([]));
    assert!(
        report["warnings"][0]
            .as_str()
            .unwrap()
            .contains("Missing did not rebuild"),
        "{report}"
    );
}
