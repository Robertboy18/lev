use super::*;

#[test]
fn reservoir_outdated_and_upgrade_are_cached_checked_and_transactional() {
    let fixture = Fixture::new();
    let old_revision = fixture.commit("def answer := 1\n", "old");
    let new_revision = fixture.commit("def answer := 2\n", "new");
    let old_manifest = fixture.registry_manifest(&old_revision, "v1.0.0");
    let new_manifest = fixture.registry_manifest(&new_revision, "v2.0.0");
    fs::write(fixture.project.join("lake-manifest.json"), &old_manifest).unwrap();
    fs::write(
        fixture.project.join("lakefile.toml"),
        r#"[package]
name = "root"

[[require]]
name = "dep"
scope = "test-owner"
rev = "v1.0.0"
"#,
    )
    .unwrap();
    let replacement_manifest = fixture.root.join("replacement-manifest.json");
    fs::write(&replacement_manifest, &new_manifest).unwrap();

    let (api, server) = reservoir_server(&old_revision, &new_revision);
    let outdated = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .arg("outdated")
        .arg("--refresh")
        .output()
        .unwrap();
    assert_success_ref(&outdated);
    let stdout = String::from_utf8_lossy(&outdated.stdout);
    assert!(stdout.contains("dep"), "{stdout}");
    assert!(stdout.contains("v2.0.0"), "{stdout}");
    assert!(stdout.contains("outdated"), "{stdout}");
    server.join().unwrap();

    let checked = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .arg("outdated")
        .arg("--offline")
        .arg("--check")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(checked.status.code(), Some(1));
    let checked = json_data(&checked, "lev.cli.outdated/v1");
    assert_eq!(checked[0]["package"], "dep");
    assert_eq!(checked[0]["status"], "outdated");

    let original_config = fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap();
    let dry_run = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .arg("upgrade")
        .arg("dep")
        .arg("--dry-run")
        .output()
        .unwrap();
    assert_success_ref(&dry_run);
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("Dry run"));
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        original_config
    );

    let mismatch = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .arg("upgrade")
        .arg("dep")
        .output()
        .unwrap();
    assert!(!mismatch.status.success());
    assert!(
        String::from_utf8_lossy(&mismatch.stderr).contains("rolling back"),
        "{}",
        String::from_utf8_lossy(&mismatch.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        old_manifest
    );

    let upgraded = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .arg("upgrade")
        .arg("dep")
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &replacement_manifest)
        .output()
        .unwrap();
    assert_success_ref(&upgraded);
    let config = fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap();
    assert!(config.contains("rev = \"v2.0.0\""), "{config}");
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        new_manifest
    );
    assert_eq!(
        git_output(
            &fixture.project.join(".lake/packages/dep"),
            &["rev-parse", "HEAD"]
        ),
        new_revision
    );
    assert!(fixture.project.join("lev.lock").is_file());
}

#[test]
fn configured_registry_routes_authenticates_and_reuses_private_metadata() {
    let fixture = Fixture::new();
    let old_revision = fixture.commit("def answer := 1\n", "old");
    let new_revision = fixture.commit("def answer := 2\n", "new");
    fs::write(
        fixture.project.join("lake-manifest.json"),
        fixture.registry_manifest(&old_revision, "v1.0.0"),
    )
    .unwrap();
    fs::write(
        fixture.project.join("lakefile.toml"),
        r#"[package]
name = "root"

[[require]]
name = "dep"
scope = "test-owner"
rev = "v1.0.0"
"#,
    )
    .unwrap();

    let (api, server) =
        authenticated_reservoir_server(&old_revision, &new_revision, "secret-token");
    fs::write(
        fixture.project.join("lev.toml"),
        format!(
            r#"[registries.private]
url = "{api}"
token-env = "LEV_TEST_PRIVATE_REGISTRY_TOKEN"

[sources]
"test-owner/dep" = "private"
"#
        ),
    )
    .unwrap();

    let online = fixture
        .lev()
        .env("LEV_TEST_PRIVATE_REGISTRY_TOKEN", "secret-token")
        .arg("outdated")
        .arg("--refresh")
        .output()
        .unwrap();
    server.join().unwrap();
    assert_success_ref(&online);
    assert!(
        String::from_utf8_lossy(&online.stdout).contains("v2.0.0"),
        "{}",
        String::from_utf8_lossy(&online.stdout)
    );

    let offline = fixture
        .lev()
        .env("LEV_TEST_PRIVATE_REGISTRY_TOKEN", "secret-token")
        .arg("outdated")
        .arg("--offline")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&offline);
    let offline = json_data(&offline, "lev.cli.outdated/v1");
    assert_eq!(offline[0]["metadata_cache_hit"], true);

    let missing_credential = fixture
        .lev()
        .env_remove("LEV_TEST_PRIVATE_REGISTRY_TOKEN")
        .arg("outdated")
        .arg("--offline")
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(missing_credential.status.code(), Some(2));
    let missing_credential = json_data(&missing_credential, "lev.cli.outdated/v1");
    assert!(
        missing_credential[0]["error"]
            .as_str()
            .unwrap()
            .contains("requires environment variable LEV_TEST_PRIVATE_REGISTRY_TOKEN"),
        "{missing_credential}"
    );
}

