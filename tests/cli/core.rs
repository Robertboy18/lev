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

    let matrix_script = root.join("scripts/test-lean-matrix.sh");
    if matrix_script.is_file() {
        let source = fs::read_to_string(matrix_script).unwrap();
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
fn root_help_exposes_families_and_familiar_direct_commands() {
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
    let imports = Command::new(env!("CARGO_BIN_EXE_lev"))
        .args(["imports", "-h"])
        .output()
        .unwrap();
    let shake = Command::new(env!("CARGO_BIN_EXE_lev"))
        .args(["shake", "-h"])
        .output()
        .unwrap();
    let doctor = Command::new(env!("CARGO_BIN_EXE_lev"))
        .args(["doctor", "-h"])
        .output()
        .unwrap();
    let profile = Command::new(env!("CARGO_BIN_EXE_lev"))
        .args(["profile", "-h"])
        .output()
        .unwrap();
    let verify = Command::new(env!("CARGO_BIN_EXE_lev"))
        .args(["verify", "-h"])
        .output()
        .unwrap();
    assert_success_ref(&short);
    assert_success_ref(&full);
    assert_success_ref(&advanced);
    assert_success_ref(&imports);
    assert_success_ref(&shake);
    assert_success_ref(&doctor);
    assert_success_ref(&profile);
    assert_success_ref(&verify);

    let short = String::from_utf8(short.stdout).unwrap();
    let full = String::from_utf8(full.stdout).unwrap();
    let advanced = String::from_utf8(advanced.stdout).unwrap();
    let imports = String::from_utf8(imports.stdout).unwrap();
    let shake = String::from_utf8(shake.stdout).unwrap();
    let doctor = String::from_utf8(doctor.stdout).unwrap();
    let profile = String::from_utf8(profile.stdout).unwrap();
    let verify = String::from_utf8(verify.stdout).unwrap();
    assert!(short.contains("  build "), "{short}");
    assert!(short.contains("  inspect "), "{short}");
    assert!(full.contains("  inspect "), "{full}");
    for visible in ["doctor", "shake", "profile", "verify"] {
        assert!(short.contains(&format!("  {visible} ")), "{short}");
        assert!(full.contains(&format!("  {visible} ")), "{full}");
    }
    assert!(!short.contains("  help "), "{short}");
    assert!(!full.contains("  help "), "{full}");
    assert!(!short.contains("  matrix "), "{short}");
    assert!(!full.contains("  matrix "), "{full}");
    assert!(advanced.contains("Usage: lev matrix"), "{advanced}");
    assert!(imports.contains("Usage: lev imports"), "{imports}");
    assert!(shake.contains("Usage: lev shake"), "{shake}");
    assert!(doctor.contains("Usage: lev doctor"), "{doctor}");
    assert!(profile.contains("Usage: lev profile"), "{profile}");
    assert!(verify.contains("Usage: lev verify"), "{verify}");
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

#[test]
fn shake_dry_run_reports_precise_imports_without_writing() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let original = "module\n\nimport Mathlib\n\ntheorem example : True := trivial\n";
    fs::write(&source, original).unwrap();
    fs::write(
        &replacement,
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n\ntheorem example : True := trivial\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .arg("imports")
        .arg("--no-sync")
        .arg("--keep-public")
        .arg("--explain")
        .arg("Root")
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .env("LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("remove #[import Mathlib]"), "{stdout}");
    assert!(
        stdout.contains("add #[import Mathlib.Data.Nat.Prime.Basic]"),
        "{stdout}"
    );
    assert_eq!(fs::read_to_string(source).unwrap(), original);
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(
        log.lines()
            .any(|line| { line.contains("lake shake --keep-public --explain Root") }),
        "{log}"
    );
    assert!(
        log.lines()
            .any(|line| line.contains("lake build --no-build +Root")),
        "{log}"
    );
    assert!(!log.contains("lake-build"), "{log}");
}

#[test]
fn shake_apply_rewrites_then_verifies_the_module_and_project() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let original = "module\n\nimport Mathlib\n\ntheorem example : True := trivial\n";
    let minimized =
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n\ntheorem example : True := trivial\n";
    fs::write(&source, original).unwrap();
    fs::write(&replacement, minimized).unwrap();

    let output = fixture
        .lev()
        .arg("shake")
        .arg("--no-sync")
        .arg("--apply")
        .arg("Root")
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .env("LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD", "1")
        .output()
        .unwrap();

    assert_success_ref(&output);
    assert_eq!(fs::read_to_string(source).unwrap(), minimized);
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("lake-shake"))
            .count(),
        2,
        "{log}"
    );
    let builds = log
        .lines()
        .filter(|line| line.contains("lake-build"))
        .collect::<Vec<_>>();
    assert_eq!(builds.len(), 2, "{log}");
    assert!(builds[0].ends_with("lake build"), "{log}");
    assert!(builds[1].contains("lake build +Root"), "{log}");
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("lake-check"))
            .count(),
        3,
        "{log}"
    );
}

