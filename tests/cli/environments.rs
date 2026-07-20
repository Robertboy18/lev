use super::*;

fn native_lock(fixture: &Fixture, registry_api: Option<&str>) -> Output {
    let mut command = fixture.lev();
    if let Some(registry_api) = registry_api {
        command.env("LEV_RESERVOIR_API_URL", registry_api);
    }
    command
        .env("LEV_TEST_LAKE_UPDATE_FAIL", "1")
        .args(["--verbose", "lock", "--lean", "4.fixture-d"])
        .output()
        .unwrap()
}

#[test]
fn versioned_lock_switches_dependency_graphs_without_mutating_source_files() {
    let fixture = Fixture::new();
    let old_revision = fixture.commit("def answer := 1\n", "old");
    let new_revision = fixture.commit("def answer := 2\n", "new");
    let old_manifest = fixture.registry_manifest(&old_revision, "v1.0.0");
    let new_manifest = fixture.registry_manifest(&new_revision, "v2.0.0");
    let lakefile = r#"[package]
name = "root"

[[require]]
name = "dep"
scope = "test-owner"
rev = "v1.0.0"
"#;
    fs::write(fixture.project.join("lakefile.toml"), lakefile).unwrap();
    fs::write(fixture.project.join("lake-manifest.json"), &old_manifest).unwrap();
    let replacement = fixture.root.join("alternate-manifest.json");
    fs::write(&replacement, &new_manifest).unwrap();

    let (api, server) = reservoir_server_for_toolchain(
        &old_revision,
        &new_revision,
        "leanprover/lean4:v4.fixture-d",
    );
    let locked = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &replacement)
        .arg("lock")
        .arg("--lean")
        .arg("4.fixture-d")
        .output()
        .unwrap();
    assert_success_ref(&locked);
    server.join().unwrap();

    assert_eq!(
        fs::read_to_string(fixture.project.join("lean-toolchain")).unwrap(),
        "leanprover/lean4:v4.test\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        lakefile
    );
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        old_manifest
    );
    let source_lock = fs::read(fixture.project.join("lev.lock")).unwrap();
    let rendered_lock = String::from_utf8_lossy(&source_lock);
    assert!(
        rendered_lock.contains("leanprover/lean4:v4.fixture-d"),
        "{rendered_lock}"
    );
    assert!(rendered_lock.contains("v2.0.0"), "{rendered_lock}");

    fs::write(&fixture.log, "").unwrap();
    for _ in 0..2 {
        assert_success(
            fixture
                .lev()
                .arg("build")
                .arg("--lean")
                .arg("4.fixture-d")
                .arg("--offline")
                .output()
                .unwrap(),
        );
    }
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!log.contains("lake-update"), "{log}");
    let builds = log
        .lines()
        .filter_map(|line| line.strip_prefix("lake-build\t"))
        .map(|line| line.split('\t').next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(builds.len(), 2, "{log}");
    assert_eq!(builds[0], builds[1], "{log}");
    let workspace = Path::new(builds[0]);
    assert!(workspace.starts_with(fixture.cache.join("workspaces-v1")));
    assert_eq!(
        fs::read_to_string(workspace.join("lean-toolchain")).unwrap(),
        "leanprover/lean4:v4.fixture-d\n"
    );
    assert!(
        fs::read_to_string(workspace.join("lakefile.toml"))
            .unwrap()
            .contains(r#"rev = "v2.0.0""#)
    );
    assert_eq!(
        fs::read(workspace.join("lake-manifest.json")).unwrap(),
        new_manifest
    );
    assert_eq!(
        fs::read(fixture.project.join("lev.lock")).unwrap(),
        source_lock,
        "executing a locked environment must not rewrite the source lock"
    );

    assert_success(
        fixture
            .lev()
            .arg("lock")
            .arg("--check")
            .arg("--lean")
            .arg("v4.fixture-d")
            .output()
            .unwrap(),
    );

    assert_success(
        fixture
            .lev()
            .arg("use")
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
        fs::read_to_string(fixture.project.join("lakefile.toml"))
            .unwrap()
            .contains(r#"rev = "v2.0.0""#)
    );
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        new_manifest
    );
    assert_success(fixture.lev().arg("lock").arg("--check").output().unwrap());
}

#[test]
fn versioned_lock_resolves_a_committed_registry_graph_without_lake_update() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    let old_manifest = fixture.registry_manifest(&graph.old_revision, "v1.0.0");
    fs::write(fixture.project.join("lake-manifest.json"), &old_manifest).unwrap();
    let source_lakefile = r#"[package]
name = "root"

[[require]]
name = "dep"
scope = "test-owner"
rev = "v1.0.0"
"#;
    fs::write(fixture.project.join("lakefile.toml"), source_lakefile).unwrap();

    let (api, server) = complete_registry_server(&graph);
    let output = native_lock(&fixture, Some(&api));
    assert_success_ref(&output);
    server.join().unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("prefetching 2 exact dependencies before Lake resolution"),
        "{stderr}"
    );
    assert!(
        stderr.contains("resolved exact dependency graph without a Lake update"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!log.contains("lake-env"), "{log}");
    assert!(!log.contains("lake-update"), "{log}");

    let dependency_environment = fs::read_dir(fixture.cache.join("dependencies-v1"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        git_output(
            &dependency_environment.join("packages/child"),
            &["rev-parse", "HEAD"]
        ),
        graph.child_revision
    );
    assert_eq!(
        git_output(
            &dependency_environment.join("packages/child"),
            &["remote", "get-url", "origin"]
        ),
        graph.child_remote.to_string_lossy()
    );
    assert_eq!(
        git_output(
            &dependency_environment.join("packages/child"),
            &["config", "--get-all", "remote.origin.fetch"]
        ),
        "+refs/heads/*:refs/remotes/origin/*"
    );
    let lock = fs::read_to_string(fixture.project.join("lev.lock")).unwrap();
    assert!(lock.contains(&graph.new_revision), "{lock}");
    assert!(lock.contains(&graph.child_revision), "{lock}");
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.toml")).unwrap(),
        source_lakefile
    );
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        old_manifest
    );

    assert_success(
        fixture
            .lev()
            .arg("build")
            .arg("--lean")
            .arg("4.fixture-d")
            .arg("--offline")
            .output()
            .unwrap(),
    );
}

