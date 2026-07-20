use super::*;

#[test]
fn authenticated_remote_cache_round_trips_through_the_cli() {
    let fixture = Fixture::new();
    let toolchain = "leanprover/lean4:v4.test";
    let lake = lake_cache_path(&fixture.cache, toolchain);
    fs::create_dir_all(lake.join("artifacts")).unwrap();
    fs::create_dir_all(lake.join("outputs/root")).unwrap();
    fs::write(lake.join("artifacts/0123456789abcdef.olean"), "olean").unwrap();
    fs::write(
        lake.join("outputs/root/1111111111111111.json"),
        r#"{"schemaVersion":"2026-02-25","service":null,
            "data":"0123456789abcdef.olean"}"#,
    )
    .unwrap();

    let remote = fixture.root.join("artifact-remote");
    let private_key = fixture.root.join("remote-cache.key");
    let public_key = fixture.root.join("remote-cache.pub");
    let keygen = fixture
        .lev()
        .arg("cache")
        .arg("remote")
        .arg("keygen")
        .arg("--private-key")
        .arg(&private_key)
        .arg("--public-key")
        .arg(&public_key)
        .output()
        .unwrap();
    assert_success_ref(&keygen);
    assert_eq!(
        fs::metadata(&private_key).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let push = fixture
        .lev()
        .arg("cache")
        .arg("remote")
        .arg("push")
        .arg(&remote)
        .arg("--signing-key")
        .arg(&private_key)
        .arg("--namespace")
        .arg("tests/project")
        .arg("--revision")
        .arg("revision-1")
        .arg("--toolchain")
        .arg(toolchain)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&push);
    let push = json_data(&push, "lev.cli.cache.remote.push/v1");
    assert_eq!(push["artifacts"], 1);
    assert_eq!(push["mappings"], 1);
    assert_eq!(push["blobs_uploaded"], 2);

    fs::remove_dir_all(&lake).unwrap();
    let pull = fixture
        .lev()
        .arg("cache")
        .arg("remote")
        .arg("pull")
        .arg(&remote)
        .arg("--public-key")
        .arg(&public_key)
        .arg("--namespace")
        .arg("tests/project")
        .arg("--revision")
        .arg("revision-1")
        .arg("--toolchain")
        .arg(toolchain)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&pull);
    let pull = json_data(&pull, "lev.cli.cache.remote.pull/v1");
    assert_eq!(pull["files_created"], 2);
    assert_eq!(
        fs::read_to_string(lake.join("artifacts/0123456789abcdef.olean")).unwrap(),
        "olean"
    );
    assert!(lake.join("outputs/root/1111111111111111.json").is_file());

    let repeat = fixture
        .lev()
        .arg("cache")
        .arg("remote")
        .arg("pull")
        .arg(&remote)
        .arg("--public-key")
        .arg(&public_key)
        .arg("--namespace")
        .arg("tests/project")
        .arg("--revision")
        .arg("revision-1")
        .arg("--toolchain")
        .arg(toolchain)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&repeat);
    let repeat = json_data(&repeat, "lev.cli.cache.remote.pull/v1");
    assert_eq!(repeat["files_created"], 0);
    assert_eq!(repeat["files_reused"], 2);
    assert_success(
        fixture
            .lev()
            .arg("cache")
            .arg("artifacts")
            .arg("verify")
            .arg("--toolchain")
            .arg(toolchain)
            .output()
            .unwrap(),
    );
}

#[test]
fn legacy_lake_artifact_cache_verifies_and_collects_through_the_cli() {
    let fixture = Fixture::new();
    let toolchain = "leanprover/lean4:v4.test";
    let lake = lake_cache_path(&fixture.cache, toolchain);
    fs::create_dir_all(lake.join("artifacts")).unwrap();
    fs::create_dir_all(lake.join("inputs")).unwrap();
    for name in [
        "14550171264454906567.olean",
        "3453551376365123793.ilean",
        "940372859017955812.c",
        "999.art",
    ] {
        fs::write(lake.join("artifacts").join(name), name).unwrap();
    }
    fs::write(
        lake.join("inputs/root.jsonl"),
        concat!(
            "[\"6147500214486957215\",",
            "{\"o\":[\"14550171264454906567\"],",
            "\"i\":\"3453551376365123793\",",
            "\"c\":\"940372859017955812\"}]\n"
        ),
    )
    .unwrap();

    let status = fixture
        .lev()
        .arg("cache")
        .arg("artifacts")
        .arg("status")
        .arg("--toolchain")
        .arg(toolchain)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&status);
    let status = json_data(&status, "lev.cli.cache.artifacts.status/v1");
    assert_eq!(status["stats"]["artifacts"], 4);
    assert_eq!(status["stats"]["mappings"], 1);
    assert_eq!(status["stats"]["referenced_artifacts"], 3);
    assert_eq!(status["stats"]["unreferenced_artifacts"], 1);

    assert_success(
        fixture
            .lev()
            .arg("cache")
            .arg("artifacts")
            .arg("verify")
            .arg("--toolchain")
            .arg(toolchain)
            .output()
            .unwrap(),
    );
    assert_success(
        fixture
            .lev()
            .arg("cache")
            .arg("artifacts")
            .arg("gc")
            .arg("--toolchain")
            .arg(toolchain)
            .arg("--max-age-days")
            .arg("0")
            .arg("--apply")
            .output()
            .unwrap(),
    );

    assert!(!lake.join("artifacts/999.art").exists());
    for name in [
        "14550171264454906567.olean",
        "3453551376365123793.ilean",
        "940372859017955812.c",
    ] {
        assert!(lake.join("artifacts").join(name).is_file(), "{name}");
    }
}

