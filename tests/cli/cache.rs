use super::*;

const MAPPING_KEPT: &str = "aaaaaaaaaaaaaaaa";
const MAPPING_UNUSED: &str = "bbbbbbbbbbbbbbbb";

/// Build once so the project is registered and its trace names the kept
/// mapping and one artifact, then fill the Lake cache around it.
fn registered_project_with_cache(fixture: &Fixture) {
    let after = fixture.root.join("after");
    let mut trace = module_trace(
        MAPPING_KEPT,
        (&fixture.project.join("Root.lean"), "1"),
        &[],
        "0123456789abcdef",
    );
    trace["outputs"] = json!({"o": ["0123456789abcdef.olean"], "m": true});
    write_trace(&after, ".lake/build/lib/lean/Root.trace", &trace);
    let build = fixture
        .lev()
        .args(["inspect", "build", "--json"])
        .env("LEV_TEST_LAKE_TREE", &after)
        .output()
        .unwrap();
    assert_success(&build);

    let lake = fixture.lake_cache();
    for artifact in [
        "0123456789abcdef.olean",
        "1111111111111111.olean",
        "2222222222222222.olean",
        "3333333333333333.olean",
    ] {
        write_file(&lake.join("artifacts").join(artifact), artifact);
    }
    for (hash, artifact) in [
        (MAPPING_KEPT, "1111111111111111.olean"),
        (MAPPING_UNUSED, "2222222222222222.olean"),
    ] {
        write_file(
            &lake.join(format!("outputs/root/{hash}.json")),
            &json!({"schemaVersion": "2026-02-25", "service": null, "data": {"o": [artifact]}})
                .to_string(),
        );
    }
}

fn write_file(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn candidates(report: &Value) -> Vec<(String, PathBuf)> {
    report["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| {
            (
                candidate["kind"].as_str().unwrap().to_owned(),
                PathBuf::from(candidate["path"].as_str().unwrap()),
            )
        })
        .collect()
}

#[test]
fn cache_status_inventories_the_lake_cache_of_registered_projects() {
    let fixture = Fixture::new();
    registered_project_with_cache(&fixture);

    let output = fixture
        .lev()
        .args(["cache", "status", "--json"])
        .output()
        .unwrap();
    assert_success(&output);
    let report = json_data(&output, "lev.cli.cache.status/v2");
    assert_eq!(report["lev_cache"]["projects"], 1);
    assert_eq!(report["lev_cache"]["missing_projects"], 0);
    let lake = &report["lake_caches"][0];
    assert_eq!(
        PathBuf::from(lake["path"].as_str().unwrap()),
        fixture.lake_cache()
    );
    assert_eq!(lake["projects"], 1);
    assert_eq!(lake["artifacts"], 4);
    assert_eq!(lake["mappings"], 2);
    assert_eq!(lake["unmapped_artifacts"], 2);
    assert_eq!(lake["missing"], json!([]));

    let text = stdout(&fixture.lev().args(["cache", "status"]).output().unwrap());
    assert!(
        text.contains("projects    1 registered, 0 no longer on disk\n"),
        "{text}"
    );
    assert!(text.contains("  mappings    2\n"), "{text}");
}

#[test]
fn cache_gc_keeps_what_registered_projects_reference() {
    let fixture = Fixture::new();
    registered_project_with_cache(&fixture);
    let lake = fixture.lake_cache();
    let artifact = |name: &str| lake.join("artifacts").join(name);

    let young = fixture
        .lev()
        .args(["cache", "gc", "--lake", "--json"])
        .output()
        .unwrap();
    assert_success(&young);
    assert_eq!(candidates(&json_data(&young, "lev.cli.cache.gc/v2")), []);

    // Without --lake, Lake's cache is left alone however old it is.
    let own = fixture
        .lev()
        .args(["cache", "gc", "--max-age-days", "0"])
        .output()
        .unwrap();
    assert_success(&own);
    assert!(
        stdout(&own).contains("nothing to collect"),
        "{}",
        stdout(&own)
    );
    assert!(stderr(&own).contains("pass --lake"), "{}", stderr(&own));

    let dry = fixture
        .lev()
        .args(["cache", "gc", "--lake", "--max-age-days", "0", "--json"])
        .output()
        .unwrap();
    assert_success(&dry);
    let report = json_data(&dry, "lev.cli.cache.gc/v2");
    assert_eq!(report["applied"], false);
    assert_eq!(report["lake"], true);
    // The toolchain's default cache serves projects lev does not know.
    assert_eq!(report["shared"], json!([lake]));
    let unused_mapping = lake.join(format!("outputs/root/{MAPPING_UNUSED}.json"));
    assert_eq!(
        candidates(&report),
        [
            ("lake-mapping".to_owned(), unused_mapping.clone()),
            (
                "lake-artifact".to_owned(),
                artifact("2222222222222222.olean")
            ),
            (
                "lake-artifact".to_owned(),
                artifact("3333333333333333.olean")
            ),
        ]
    );
    assert!(unused_mapping.exists());

    let listed = fixture
        .lev()
        .args(["cache", "gc", "--lake", "--max-age-days", "0"])
        .output()
        .unwrap();
    assert_success(&listed);
    assert!(
        stdout(&listed).contains(&format!(
            "shared {}: projects lev has not registered may use it too",
            lake.display()
        )),
        "{}",
        stdout(&listed)
    );

    let text = fixture
        .lev()
        .args(["cache", "gc", "--lake", "--max-age-days", "0", "--apply"])
        .output()
        .unwrap();
    assert_success(&text);
    assert!(
        stdout(&text).contains("removed 3 entries"),
        "{}",
        stdout(&text)
    );
    assert!(!unused_mapping.exists());
    assert!(!artifact("3333333333333333.olean").exists());
    assert!(artifact("0123456789abcdef.olean").exists());
    assert!(artifact("1111111111111111.olean").exists());
    assert!(
        lake.join(format!("outputs/root/{MAPPING_KEPT}.json"))
            .exists()
    );

    // Once the project is gone, nothing protects its record or its entries.
    fs::remove_dir_all(&fixture.project).unwrap();
    let elsewhere = fixture.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let gone = fixture
        .lev_at(&elsewhere)
        .args([
            "cache",
            "gc",
            "--lake",
            "--max-age-days",
            "0",
            "--apply",
            "--json",
        ])
        .output()
        .unwrap();
    assert_success(&gone);
    let kinds = candidates(&json_data(&gone, "lev.cli.cache.gc/v2"))
        .into_iter()
        .map(|(kind, _)| kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "project-record",
            "lake-mapping",
            "lake-artifact",
            "lake-artifact"
        ]
    );
    assert_eq!(fs::read_dir(lake.join("artifacts")).unwrap().count(), 0);
}