#[test]
fn native_resolution_rejects_a_declaration_missing_from_the_starting_manifest() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    fs::write(
        fixture.project.join("lake-manifest.json"),
        fixture.registry_manifest(&graph.old_revision, "v1.0.0"),
    )
    .unwrap();
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
name = "new-root"
git = "{}"
rev = "{}"
"#,
            fixture.remote.display(),
            graph.old_revision
        ),
    )
    .unwrap();

    assert_versioned_resolution_delegates_to_lake(&fixture, &graph);
}

#[test]
fn native_resolution_rejects_a_manifest_root_removed_from_the_lakefile() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    fs::write(
        fixture.project.join("lake-manifest.json"),
        registry_manifest_with_extra_root(&fixture, &graph, "removed-root"),
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

    assert_versioned_resolution_delegates_to_lake(&fixture, &graph);
}

#[test]
fn native_resolution_merges_compatible_multi_root_graphs_without_lake() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    let source_manifest = write_multi_registry_project(&fixture, &graph);

    let (api, server) = complete_multi_registry_server(&graph);
    let output = native_lock(&fixture, Some(&api));
    server.join().unwrap();
    assert_success_ref(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("prefetching 3 exact dependencies before Lake resolution"),
        "{stderr}"
    );
    assert!(
        stderr.contains("resolved exact dependency graph without a Lake update"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!log.contains("lake-update"), "{log}");

    let lock = fs::read_to_string(fixture.project.join("lev.lock")).unwrap();
    assert!(lock.contains(&graph.new_revision), "{lock}");
    assert!(lock.contains(&graph.second_new_revision), "{lock}");
    assert!(lock.contains(&graph.child_revision), "{lock}");
    assert_eq!(lock.matches("name = \"child\"").count(), 1, "{lock}");
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        source_manifest
    );

    let dependency_environment = fs::read_dir(fixture.cache.join("dependencies-v1"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        git_output(
            &dependency_environment.join("packages/dep"),
            &["rev-parse", "HEAD"]
        ),
        graph.new_revision
    );
    assert_eq!(
        git_output(
            &dependency_environment.join("packages/second-root"),
            &["rev-parse", "HEAD"]
        ),
        graph.second_new_revision
    );
    assert_eq!(
        git_output(
            &dependency_environment.join("packages/child"),
            &["rev-parse", "HEAD"]
        ),
        graph.child_revision
    );
}

