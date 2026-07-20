use super::*;

#[test]
fn elan_backend_accepts_future_shorthands_and_arbitrary_full_identifiers() {
    let fixture = Fixture::new();
    let cases = [
        ("42.preview", "leanprover/lean4:v42.preview"),
        (
            "vendor/toolchain:future-channel",
            "vendor/toolchain:future-channel",
        ),
    ];

    for (input, expected) in cases {
        let installed = fixture
            .lev()
            .arg("toolchain")
            .arg("install")
            .arg(input)
            .arg("--backend")
            .arg("elan")
            .output()
            .unwrap();
        assert_success_ref(&installed);

        let state = fs::read_to_string(fixture.root.join("elan-state/installed")).unwrap();
        assert!(state.lines().any(|line| line == expected), "{state}");
    }

    let log = fs::read_to_string(&fixture.log).unwrap();
    for (_, expected) in cases {
        assert!(
            log.contains(&format!("run --install {expected} lean --version")),
            "{log}"
        );
    }
}

#[test]
fn signed_chunked_toolchain_publishes_installs_and_links_through_the_cli() {
    let fixture = Fixture::new();
    let source = fixture.root.join("chunk-toolchain");
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::create_dir_all(source.join("lib")).unwrap();
    fs::write(source.join("bin/lean"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::write(source.join("lib/runtime"), "shared runtime bytes").unwrap();
    let mut permissions = fs::metadata(source.join("bin/lean")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(source.join("bin/lean"), permissions).unwrap();

    let private_key = fixture.root.join("chunks.key");
    let public_key = fixture.root.join("chunks.pub");
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
    let remote = fixture.root.join("toolchain-chunks");
    let publish = fixture
        .lev()
        .arg("toolchain")
        .arg("chunks")
        .arg("publish")
        .arg("4.fixture-release")
        .arg(&remote)
        .arg("--source")
        .arg(&source)
        .arg("--signing-key")
        .arg(&private_key)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&publish);
    let publish = json_data(&publish, "lev.cli.toolchain.chunks.publish/v1");
    assert_eq!(publish["files"], 2);
    assert_eq!(publish["chunks_uploaded"], 2);

    let index = fixture
        .lev()
        .arg("toolchain")
        .arg("index")
        .arg("build")
        .arg(&remote)
        .arg("--signing-key")
        .arg(&private_key)
        .arg("--public-key")
        .arg(&public_key)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&index);
    let index = json_data(&index, "lev.cli.toolchain.index.build/v1");
    assert_eq!(index["schema"], "lev.toolchain-index-build/v1");
    assert_eq!(index["entries"], 1);

    let listed = fixture
        .lev()
        .arg("toolchain")
        .arg("index")
        .arg("list")
        .arg(&remote)
        .arg("--public-key")
        .arg(&public_key)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&listed);
    let listed = json_data(&listed, "lev.cli.toolchain.index.list/v1");
    assert_eq!(listed["schema"], "lev.toolchain-index/v1");
    assert_eq!(
        listed["entries"][0]["toolchain"],
        "leanprover/lean4:v4.fixture-release"
    );

    let verified = fixture
        .lev()
        .arg("toolchain")
        .arg("index")
        .arg("verify")
        .arg(&remote)
        .arg("--public-key")
        .arg(&public_key)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&verified);
    let verified = json_data(&verified, "lev.cli.toolchain.index.verify/v1");
    assert_eq!(verified["entries"], 1);
    assert_eq!(verified["toolchains"], 1);
    assert_eq!(verified["platforms"], 1);

    let install = fixture
        .lev()
        .arg("toolchain")
        .arg("chunks")
        .arg("install")
        .arg("4.fixture-release")
        .arg(&remote)
        .arg("--public-key")
        .arg(&public_key)
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&install);
    let install = json_data(&install, "lev.cli.toolchain.chunks.install/v1");
    let alias = install["imported"]["alias"].as_str().unwrap();
    let view = PathBuf::from(install["imported"]["view"].as_str().unwrap());
    assert!(alias.starts_with("lev-v4.fixture-release-"));
    assert_eq!(
        fs::read_to_string(view.join("lib/runtime")).unwrap(),
        "shared runtime bytes"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("elan-state/links").join(alias))
            .unwrap()
            .trim(),
        view.to_string_lossy()
    );

    // A corrupt or partial store view must not borrow Lake from the host PATH.
    let host_bin = fixture.root.join("host-bin");
    fs::create_dir_all(&host_bin).unwrap();
    fs::write(host_bin.join("lake"), "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(host_bin.join("lake")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(host_bin.join("lake"), permissions).unwrap();
    fs::write(
        fixture.project.join("lean-toolchain"),
        "leanprover/lean4:v4.fixture-release\n",
    )
    .unwrap();
    let missing_lake = fixture
        .lev()
        .env("PATH", &host_bin)
        .arg("build")
        .arg("--no-sync")
        .arg("--offline")
        .output()
        .unwrap();
    assert!(!missing_lake.status.success());
    assert!(String::from_utf8_lossy(&missing_lake.stderr).contains("required executable"));
    fs::write(
        fixture.project.join("lean-toolchain"),
        "leanprover/lean4:v4.test\n",
    )
    .unwrap();

    assert_success(
        fixture
            .lev()
            .arg("toolchain")
            .arg("store")
            .arg("verify")
            .output()
            .unwrap(),
    );

    let second_data = fixture.root.join("second-data");
    let second_elan_state = fixture.root.join("second-elan-state");
    let ordinary_install = fixture
        .lev()
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("chunks")
        .arg("--remote")
        .arg(&remote)
        .arg("--public-key")
        .arg(&public_key)
        .env("LEV_DATA_DIR", &second_data)
        .env("LEV_TEST_ELAN_STATE", &second_elan_state)
        .output()
        .unwrap();
    assert_success_ref(&ordinary_install);
    let links = fs::read_dir(second_elan_state.join("links"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(links.len(), 1);
}

#[test]
fn toolchain_backends_reject_incompatible_or_incomplete_trust_options() {
    let fixture = Fixture::new();

    let elan_unverified = fixture
        .lev()
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("elan")
        .arg("--allow-unverified")
        .output()
        .unwrap();
    assert!(!elan_unverified.status.success());
    assert!(
        String::from_utf8_lossy(&elan_unverified.stderr)
            .contains("--allow-unverified applies only to direct archive installation")
    );

    let elan_remote = fixture
        .lev()
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("elan")
        .arg("--remote")
        .arg(&fixture.remote)
        .output()
        .unwrap();
    assert!(!elan_remote.status.success());
    assert!(
        String::from_utf8_lossy(&elan_remote.stderr)
            .contains("remote chunk options do not apply to the elan backend")
    );

    let direct_remote = fixture
        .lev()
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("direct")
        .arg("--remote")
        .arg(&fixture.remote)
        .output()
        .unwrap();
    assert!(!direct_remote.status.success());
    assert!(
        String::from_utf8_lossy(&direct_remote.stderr)
            .contains("remote chunk options do not apply to the direct backend")
    );

    let chunks_without_remote = fixture
        .lev()
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("chunks")
        .output()
        .unwrap();
    assert!(!chunks_without_remote.status.success());
    assert!(
        String::from_utf8_lossy(&chunks_without_remote.stderr)
            .contains("chunks backend requires --remote")
    );

    let chunks_without_key = fixture
        .lev()
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("chunks")
        .arg("--remote")
        .arg(&fixture.remote)
        .output()
        .unwrap();
    assert!(!chunks_without_key.status.success());
    assert!(
        String::from_utf8_lossy(&chunks_without_key.stderr)
            .contains("chunks backend requires --public-key")
    );

    // v4.test is visible to fake elan, but an explicit chunk request must
    // still enforce its own remote trust configuration.
    let chunks_cannot_borrow_elan = fixture
        .lev()
        .arg("toolchain")
        .arg("install")
        .arg("4.test")
        .arg("--backend")
        .arg("chunks")
        .output()
        .unwrap();
    assert!(!chunks_cannot_borrow_elan.status.success());
    assert!(
        String::from_utf8_lossy(&chunks_cannot_borrow_elan.stderr)
            .contains("chunks backend requires --remote")
    );
}

#[test]
fn direct_toolchain_install_verifies_streams_links_and_executes_the_store() {
    let fixture = Fixture::new();
    let archive = compressed_toolchain_archive();
    let sha256 = format!("{:x}", Sha256::digest(&archive));
    let (api, server) = release_server(archive, &sha256);

    let installed = fixture
        .lev()
        .env("LEV_GITHUB_API_URL", &api)
        .arg("--verbose")
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("direct")
        .output()
        .unwrap();
    assert_success_ref(&installed);
    let stderr = String::from_utf8_lossy(&installed.stderr);
    assert!(stderr.contains("SHA-256"), "{stderr}");
    assert!(stderr.contains("(verified)"), "{stderr}");
    server.join().unwrap();

    // An explicit direct request may reuse only verified archive provenance.
    let reused = fixture
        .lev()
        .env(
            "LEV_GITHUB_API_URL",
            "http://127.0.0.1:9/should-not-be-contacted",
        )
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("direct")
        .output()
        .unwrap();
    assert_success_ref(&reused);

    let alias = format!(
        "lev-v4.fixture-release-{}",
        &format!(
            "{:x}",
            Sha256::digest(b"leanprover/lean4:v4.fixture-release")
        )[..8]
    );
    assert!(fixture.root.join("elan-state/links").join(&alias).is_file());
    assert_success(
        fixture
            .lev()
            .arg("toolchain")
            .arg("store")
            .arg("verify")
            .output()
            .unwrap(),
    );

    fs::write(
        fixture.project.join("lean-toolchain"),
        "leanprover/lean4:v4.fixture-release\n",
    )
    .unwrap();
    let doctor = fixture.lev().arg("doctor").output().unwrap();
    assert_success_ref(&doctor);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains("runtime: lev store ("), "{stdout}");
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(
        !log.contains(&format!("run {alias} lean --version")),
        "store-backed doctor must not route Lean through elan: {log}"
    );
}

#[test]
fn direct_toolchains_install_run_list_and_remove_without_elan() {
    let fixture = Fixture::new();
    let archive = compressed_toolchain_archive();
    let sha256 = format!("{:x}", Sha256::digest(&archive));
    let (api, server) = release_server(archive, &sha256);
    let missing_elan = fixture.root.join("missing-elan");

    let install = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .env("LEV_GITHUB_API_URL", &api)
        .arg("toolchain")
        .arg("install")
        .arg("4.fixture-release")
        .arg("--backend")
        .arg("direct")
        .arg("--pin")
        .output()
        .unwrap();
    assert_success_ref(&install);
    server.join().unwrap();
    assert_eq!(
        fs::read_to_string(fixture.project.join("lean-toolchain"))
            .unwrap()
            .trim(),
        "leanprover/lean4:v4.fixture-release"
    );

    let build = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .arg("build")
        .arg("--no-sync")
        .arg("--offline")
        .output()
        .unwrap();
    assert_success_ref(&build);

    let runtime = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .arg("run")
        .arg("--no-sync")
        .arg("--offline")
        .arg("sh")
        .arg("-c")
        .arg("test -x \"$(command -v lean)\" && test -n \"$LEV_TOOLCHAIN_ROOT\"")
        .output()
        .unwrap();
    assert_success_ref(&runtime);

    let doctor = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .arg("doctor")
        .output()
        .unwrap();
    assert_success_ref(&doctor);
    let doctor = String::from_utf8_lossy(&doctor.stdout);
    assert!(doctor.contains("elan: not installed (optional"), "{doctor}");
    assert!(doctor.contains("runtime: lev store ("), "{doctor}");

    let gc = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .arg("toolchain")
        .arg("gc")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&gc);
    let gc = json_data(&gc, "lev.cli.toolchain.gc/v1");
    assert_eq!(gc["store"]["applied"], false);
    assert!(gc["elan"].is_null());

    let list = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .arg("toolchain")
        .arg("list")
        .output()
        .unwrap();
    assert_success_ref(&list);
    assert!(
        String::from_utf8_lossy(&list.stdout)
            .contains("leanprover/lean4:v4.fixture-release (lev store)")
    );

    let remove = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .arg("toolchain")
        .arg("remove")
        .arg("4.fixture-release")
        .output()
        .unwrap();
    assert_success_ref(&remove);
    let status = fixture
        .lev()
        .env("LEV_ELAN", &missing_elan)
        .arg("toolchain")
        .arg("store")
        .arg("status")
        .output()
        .unwrap();
    assert_success_ref(&status);
    assert!(String::from_utf8_lossy(&status.stdout).contains("manifests: 0"));
}