#[test]
fn cache_status_fails_on_missing_artifacts_and_gc_leaves_that_cache_alone() {
    let fixture = Fixture::new();
    registered_project_with_cache(&fixture);
    let lake = fixture.lake_cache();
    fs::remove_file(lake.join("artifacts/1111111111111111.olean")).unwrap();

    let status = fixture
        .lev()
        .args(["cache", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(status.status.code(), Some(1), "{}", stderr(&status));
    let report = json_data(&status, "lev.cli.cache.status/v2");
    assert_eq!(
        report["lake_caches"][0]["missing"][0]["artifact"],
        "1111111111111111.olean"
    );

    let gc = fixture
        .lev()
        .args([
            "cache",
            "gc",
            "--lake",
            "--max-age-days",
            "0",
            "--apply",
            "--json",
        ])
        .output()
        .unwrap();
    assert_success(&gc);
    let report = json_data(&gc, "lev.cli.cache.gc/v2");
    assert_eq!(candidates(&report), []);
    assert!(
        report["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("missing artifacts"),
        "{report}"
    );
    assert!(lake.join("artifacts/3333333333333333.olean").exists());
}

#[test]
fn cache_gc_leaves_an_older_lake_layout_alone() {
    let fixture = Fixture::new();
    registered_project_with_cache(&fixture);
    let lake = fixture.lake_cache();
    write_file(&lake.join("inputs/root.jsonl"), "[\"1\",42]\n");

    let gc = fixture
        .lev()
        .args([
            "cache",
            "gc",
            "--lake",
            "--max-age-days",
            "0",
            "--apply",
            "--json",
        ])
        .output()
        .unwrap();
    assert_success(&gc);
    let report = json_data(&gc, "lev.cli.cache.gc/v2");
    assert_eq!(candidates(&report), []);
    assert!(
        report["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("older inputs/ layout"),
        "{report}"
    );
    assert!(lake.join("artifacts/3333333333333333.olean").exists());
}

#[test]
fn cache_gc_skips_a_lake_cache_whose_references_it_cannot_read_in_full() {
    let fixture = Fixture::new();
    registered_project_with_cache(&fixture);
    let lake = fixture.lake_cache();
    let gc = || {
        let output = fixture
            .lev()
            .args([
                "cache",
                "gc",
                "--lake",
                "--max-age-days",
                "0",
                "--apply",
                "--json",
            ])
            .output()
            .unwrap();
        assert_success(&output);
        let report = json_data(&output, "lev.cli.cache.gc/v2");
        assert_eq!(candidates(&report), []);
        assert_eq!(report["skipped"][0]["path"], json!(lake));
        report["skipped"][0]["reason"].as_str().unwrap().to_owned()
    };

    // A trace lev cannot parse may name entries of this cache.
    let broken = fixture.project.join(".lake/build/lib/lean/Broken.trace");
    write_file(&broken, "not json");
    let reason = gc();
    assert!(reason.contains("Broken.trace"), "{reason}");
    fs::remove_file(&broken).unwrap();

    // A lakefile may put the build, and its traces, elsewhere.
    let lakefile = fixture.project.join("lakefile.toml");
    let original = fs::read_to_string(&lakefile).unwrap();
    fs::write(&lakefile, format!("buildDir = \"out\"\n{original}")).unwrap();
    let reason = gc();
    assert!(reason.contains("buildDir"), "{reason}");
    fs::write(&lakefile, original).unwrap();

    // Without the manifest, lev does not know the project's packages.
    fs::write(fixture.project.join("lake-manifest.json"), "{").unwrap();
    let reason = gc();
    assert!(reason.contains("lake-manifest.json"), "{reason}");

    assert!(lake.join("artifacts/3333333333333333.olean").exists());
    assert!(
        lake.join(format!("outputs/root/{MAPPING_UNUSED}.json"))
            .exists()
    );
}