#[test]
fn native_resolution_merges_indexed_and_exact_git_roots() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    let source_manifest =
        write_mixed_registry_project(&fixture, &graph, &graph.second_new_revision);

    let (api, server) = complete_registry_server(&graph);
    let output = native_lock(&fixture, Some(&api));
    server.join().unwrap();
    assert_success_ref(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("preserving exact commit pin for second-root"),
        "{stderr}"
    );
    assert!(
        stderr.contains("resolved exact dependency graph without a Lake update"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!log.contains("lake-update"), "{log}");

    let lock = fs::read_to_string(fixture.project.join("lev.lock")).unwrap();
    assert!(lock.contains(&graph.new_revision), "{lock}");
    assert!(lock.contains(&graph.second_new_revision), "{lock}");
    assert_eq!(lock.matches("name = \"child\"").count(), 1, "{lock}");
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        source_manifest
    );
}

#[test]
fn native_resolution_supports_git_packages_rooted_in_subdirectories() {
    let fixture = Fixture::new();
    let mut graph = complete_registry_graph(&fixture);
    move_registry_root_into_subdirectory(&fixture, &mut graph);

    let mut source_manifest: serde_json::Value =
        serde_json::from_slice(&fixture.registry_manifest(&graph.old_revision, "v1.0.0")).unwrap();
    source_manifest["packages"][0]["subDir"] = json!("packages/dep");
    source_manifest["packages"][0]["manifestFile"] = json!("lake-manifest.json");
    source_manifest["packages"][0]["configFile"] = json!("lakefile.toml");
    let source_manifest = serde_json::to_vec_pretty(&source_manifest).unwrap();
    fs::write(fixture.project.join("lake-manifest.json"), &source_manifest).unwrap();
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

    let (api, server) = complete_registry_server(&graph);
    let output = native_lock(&fixture, Some(&api));
    server.join().unwrap();
    assert_success_ref(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolved exact dependency graph without a Lake update"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!log.contains("lake-update"), "{log}");

    let dependency_environment = fs::read_dir(fixture.cache.join("dependencies-v1"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let checkout = dependency_environment.join("packages/dep");
    assert_eq!(
        git_output(&checkout, &["rev-parse", "HEAD"]),
        graph.new_revision
    );
    assert!(checkout.join("packages/dep/lakefile.toml").is_file());
    assert!(checkout.join("packages/dep/lake-manifest.json").is_file());

    let lock = fs::read_to_string(fixture.project.join("lev.lock")).unwrap();
    assert!(lock.contains("packages/dep"), "{lock}");
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        source_manifest
    );
}

#[test]
fn native_resolution_preserves_an_unindexed_moving_root_at_its_locked_commit() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    let source_manifest = write_mixed_registry_project(&fixture, &graph, "main");

    let (api, server) = complete_registry_server(&graph);
    let output = native_lock(&fixture, Some(&api));
    server.join().unwrap();
    assert_success_ref(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("preserving unindexed dependency second-root"),
        "{stderr}"
    );
    assert!(
        stderr.contains("resolved exact dependency graph without a Lake update"),
        "{stderr}"
    );
    let lock = fs::read_to_string(fixture.project.join("lev.lock")).unwrap();
    assert!(lock.contains(&graph.second_new_revision), "{lock}");
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        source_manifest
    );
}