#[test]
fn shake_fix_restores_sources_when_default_artifacts_are_missing() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let original = "module\n\nimport Mathlib\n\ntheorem example : True := trivial\n";
    fs::write(&source, original).unwrap();
    fs::write(
        &replacement,
        "module\n\nimport Mathlib.Init\n\ntheorem example : True := trivial\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .arg("shake")
        .arg("--no-sync")
        .arg("--fix")
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .env("LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD", "1")
        .env("LEV_TEST_SHAKE_FRESHNESS_FAIL", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_to_string(source).unwrap(), original);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("object file 'Root.olean'"), "{stderr}");
    assert!(
        stderr.contains("import verification failed; restoring the original Lean sources"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("lake-build"))
            .count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("lake-check"))
            .count(),
        1,
        "{log}"
    );
}

#[test]
fn shake_fix_restores_sources_when_the_verification_build_fails() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let original = "module\n\nimport Mathlib\n\ntheorem example : True := trivial\n";
    fs::write(&source, original).unwrap();
    fs::write(
        &replacement,
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n\ntheorem example : True := trivial\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .arg("shake")
        .arg("--no-sync")
        .arg("--fix")
        .arg("Root")
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .env("LEV_TEST_LAKE_BUILD_FAIL_MEMBER", "project")
        .env("LEV_TEST_LAKE_BUILD_EXIT", "37")
        .env("LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(37));
    assert_eq!(fs::read_to_string(source).unwrap(), original);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("import verification failed; restoring the original Lean sources"),
        "{stderr}"
    );
}

#[test]
fn shake_builds_stale_modules_and_retries_analysis() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let marker = fixture.root.join("shake-built");
    fs::write(&source, "module\n\nimport Mathlib\n").unwrap();
    fs::write(&replacement, "module\n\nimport Mathlib.Init\n").unwrap();

    let output = fixture
        .lev()
        .arg("shake")
        .arg("--no-sync")
        .arg("Root")
        .env("LEV_TEST_SHAKE_REQUIRE_BUILD", "1")
        .env("LEV_TEST_SHAKE_BUILD_MARKER", &marker)
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .env("LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(marker.is_file());
    let log = fs::read_to_string(&fixture.log).unwrap();
    let actions = log
        .lines()
        .filter(|line| line.contains("lake-shake") || line.contains("lake-build"))
        .collect::<Vec<_>>();
    assert_eq!(actions.len(), 4, "{log}");
    assert!(actions[0].contains("lake-shake"), "{log}");
    assert!(actions[1].ends_with("lake build"), "{log}");
    assert!(actions[2].contains("lake build +Root"), "{log}");
    assert!(actions[3].contains("lake-shake"), "{log}");
}

#[test]
fn shake_builds_stale_explicit_modules_before_analysis() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let marker = fixture.root.join("explicit-shake-built");
    fs::write(&source, "module\n\nimport Mathlib\n").unwrap();
    fs::write(&replacement, "module\n\nimport Mathlib.Init\n").unwrap();

    let output = fixture
        .lev()
        .arg("shake")
        .arg("--no-sync")
        .arg("Root")
        .env("LEV_TEST_SHAKE_EXPLICIT_REQUIRE_BUILD", "1")
        .env("LEV_TEST_SHAKE_BUILD_MARKER", &marker)
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .env("LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(marker.is_file());
    let log = fs::read_to_string(&fixture.log).unwrap();
    let actions = log
        .lines()
        .filter(|line| {
            line.contains("lake-check")
                || line.contains("lake-build")
                || line.contains("lake-shake")
        })
        .collect::<Vec<_>>();
    assert_eq!(actions.len(), 3, "{log}");
    assert!(actions[0].contains("lake build --no-build +Root"), "{log}");
    assert!(actions[1].contains("lake build +Root"), "{log}");
    assert!(actions[2].contains("lake shake Root"), "{log}");
}