#[test]
fn toolchain_import_pin_verify_and_gc_work_end_to_end() {
    let fixture = Fixture::new();
    let imported = fixture
        .lev()
        .arg("toolchain")
        .arg("import")
        .arg("leanprover/lean4:v4.test")
        .arg("--pin")
        .output()
        .unwrap();
    assert_success_ref(&imported);

    let pin = fs::read_to_string(fixture.project.join("lean-toolchain"))
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(pin, "leanprover/lean4:v4.test");
    let output = String::from_utf8_lossy(&imported.stdout);
    let alias = output
        .lines()
        .find_map(|line| line.strip_prefix("alias: "))
        .unwrap()
        .to_owned();
    assert!(alias.starts_with("lev-v4.test-"), "{alias}");
    assert!(output.contains("reused objects: 12 B"), "{output}");

    let verify = fixture
        .lev()
        .arg("toolchain")
        .arg("store")
        .arg("verify")
        .output()
        .unwrap();
    assert_success_ref(&verify);
    let output = String::from_utf8_lossy(&verify.stdout);
    assert!(output.contains("verified manifests: 1"), "{output}");
    assert!(output.contains("verified views: 1"), "{output}");
    assert!(output.contains("verified objects: 2"), "{output}");

    let aggregate_gc = fixture
        .lev()
        .arg("toolchain")
        .arg("gc")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&aggregate_gc);
    let aggregate_gc = json_data(&aggregate_gc, "lev.cli.toolchain.gc/v1");
    assert_eq!(aggregate_gc["store"]["manifests"], 0);
    assert_eq!(aggregate_gc["elan"]["unused"], json!([]));

    let live_gc = fixture
        .lev()
        .arg("toolchain")
        .arg("store")
        .arg("gc")
        .output()
        .unwrap();
    assert_success_ref(&live_gc);
    let output = String::from_utf8_lossy(&live_gc.stdout);
    assert!(output.contains("unreferenced manifests: 0"), "{output}");
    assert!(output.contains("unreferenced views: 0"), "{output}");
    assert!(output.contains("unreferenced objects: 0"), "{output}");

    assert_success(
        fixture
            .lev()
            .arg("toolchain")
            .arg("remove")
            .arg(&alias)
            .output()
            .unwrap(),
    );
    let stale_gc = fixture
        .lev()
        .arg("toolchain")
        .arg("store")
        .arg("gc")
        .output()
        .unwrap();
    assert_success_ref(&stale_gc);
    let output = String::from_utf8_lossy(&stale_gc.stdout);
    assert!(output.contains("unreferenced manifests: 0"), "{output}");
    assert!(output.contains("unreferenced views: 0"), "{output}");
    assert!(output.contains("unreferenced objects: 0"), "{output}");

    assert_success(
        fixture
            .lev()
            .arg("toolchain")
            .arg("store")
            .arg("gc")
            .arg("--apply")
            .output()
            .unwrap(),
    );
    let status = fixture
        .lev()
        .arg("toolchain")
        .arg("store")
        .arg("status")
        .output()
        .unwrap();
    assert_success_ref(&status);
    let output = String::from_utf8_lossy(&status.stdout);
    assert!(output.contains("manifests: 0"), "{output}");
    assert!(output.contains("views: 0"), "{output}");
    assert!(output.contains("objects: 0"), "{output}");
}