#[test]
fn authenticated_remote_cache_uses_http_object_transport_and_bearer_token() {
    let fixture = Fixture::new();
    let toolchain = "leanprover/lean4:v4.test";
    let lake = lake_cache_path(&fixture.cache, toolchain);
    fs::create_dir_all(lake.join("artifacts")).unwrap();
    fs::create_dir_all(lake.join("outputs/root")).unwrap();
    fs::write(lake.join("artifacts/0123456789abcdef.olean"), "olean").unwrap();
    fs::write(
        lake.join("outputs/root/1111111111111111.json"),
        r#"{"schemaVersion":"2026-02-25","service":null,
            "data":"0123456789abcdef.olean"}"#,
    )
    .unwrap();

    let private_key = fixture.root.join("http-cache.key");
    let public_key = fixture.root.join("http-cache.pub");
    assert_success(
        fixture
            .lev()
            .arg("cache")
            .arg("remote")
            .arg("keygen")
            .arg("--private-key")
            .arg(&private_key)
            .arg("--public-key")
            .arg(&public_key)
            .output()
            .unwrap(),
    );
    let server = TestObjectServer::start("secret-token");

    let push = fixture
        .lev()
        .arg("cache")
        .arg("remote")
        .arg("push")
        .arg(&server.base)
        .arg("--signing-key")
        .arg(&private_key)
        .arg("--namespace")
        .arg("tests/http")
        .arg("--revision")
        .arg("revision-1")
        .arg("--toolchain")
        .arg(toolchain)
        .env("LEV_REMOTE_TOKEN", "secret-token")
        .output()
        .unwrap();
    assert_success_ref(&push);

    fs::remove_dir_all(&lake).unwrap();
    let pull = fixture
        .lev()
        .arg("cache")
        .arg("remote")
        .arg("pull")
        .arg(&server.base)
        .arg("--public-key")
        .arg(&public_key)
        .arg("--namespace")
        .arg("tests/http")
        .arg("--revision")
        .arg("revision-1")
        .arg("--toolchain")
        .arg(toolchain)
        .env("LEV_REMOTE_TOKEN", "secret-token")
        .output()
        .unwrap();
    assert_success_ref(&pull);
    assert_eq!(
        fs::read_to_string(lake.join("artifacts/0123456789abcdef.olean")).unwrap(),
        "olean"
    );
    assert!(lake.join("outputs/root/1111111111111111.json").is_file());

    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().any(|request| request.method == "PUT"));
    assert!(requests.iter().any(|request| request.method == "GET"));
    assert!(
        requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer secret-token"))
    );
}

#[test]
fn local_sync_publishes_locks_and_attaches_shared_dependencies() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "first");
    fixture.write_manifest(&revision);
    let manifest = fs::read(fixture.project.join("lake-manifest.json")).unwrap();
    let resolved = fixture.root.join("resolved-manifest.json");
    fs::write(&resolved, &manifest).unwrap();
    fs::remove_file(fixture.project.join("lake-manifest.json")).unwrap();

    let output = fixture
        .lev()
        .arg("--local")
        .arg("--verbose")
        .arg("sync")
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &resolved)
        .output()
        .unwrap();
    assert_success_ref(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("shared dependency environment"), "{stderr}");
    assert!(stderr.contains("published lake-manifest.json"), "{stderr}");
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        manifest
    );
    assert!(fixture.project.join("lev.lock").is_file());
    assert_success(fixture.lev().arg("lock").arg("--check").output().unwrap());

    let workspaces = workspace_projects(&fixture.cache);
    assert_eq!(workspaces.len(), 1, "{workspaces:?}");
    let packages = workspaces[0].join(".lake/packages");
    assert!(
        fs::symlink_metadata(&packages)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        git_output(&packages.join("dep"), &["rev-parse", "HEAD"]),
        revision
    );

    let status = fixture.lev().arg("cache").arg("status").output().unwrap();
    assert_success_ref(&status);
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("shared dependency environments: 1"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
}