#[test]
fn shake_fix_refuses_to_edit_dependency_checkouts() {
    let fixture = Fixture::new();
    let source = fixture
        .project
        .join(".lake/packages/mathlib/Mathlib/Unsafe.lean");
    let replacement = fixture.root.join("replacement.lean");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "module\n\nimport Mathlib\n").unwrap();
    fs::write(&replacement, "module\n\nimport Mathlib.Init\n").unwrap();

    let output = fixture
        .lev()
        .arg("shake")
        .arg("--no-sync")
        .arg("--fix")
        .arg("Mathlib.Unsafe")
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(source).unwrap(),
        "module\n\nimport Mathlib\n"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to edit generated or dependency source"),
        "{stderr}"
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("lake-shake"))
            .count(),
        1,
        "{log}"
    );
}

#[test]
fn shake_check_conflicts_with_fix() {
    let fixture = Fixture::new();
    let output = fixture
        .lev()
        .args(["shake", "--check", "--fix"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--check"), "{stderr}");
    assert!(stderr.contains("--fix"), "{stderr}");
}

#[test]
fn shake_isolate_requires_fix() {
    let fixture = Fixture::new();
    let output = fixture.lev().args(["shake", "--isolate"]).output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--fix"), "{stderr}");
}

#[test]
fn shake_check_emits_a_versioned_json_drift_report() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    fs::write(&source, "module\n\nimport Mathlib\n").unwrap();
    fs::write(
        &replacement,
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["shake", "--no-sync", "--check", "--json"])
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let report = json_data(&output, "lev.cli.shake/v1");
    assert_eq!(report["mode"], "check");
    assert_eq!(report["status"], "changes_required");
    assert_eq!(report["files"].as_array().unwrap().len(), 1);
    assert_eq!(report["files"][0]["path"], "Root.lean");
    assert_eq!(
        report["files"][0]["removed_imports"],
        json!(["import Mathlib"])
    );
    assert_eq!(
        report["files"][0]["added_imports"],
        json!(["import Mathlib.Data.Nat.Prime.Basic"])
    );
    assert_eq!(report["files"][0]["disposition"], "proposed");
    assert_eq!(
        fs::read_to_string(source).unwrap(),
        "module\n\nimport Mathlib\n"
    );
}

#[test]
fn shake_can_emit_sarif_for_ci_annotations() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let sarif = fixture.root.join("shake.sarif");
    fs::write(&source, "module\n\nimport Mathlib\n").unwrap();
    fs::write(
        &replacement,
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["shake", "--no-sync", "--check", "--sarif"])
        .arg(&sarif)
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&fs::read(sarif).unwrap()).unwrap();
    assert_eq!(report["version"], "2.1.0");
    assert_eq!(report["runs"][0]["tool"]["driver"]["name"], "lev");
    assert_eq!(
        report["runs"][0]["results"][0]["ruleId"],
        "lev.import-drift"
    );
    assert_eq!(
        report["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "Root.lean"
    );
}

#[test]
fn shake_sarif_omits_imports_that_were_successfully_applied() {
    let fixture = Fixture::new();
    let source = fixture.project.join("Root.lean");
    let replacement = fixture.root.join("replacement.lean");
    let sarif = fixture.root.join("shake-applied.sarif");
    fs::write(&source, "module\n\nimport Mathlib\n").unwrap();
    fs::write(
        &replacement,
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["shake", "--no-sync", "--fix", "--sarif"])
        .arg(&sarif)
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &source)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &replacement)
        .output()
        .unwrap();
    assert_success_ref(&output);
    assert!(output.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(source).unwrap(),
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n"
    );

    let report: serde_json::Value = serde_json::from_slice(&fs::read(sarif).unwrap()).unwrap();
    assert_eq!(report["runs"][0]["results"], json!([]));
}

