use super::*;

#[test]
fn checked_in_release_surfaces_do_not_pin_concrete_lean_versions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();

    for directory in ["src", "scripts", "schemas", ".github"] {
        collect_text_files(&root.join(directory), &mut files);
    }
    files.extend(
        ["README.md", "CONTRIBUTING.md", "SECURITY.md"]
            .into_iter()
            .map(|name| root.join(name))
            .filter(|path| path.is_file()),
    );
    files.sort();
    files.dedup();

    for path in files {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        assert!(
            !contains_concrete_lean_release(&source),
            "{} contains a concrete Lean release",
            path.display()
        );
    }

    let harness = root.join("scripts/test-lean-matrix.sh");
    if harness.is_file() {
        let source = fs::read_to_string(harness).unwrap();
        assert!(source.contains("LEV_MATRIX_VERSIONS is required"));
        assert!(source.contains(r#"project="$ROOT/project-$index""#));
        assert!(!source.contains(r#"project="$ROOT/lean-$version""#));
        assert!(!contains_concrete_lean_release(&source));
    }

    let workflow = root.join(".github/workflows/ci.yml");
    if workflow.is_file() {
        let source = fs::read_to_string(workflow).unwrap();
        assert!(source.contains("vars.LEV_CI_TOOLCHAINS"));
        assert!(!source.contains("matrix.lean"));
        assert!(!contains_concrete_lean_release(&source));
    }
}

fn collect_text_files(directory: &Path, output: &mut Vec<PathBuf>) {
    if !directory.is_dir() {
        return;
    }
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            collect_text_files(&entry.path(), output);
        } else if is_release_text_file(&entry.path()) {
            output.push(entry.path());
        }
    }
}

fn is_release_text_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("json" | "md" | "ps1" | "rs" | "sh" | "yaml" | "yml")
    )
}

fn contains_concrete_lean_release(source: &str) -> bool {
    let contextual_prefixes = [
        "v4.",
        "Lean 4.",
        "lean 4.",
        "--lean 4.",
        "--lean v4.",
        "nightly-",
        "\"4.",
        "'4.",
    ];
    if contextual_prefixes
        .iter()
        .any(|prefix| has_digit_after(source, prefix))
    {
        return true;
    }

    // Also catch bare three-part releases in prose. Two-part decimals are
    // ambiguous with timings, so those are covered by the contexts above.
    source.match_indices("4.").any(|(start, _)| {
        let bytes = source.as_bytes();
        let mut index = start + 2;
        let minor_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == minor_start || bytes.get(index) != Some(&b'.') {
            return false;
        }
        index += 1;
        let patch_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        index > patch_start
    })
}

fn has_digit_after(source: &str, prefix: &str) -> bool {
    source.match_indices(prefix).any(|(start, _)| {
        source
            .as_bytes()
            .get(start + prefix.len())
            .is_some_and(u8::is_ascii_digit)
    })
}

#[test]
fn ordinary_git_projects_build_from_the_generic_dependency_path() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "generic dependency");
    fixture.write_manifest(&revision);

    let output = fixture.lev().arg("build").output().unwrap();
    assert_success_ref(&output);
    assert_eq!(
        git_output(
            &fixture.project.join(".lake/packages/dep"),
            &["rev-parse", "HEAD"],
        ),
        revision
    );
}

#[test]
fn short_help_prioritizes_daily_commands_without_hiding_full_help() {
    let short = Command::new(env!("CARGO_BIN_EXE_lev"))
        .arg("-h")
        .output()
        .unwrap();
    let full = Command::new(env!("CARGO_BIN_EXE_lev"))
        .arg("--help")
        .output()
        .unwrap();
    let advanced = Command::new(env!("CARGO_BIN_EXE_lev"))
        .args(["matrix", "-h"])
        .output()
        .unwrap();
    assert_success_ref(&short);
    assert_success_ref(&full);
    assert_success_ref(&advanced);

    let short = String::from_utf8(short.stdout).unwrap();
    let full = String::from_utf8(full.stdout).unwrap();
    let advanced = String::from_utf8(advanced.stdout).unwrap();
    assert!(short.contains("  build "), "{short}");
    assert!(!short.contains("  matrix "), "{short}");
    assert!(full.contains("  matrix "), "{full}");
    assert!(advanced.contains("Usage: lev matrix"), "{advanced}");
}