#[test]
fn native_resolution_delegates_conflicting_multi_root_manifests_to_lake() {
    let fixture = Fixture::new();
    let mut graph = complete_registry_graph(&fixture);
    make_second_root_manifest_conflict(&mut graph);
    write_multi_registry_project(&fixture, &graph);

    let (api, server) = complete_multi_registry_server(&graph);
    let output = native_lock(&fixture, Some(&api));
    server.join().unwrap();
    assert_failed_resolution_reached_lake(&fixture, &output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("package child disagrees"), "{stderr}");
}

#[test]
fn native_resolution_delegates_explicit_dependency_overrides_to_lake() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 1\n", "initial");
    fs::write(
        fixture.project.join("lake-manifest.json"),
        fixture.registry_manifest(&revision, "v1.0.0"),
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
    fs::write(
        fixture.project.join("lev.toml"),
        r#"[environments."4.fixture-d".dependencies]
dep = "custom-release"
"#,
    )
    .unwrap();

    let output = native_lock(&fixture, None);
    assert_failed_resolution_reached_lake(&fixture, &output);
}

#[test]
fn native_resolution_accepts_exact_explicit_dependency_overrides() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    let source_manifest = fixture.registry_manifest(&graph.old_revision, "v1.0.0");
    fs::write(fixture.project.join("lake-manifest.json"), &source_manifest).unwrap();
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
    fs::write(
        fixture.project.join("lev.toml"),
        format!(
            r#"[environments."4.fixture-d"]
auto = false

[environments."4.fixture-d".dependencies]
dep = "{}"
"#,
            graph.new_revision
        ),
    )
    .unwrap();

    let output = native_lock(&fixture, Some("http://127.0.0.1:9"));
    assert_success_ref(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("using exact override for dep"), "{stderr}");
    assert!(
        stderr.contains("resolved exact dependency graph without a Lake update"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!log.contains("lake-update"), "{log}");

    let lock = fs::read_to_string(fixture.project.join("lev.lock")).unwrap();
    assert!(lock.contains(&graph.new_revision), "{lock}");
    assert!(lock.contains(&graph.child_revision), "{lock}");
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        source_manifest
    );
}