#[test]
fn dependency_edits_pin_and_rollback_are_transactional() {
    let fixture = Fixture::new();
    let original = fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap();

    let output = fixture
        .lev()
        .arg("add")
        .arg("dep")
        .arg("--scope")
        .arg("test-owner")
        .arg("--rev")
        .arg("v1.2.3")
        .arg("--no-sync")
        .output()
        .unwrap();
    assert_success_ref(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("added dependency dep at v1.2.3"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let edited = fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap();
    assert!(edited.contains("[[require]]"));
    assert!(edited.contains("name = \"dep\""));
    assert!(edited.contains("scope = \"test-owner\""));

    let duplicate = fixture
        .lev()
        .arg("add")
        .arg("dep")
        .arg("--no-sync")
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        edited
    );

    assert_success(
        fixture
            .lev()
            .arg("remove")
            .arg("dep")
            .arg("--no-sync")
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        original
    );

    assert_success(
        fixture
            .lev()
            .arg("add")
            .arg("sibling")
            .arg("--path")
            .arg("../sibling")
            .arg("--no-sync")
            .output()
            .unwrap(),
    );
    let with_path = fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap();
    assert!(with_path.contains("name = \"sibling\""), "{with_path}");
    assert!(with_path.contains("path = \"../sibling\""), "{with_path}");
    assert_success(
        fixture
            .lev()
            .arg("remove")
            .arg("sibling")
            .arg("--no-sync")
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        original
    );

    let failed = fixture
        .lev()
        .arg("add")
        .arg("local")
        .arg("--path")
        .arg("../local")
        .env("LEV_TEST_LAKE_UPDATE_FAIL", "1")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        original
    );

    fs::write(&fixture.log, "").unwrap();
    assert_success(
        fixture
            .lev()
            .arg("pin")
            .arg("4.fixture-d")
            .arg("--offline")
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("lean-toolchain")).unwrap(),
        "leanprover/lean4:v4.fixture-d\n"
    );
    assert!(
        !fs::read_to_string(&fixture.log)
            .unwrap()
            .contains("--install")
    );
}

#[test]
fn package_names_do_not_infer_registry_scope_or_revision() {
    let fixture = Fixture::new();

    assert_success(
        fixture
            .lev()
            .arg("add")
            .arg("ordinary-package")
            .arg("--no-sync")
            .output()
            .unwrap(),
    );

    let lakefile = fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap();
    assert!(
        lakefile.contains("name = \"ordinary-package\""),
        "{lakefile}"
    );
    assert!(!lakefile.contains("scope ="), "{lakefile}");
    assert!(!lakefile.contains("rev ="), "{lakefile}");
}

#[test]
fn dependency_groups_select_updates_and_are_cleaned_on_remove() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "grouped dependency");
    fixture.write_manifest(&revision);

    assert_success(
        fixture
            .lev()
            .arg("add")
            .arg("dep")
            .arg("--git")
            .arg(&fixture.remote)
            .arg("--rev")
            .arg(&revision)
            .arg("--group")
            .arg("dev")
            .arg("--no-sync")
            .output()
            .unwrap(),
    );
    let config = fs::read_to_string(fixture.project.join("lev.toml")).unwrap();
    assert!(config.contains("[dependency-groups]"), "{config}");
    assert!(config.contains(r#"dev = ["dep"]"#), "{config}");

    assert_success(
        fixture
            .lev()
            .env("LEV_TEST_LAKE_UPDATE_EXPECT_PACKAGE", "dep")
            .arg("update")
            .arg("--group")
            .arg("dev")
            .output()
            .unwrap(),
    );

    assert_success(
        fixture
            .lev()
            .arg("remove")
            .arg("dep")
            .arg("--no-sync")
            .output()
            .unwrap(),
    );
    let config = fs::read_to_string(fixture.project.join("lev.toml")).unwrap();
    assert!(!config.contains("dependency-groups"), "{config}");
}

#[test]
fn constraints_reject_and_roll_back_a_lake_resolution() {
    let fixture = Fixture::new();
    let old_revision = fixture.commit("def answer := 1\n", "old");
    let new_revision = fixture.commit("def answer := 2\n", "new");
    let old_manifest = fixture.registry_manifest(&old_revision, "v1.0.0");
    let new_manifest = fixture.registry_manifest(&new_revision, "v2.0.0");
    fs::write(fixture.project.join("lake-manifest.json"), &old_manifest).unwrap();
    fs::write(
        fixture.project.join("lev.toml"),
        format!("[constraints]\ndep = \"{old_revision}\"\n"),
    )
    .unwrap();

    assert_success(fixture.lev().arg("sync").output().unwrap());
    let lock = fs::read(fixture.project.join("lev.lock")).unwrap();
    let replacement = fixture.root.join("constraint-violating-manifest.json");
    fs::write(&replacement, new_manifest).unwrap();

    let rejected = fixture
        .lev()
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &replacement)
        .arg("sync")
        .arg("--update")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("constraint for dep"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        old_manifest
    );
    assert_eq!(fs::read(fixture.project.join("lev.lock")).unwrap(), lock);
}