#[test]
fn sync_reuses_a_mirror_offline_and_run_exports_the_cache_environment() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "first");
    fixture.write_manifest(&revision);

    assert_success(fixture.lev().arg("sync").output().unwrap());
    let checkout = fixture.project.join(".lake/packages/dep");
    assert_eq!(git_output(&checkout, &["rev-parse", "HEAD"]), revision);
    let remote_key = format!(
        "{:x}",
        Sha256::digest(fixture.remote.to_string_lossy().as_bytes())
    );
    let revision_key = format!("{:x}", Sha256::digest(revision.as_bytes()));
    let mirror = fixture
        .cache
        .join("git-v1")
        .join(&remote_key[..2])
        .join(format!("{remote_key}.git"));
    assert_eq!(
        fs::read_to_string(mirror.join("shallow")).unwrap().trim(),
        revision,
        "a cold locked sync should fetch only the selected commit"
    );
    assert_eq!(
        fs::read_to_string(mirror.join("refs/heads/lev-cache").join(&revision_key))
            .unwrap()
            .trim(),
        revision,
        "the private materialization branch must keep the shallow commit clonable"
    );

    fs::remove_dir_all(&checkout).unwrap();
    let unavailable_remote = fixture.root.join("remote-unavailable");
    fs::rename(&fixture.remote, &unavailable_remote).unwrap();
    fs::write(&fixture.log, "").unwrap();

    assert_success(fixture.lev().arg("sync").arg("--offline").output().unwrap());
    assert_eq!(git_output(&checkout, &["rev-parse", "HEAD"]), revision);
    let offline_log = fs::read_to_string(&fixture.log).unwrap();
    assert!(
        offline_log.lines().any(|line| line == "which lean"),
        "{offline_log}"
    );
    assert!(
        !offline_log
            .lines()
            .any(|line| line.ends_with("lean --version")),
        "warm toolchain checks must not start a second Lean process: {offline_log}"
    );
    assert!(
        !offline_log.lines().any(|line| line.contains("--install")),
        "{offline_log}"
    );

    let environment_file = fixture.root.join("environment.txt");
    let output = fixture
        .lev()
        .arg("run")
        .arg("--no-sync")
        .arg("sh")
        .arg("-c")
        .arg("printf '%s\\n%s\\n' \"$LAKE_CACHE_DIR\" \"$LAKE_ARTIFACT_CACHE\" > \"$LEV_TEST_ENV_OUT\"")
        .env("LEV_TEST_ENV_OUT", &environment_file)
        .output()
        .unwrap();
    assert_success(output);

    let environment = fs::read_to_string(environment_file).unwrap();
    let lines: Vec<_> = environment.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(Path::new(lines[0]).starts_with(fixture.cache.join("lake-v1")));
    assert_eq!(lines[1], "true");

    fs::write(&fixture.log, "").unwrap();
    let status = fixture
        .lev()
        .arg("run")
        .arg("--no-sync")
        .arg("--offline")
        .arg("sh")
        .arg("-c")
        .arg("exit 23")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(23));
    let offline_run_log = fs::read_to_string(&fixture.log).unwrap();
    assert!(
        !offline_run_log
            .lines()
            .any(|line| line.contains("--install")),
        "{offline_run_log}"
    );
    assert!(
        !offline_run_log
            .lines()
            .any(|line| line.ends_with("lean --version")),
        "run should launch only the requested process: {offline_run_log}"
    );
    assert_eq!(
        offline_run_log
            .lines()
            .filter(|line| line.starts_with("run "))
            .count(),
        1,
        "{offline_run_log}"
    );
}