#[test]
fn shake_coverage_classifies_every_project_lean_file_once() {
    let fixture = Fixture::new();
    let analyzed = fixture.project.join("Analyzed.lean");
    fs::write(&analyzed, "module\n\nimport Init\n").unwrap();
    fs::write(
        fixture.project.join("Outside.lean"),
        "-- header\nmodule\n\nimport Init\n",
    )
    .unwrap();
    fs::write(
        fixture.project.join("Legacy.lean"),
        "import Init\n\ndef legacy := true\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["shake", "--no-sync", "--check", "--coverage", "--json"])
        .env("LEV_TEST_SHAKE_EXPLAIN_FILE", &analyzed)
        .output()
        .unwrap();

    assert_success_ref(&output);
    let report = json_data(&output, "lev.cli.shake/v1");
    assert_eq!(report["status"], "clean");
    assert_eq!(report["coverage"]["total_files"], 3);
    assert_eq!(report["coverage"]["analyzed"], json!(["Analyzed.lean"]));
    assert_eq!(report["coverage"]["outside_roots"], json!(["Outside.lean"]));
    assert_eq!(report["coverage"]["legacy"], json!(["Legacy.lean"]));
}

#[test]
fn shake_isolate_keeps_passing_edits_and_restores_failing_edits() {
    let fixture = Fixture::new();
    let passing = fixture.project.join("A.lean");
    let failing = fixture.project.join("B.lean");
    let passing_replacement = fixture.root.join("A.replacement.lean");
    let failing_replacement = fixture.root.join("B.replacement.lean");
    let passing_original = "module\n\nimport Mathlib\n\ndef passing := true\n";
    let failing_original = "module\n\nimport Mathlib\n\ndef failing := true\n";
    let passing_minimized =
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n\ndef passing := true\n";
    let failing_minimized = "module\n\nimport Mathlib.Algebra.Order.Ring.Nat\n\n-- BROKEN_IMPORT\ndef failing := true\n";
    fs::write(&passing, passing_original).unwrap();
    fs::write(&failing, failing_original).unwrap();
    fs::write(&passing_replacement, passing_minimized).unwrap();
    fs::write(&failing_replacement, failing_minimized).unwrap();

    let output = fixture
        .lev()
        .args(["shake", "--no-sync", "--fix", "--isolate", "--json"])
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &passing)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &passing_replacement)
        .env("LEV_TEST_SHAKE_SECOND_FILE", &failing)
        .env(
            "LEV_TEST_SHAKE_SECOND_REPLACEMENT_FILE",
            &failing_replacement,
        )
        .env("LEV_TEST_SHAKE_FAIL_FILE", &failing)
        .env("LEV_TEST_SHAKE_FAIL_CONTENT", "BROKEN_IMPORT")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&passing).unwrap(), passing_minimized);
    assert_eq!(fs::read_to_string(&failing).unwrap(), failing_original);
    let report = json_data(&output, "lev.cli.shake/v1");
    assert_eq!(report["mode"], "isolate");
    assert_eq!(report["status"], "partially_applied");
    assert_eq!(report["files"][0]["path"], "A.lean");
    assert_eq!(report["files"][0]["disposition"], "applied");
    assert_eq!(report["files"][1]["path"], "B.lean");
    assert_eq!(report["files"][1]["disposition"], "rejected");
    assert_eq!(
        report["files"][1]["keep_annotations"],
        json!(["import Mathlib -- shake: keep"])
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("keep the dependency with `import Mathlib -- shake: keep`"),
        "{stderr}"
    );
}

#[test]
fn shake_isolate_restores_every_edit_when_the_baseline_fails() {
    let fixture = Fixture::new();
    let first = fixture.project.join("A.lean");
    let broken = fixture.project.join("B.lean");
    let first_replacement = fixture.root.join("A.replacement.lean");
    let broken_replacement = fixture.root.join("B.replacement.lean");
    let first_original = "module\n\nimport Mathlib\n\ndef first := true\n";
    let broken_original = "module\n\nimport Mathlib\n\n-- BASELINE_BROKEN\ndef broken := true\n";
    fs::write(&first, first_original).unwrap();
    fs::write(&broken, broken_original).unwrap();
    fs::write(
        &first_replacement,
        "module\n\nimport Mathlib.Data.Nat.Prime.Basic\n\ndef first := true\n",
    )
    .unwrap();
    fs::write(
        &broken_replacement,
        "module\n\nimport Mathlib.Algebra.Order.Ring.Nat\n\n-- BASELINE_BROKEN\ndef broken := true\n",
    )
    .unwrap();

    let output = fixture
        .lev()
        .args(["shake", "--no-sync", "--fix", "--isolate", "--json"])
        .env("LEV_TEST_SHAKE_SUGGEST", "1")
        .env("LEV_TEST_SHAKE_FILE", &first)
        .env("LEV_TEST_SHAKE_REPLACEMENT_FILE", &first_replacement)
        .env("LEV_TEST_SHAKE_SECOND_FILE", &broken)
        .env(
            "LEV_TEST_SHAKE_SECOND_REPLACEMENT_FILE",
            &broken_replacement,
        )
        .env("LEV_TEST_SHAKE_FAIL_FILE", &broken)
        .env("LEV_TEST_SHAKE_FAIL_CONTENT", "BASELINE_BROKEN")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(41));
    assert_eq!(fs::read_to_string(&first).unwrap(), first_original);
    assert_eq!(fs::read_to_string(&broken).unwrap(), broken_original);
    let report = json_data(&output, "lev.cli.shake/v1");
    assert_eq!(report["status"], "baseline_failed");
    for file in report["files"].as_array().unwrap() {
        assert_eq!(file["disposition"], "restored");
    }
}