#[test]
fn native_resolution_rejects_a_registry_tag_pointing_at_another_commit() {
    let fixture = Fixture::new();
    let graph = complete_registry_graph(&fixture);
    git(
        &fixture.remote,
        &["tag", "--force", "v2.0.0", &graph.old_revision],
    );
    assert_ne!(graph.old_revision, graph.new_revision);
    assert_eq!(
        git_output(&fixture.remote, &["rev-parse", "refs/tags/v2.0.0^{commit}"]),
        graph.old_revision
    );
    fs::write(
        fixture.project.join("lake-manifest.json"),
        fixture.registry_manifest(&graph.old_revision, "v1.0.0"),
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

    assert_versioned_resolution_delegates_to_lake(&fixture, &graph);
}

#[test]
fn resolution_cache_reuses_an_identical_graph_offline_and_repairs_corruption_online() {
    let fixture = Fixture::new();
    let old_revision = fixture.commit("def answer := 1\n", "old");
    let new_revision = fixture.commit("def answer := 2\n", "new");
    let old_manifest = fixture.registry_manifest(&old_revision, "v1.0.0");
    let new_manifest = fixture.registry_manifest(&new_revision, "v2.0.0");
    let lakefile = r#"[package]
name = "root"

[[require]]
name = "dep"
scope = "test-owner"
rev = "v1.0.0"
"#;
    fs::write(fixture.project.join("lakefile.toml"), lakefile).unwrap();
    fs::write(fixture.project.join("lake-manifest.json"), &old_manifest).unwrap();

    let second = fixture.root.join("second-project");
    let third = fixture.root.join("third-project");
    for project in [&second, &third] {
        fs::create_dir_all(project).unwrap();
        fs::copy(
            fixture.project.join("lean-toolchain"),
            project.join("lean-toolchain"),
        )
        .unwrap();
        fs::copy(
            fixture.project.join("lakefile.toml"),
            project.join("lakefile.toml"),
        )
        .unwrap();
        fs::copy(
            fixture.project.join("lake-manifest.json"),
            project.join("lake-manifest.json"),
        )
        .unwrap();
    }
    let replacement = fixture.root.join("alternate-manifest.json");
    fs::write(&replacement, &new_manifest).unwrap();

    let (api, server) = reservoir_server_for_toolchain(
        &old_revision,
        &new_revision,
        "leanprover/lean4:v4.fixture-d",
    );
    let first = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &replacement)
        .env("LEV_TEST_LAKE_UPDATE_CLONE_URL", &fixture.remote)
        .env("LEV_TEST_LAKE_UPDATE_CLONE_REV", &new_revision)
        .arg("--verbose")
        .arg("lock")
        .arg("--lean")
        .arg("4.fixture-d")
        .output()
        .unwrap();
    assert_success_ref(&first);
    server.join().unwrap();
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_stderr.contains("stored cross-project resolution"),
        "{first_stderr}"
    );
    assert!(
        first_stderr.contains("deferred 1 redundant Git mirror creation"),
        "{first_stderr}"
    );
    assert!(
        fs::read_dir(fixture.cache.join("git-v1"))
            .unwrap()
            .next()
            .is_none(),
        "Lake's complete managed checkout should not be duplicated as a bare mirror"
    );

    let unavailable_remote = fixture.root.join("remote-unavailable");
    fs::rename(&fixture.remote, &unavailable_remote).unwrap();
    fs::write(&fixture.log, "").unwrap();
    let reused = fixture
        .lev_at(&second)
        .arg("--verbose")
        .arg("lock")
        .arg("--offline")
        .arg("--lean")
        .arg("4.fixture-d")
        .output()
        .unwrap();
    assert_success_ref(&reused);
    let reused_stderr = String::from_utf8_lossy(&reused.stderr);
    assert!(
        reused_stderr.contains("reusing cross-project resolution"),
        "{reused_stderr}"
    );
    assert!(
        reused_stderr.contains("reused shared leanprover/lean4:v4.fixture-d dependency resolution"),
        "{reused_stderr}"
    );
    assert!(
        !fs::read_to_string(&fixture.log)
            .unwrap()
            .contains("lake-update"),
        "an offline resolution-cache hit must not invoke Lake"
    );
    assert_success(
        fixture
            .lev_at(&second)
            .arg("build")
            .arg("--lean")
            .arg("4.fixture-d")
            .arg("--offline")
            .output()
            .unwrap(),
    );

    let resolution_path = fs::read_dir(fixture.cache.join("resolutions-v1"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&resolution_path, "{not valid JSON").unwrap();
    let corrupt_offline = fixture
        .lev_at(&third)
        .arg("lock")
        .arg("--offline")
        .arg("--lean")
        .arg("4.fixture-d")
        .output()
        .unwrap();
    assert!(!corrupt_offline.status.success());
    assert!(
        String::from_utf8_lossy(&corrupt_offline.stderr)
            .contains("cannot be replaced in offline mode"),
        "{}",
        String::from_utf8_lossy(&corrupt_offline.stderr)
    );

    fs::write(&fixture.log, "").unwrap();
    let repaired = fixture
        .lev_at(&third)
        .env("LEV_RESERVOIR_API_URL", &api)
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &replacement)
        .arg("--verbose")
        .arg("lock")
        .arg("--lean")
        .arg("4.fixture-d")
        .output()
        .unwrap();
    assert_success_ref(&repaired);
    assert!(
        String::from_utf8_lossy(&repaired.stderr).contains("discarding invalid cached resolution"),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .contains("lake-update"),
        "online recovery must ask Lake for a replacement resolution"
    );
    let verified = fixture.lev().arg("cache").arg("verify").output().unwrap();
    assert_success_ref(&verified);
    assert!(
        String::from_utf8_lossy(&verified.stdout).contains("verified dependency resolutions: 1"),
        "{}",
        String::from_utf8_lossy(&verified.stdout)
    );
}