#[test]
fn sync_preserves_release_tags_needed_by_lake_cloud_releases() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "tagged release");
    git(
        &fixture.remote,
        &["tag", "-a", "release-test", "-m", "release test"],
    );
    let manifest = json!({
        "version": "1.2.0",
        "packagesDir": ".lake/packages",
        "packages": [{
            "url": fixture.remote.to_string_lossy(),
            "type": "git",
            "rev": revision,
            "inputRev": "release-test",
            "name": "dep"
        }],
        "name": "root"
    });
    fs::write(
        fixture.project.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    assert_success(fixture.lev().arg("sync").output().unwrap());
    let checkout = fixture.project.join(".lake/packages/dep");
    assert_eq!(
        git_output(&checkout, &["rev-parse", "refs/tags/release-test^{commit}"],),
        revision
    );
    assert_eq!(
        git_output(&checkout, &["cat-file", "-t", "refs/tags/release-test"]),
        "tag",
        "annotated tag objects should survive materialization"
    );

    fs::remove_dir_all(&checkout).unwrap();
    fs::rename(&fixture.remote, fixture.root.join("remote-unavailable")).unwrap();
    assert_success(fixture.lev().arg("sync").arg("--offline").output().unwrap());
    assert_eq!(
        git_output(&checkout, &["rev-parse", "refs/tags/release-test^{commit}"],),
        revision,
        "the release tag should be reusable without the original remote"
    );
}

#[test]
fn sync_does_not_attach_a_moved_symbolic_ref_to_an_old_lock() {
    let fixture = Fixture::new();
    let locked = fixture.commit("def answer := 1\n", "locked revision");
    git(&fixture.remote, &["branch", "moving-release", &locked]);
    let newer = fixture.commit("def answer := 2\n", "newer revision");
    git(
        &fixture.remote,
        &["branch", "--force", "moving-release", &newer],
    );
    let manifest = json!({
        "version": "1.2.0",
        "packagesDir": ".lake/packages",
        "packages": [{
            "url": fixture.remote.to_string_lossy(),
            "type": "git",
            "rev": locked,
            "inputRev": "moving-release",
            "name": "dep"
        }],
        "name": "root"
    });
    fs::write(
        fixture.project.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    assert_success(fixture.lev().arg("sync").output().unwrap());
    let checkout = fixture.project.join(".lake/packages/dep");
    assert_eq!(git_output(&checkout, &["rev-parse", "HEAD"]), locked);
    let status = Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/moving-release",
        ])
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "a moving branch must not be presented as the selector for an older lock"
    );
}

#[test]
fn sync_retries_symbolic_ref_discovery_after_a_remote_failure() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "release");
    fixture.write_manifest(&revision);
    assert_success(fixture.lev().arg("sync").output().unwrap());

    git(
        &fixture.remote,
        &["tag", "-a", "retry-release", "-m", "retry release"],
    );
    let manifest = json!({
        "version": "1.2.0",
        "packagesDir": ".lake/packages",
        "packages": [{
            "url": fixture.remote.to_string_lossy(),
            "type": "git",
            "rev": revision,
            "inputRev": "retry-release",
            "name": "dep"
        }],
        "name": "root"
    });
    fs::write(
        fixture.project.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let unavailable_remote = fixture.root.join("remote-unavailable");
    fs::rename(&fixture.remote, &unavailable_remote).unwrap();
    assert_success(fixture.lev().arg("sync").output().unwrap());

    fs::rename(&unavailable_remote, &fixture.remote).unwrap();
    assert_success(fixture.lev().arg("sync").output().unwrap());
    assert_eq!(
        git_output(
            &fixture.project.join(".lake/packages/dep"),
            &["rev-parse", "refs/tags/retry-release^{commit}"],
        ),
        revision
    );
}

#[test]
fn sync_materializes_lake_escaped_names_at_the_unescaped_directory() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "quoted package");
    let manifest = json!({
        "version": "1.2.0",
        "packagesDir": ".lake/packages",
        "packages": [{
            "url": fixture.remote.to_string_lossy(),
            "type": "git",
            "rev": revision,
            "name": "\u{00ab}dep-name\u{00bb}"
        }],
        "name": "root"
    });
    fs::write(
        fixture.project.join("lake-manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    assert_success(fixture.lev().arg("sync").output().unwrap());
    let checkout = fixture.project.join(".lake/packages/dep-name");
    assert_eq!(git_output(&checkout, &["rev-parse", "HEAD"]), revision);
    assert!(
        !fixture
            .project
            .join(".lake/packages/\u{00ab}dep-name\u{00bb}")
            .exists()
    );
    assert_success(fixture.lev().arg("audit").output().unwrap());

    fs::remove_dir_all(&checkout).unwrap();
    fs::rename(&fixture.remote, fixture.root.join("remote-unavailable")).unwrap();
    assert_success(fixture.lev().arg("sync").arg("--offline").output().unwrap());
    assert_eq!(git_output(&checkout, &["rev-parse", "HEAD"]), revision);
}

#[test]
fn lock_and_frozen_sync_detect_project_drift_before_running_elan() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "first");
    fixture.write_manifest(&revision);

    assert_success(fixture.lev().arg("sync").output().unwrap());
    let lock = fixture.project.join("lev.lock");
    assert!(lock.is_file());
    assert_success(fixture.lev().arg("lock").arg("--check").output().unwrap());

    let config = fixture.project.join("lakefile.toml");
    let original = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{original}\n# drift\n")).unwrap();
    fs::write(&fixture.log, "").unwrap();
    let drifted = fixture
        .lev()
        .arg("sync")
        .arg("--frozen")
        .arg("--offline")
        .output()
        .unwrap();
    assert!(!drifted.status.success());
    let stderr = String::from_utf8_lossy(&drifted.stderr);
    assert!(stderr.contains("configuration changed"), "{stderr}");
    assert_eq!(fs::read_to_string(&fixture.log).unwrap(), "");

    fs::write(&config, original).unwrap();
    assert_success(
        fixture
            .lev()
            .arg("sync")
            .arg("--frozen")
            .arg("--offline")
            .output()
            .unwrap(),
    );

    let manifest = fixture.project.join("lake-manifest.json");
    let mut contents = fs::read_to_string(&manifest).unwrap();
    contents.push('\n');
    fs::write(&manifest, contents).unwrap();
    let drifted = fixture.lev().arg("lock").arg("--check").output().unwrap();
    assert!(!drifted.status.success());
    let stderr = String::from_utf8_lossy(&drifted.stderr);
    assert!(stderr.contains("lake-manifest.json changed"), "{stderr}");

    assert_success(fixture.lev().arg("lock").output().unwrap());
    assert_success(
        fixture
            .lev()
            .arg("sync")
            .arg("--frozen")
            .arg("--offline")
            .output()
            .unwrap(),
    );
}

#[test]
fn sync_refuses_to_overwrite_dirty_dependency_work() {
    let fixture = Fixture::new();
    let first = fixture.commit("def answer := 1\n", "first");
    fixture.write_manifest(&first);
    assert_success(fixture.lev().arg("sync").output().unwrap());

    let second = fixture.commit("def answer := 2\n", "second");
    fixture.write_manifest(&second);
    let checkout_file = fixture.project.join(".lake/packages/dep/Dep.lean");
    fs::write(&checkout_file, "def localWork := 99\n").unwrap();

    let output = fixture.lev().arg("sync").output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("has local changes"), "{stderr}");
    assert_eq!(
        fs::read_to_string(checkout_file).unwrap(),
        "def localWork := 99\n"
    );
}

#[test]
fn init_pins_the_requested_normalized_toolchain() {
    let fixture = Fixture::new();
    let target = fixture.root.join("new-project");
    let output = fixture
        .lev()
        .arg("init")
        .arg(&target)
        .arg("--lean")
        .arg("4.fixture-b")
        .arg("--template")
        .arg("lib")
        .output()
        .unwrap();
    assert_success(output);
    assert_eq!(
        fs::read_to_string(target.join("lean-toolchain")).unwrap(),
        "leanprover/lean4:v4.fixture-b\n"
    );
    assert!(target.join("lakefile.toml").is_file());
}

#[test]
fn build_failures_name_lake_the_toolchain_and_a_focused_rerun() {
    let fixture = Fixture::new();
    let output = fixture
        .lev()
        .arg("build")
        .arg("--no-sync")
        .arg("--rehash")
        .env("LEV_TEST_LAKE_BUILD_FAIL_MEMBER", "project")
        .env("LEV_TEST_LAKE_BUILD_EXIT", "37")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(37));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Lake build failed under leanprover/lean4:v4.test with exit code 37"),
        "{stderr}"
    );
    assert!(
        stderr.contains("lev build --no-sync --rehash TARGET"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(
        log.lines().any(|line| line.contains("lake --rehash build")),
        "{log}"
    );
}