#[test]
fn versioned_lock_resolution_rolls_back_and_offline_never_guesses() {
    let fixture = Fixture::new();
    let old_revision = fixture.commit("def answer := 1\n", "old");
    let new_revision = fixture.commit("def answer := 2\n", "new");
    let old_manifest = fixture.registry_manifest(&old_revision, "v1.0.0");
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
    assert_success(fixture.lev().arg("lock").output().unwrap());
    let original_lock = fs::read(fixture.project.join("lev.lock")).unwrap();
    let original_config = fs::read(fixture.project.join("lakefile.toml")).unwrap();

    let missing = fixture
        .lev()
        .arg("build")
        .arg("--lean")
        .arg("4.fixture-b")
        .arg("--offline")
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("offline mode"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(!fixture.cache.join("workspaces-v1").exists());

    let (api, server) = reservoir_server_for_toolchain(
        &old_revision,
        &new_revision,
        "leanprover/lean4:v4.fixture-b",
    );
    let failed = fixture
        .lev()
        .env("LEV_RESERVOIR_API_URL", &api)
        .env("LEV_TEST_LAKE_UPDATE_FAIL", "1")
        .arg("lock")
        .arg("--lean")
        .arg("4.fixture-b")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("simulated Lake update failure"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    server.join().unwrap();
    assert_eq!(
        fs::read(fixture.project.join("lev.lock")).unwrap(),
        original_lock
    );
    assert_eq!(
        fs::read(fixture.project.join("lakefile.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(fixture.project.join("lake-manifest.json")).unwrap(),
        old_manifest
    );
}

#[test]
fn executable_lakefiles_require_an_explicit_versioned_alternative() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.project.join("lakefile.toml")).unwrap();
    let source_lakefile = "import Lake\nopen Lake DSL\npackage root\n";
    fs::write(fixture.project.join("lakefile.lean"), source_lakefile).unwrap();
    fs::write(
        fixture.project.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [],
            "name": "root"
        }))
        .unwrap(),
    )
    .unwrap();

    let rejected = fixture
        .lev()
        .arg("lock")
        .arg("--lean")
        .arg("4.fixture-b")
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("cannot safely resolve"), "{stderr}");
    assert!(stderr.contains("complete alternate Lakefile"), "{stderr}");

    fs::create_dir_all(fixture.project.join("compat")).unwrap();
    fs::write(
        fixture.project.join("compat/alternate-lakefile.lean"),
        "import Lake\nopen Lake DSL\n-- compatible-alternate\npackage root\n",
    )
    .unwrap();
    fs::write(
        fixture.project.join("lev.toml"),
        r#"[environments."4.fixture-b"]
lakefile = "compat/alternate-lakefile.lean"
"#,
    )
    .unwrap();
    assert_success(
        fixture
            .lev()
            .arg("lock")
            .arg("--lean")
            .arg("4.fixture-b")
            .output()
            .unwrap(),
    );
    assert!(
        !fixture.cache.join("resolutions-v1").exists(),
        "executable Lakefiles must not produce cross-project resolution records"
    );

    let marker = fixture.root.join("alternate-used");
    assert_success(
        fixture
            .lev()
            .env("LEV_TEST_ALTERNATE_MARKER", &marker)
            .arg("run")
            .arg("--lean")
            .arg("4.fixture-b")
            .arg("--no-sync")
            .arg("--offline")
            .arg("sh")
            .arg("-c")
            .arg(
                "grep -q compatible-alternate lakefile.lean && \
                 printf used > \"$LEV_TEST_ALTERNATE_MARKER\"",
            )
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_to_string(marker).unwrap(), "used");
    assert_eq!(
        fs::read_to_string(fixture.project.join("lakefile.lean")).unwrap(),
        source_lakefile
    );
}
