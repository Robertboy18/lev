use super::*;

#[test]
fn workspace_lists_syncs_locks_and_rejects_partial_aggregate_updates() {
    let fixture = Fixture::new();
    let workspace = fixture.root.join("monorepo");
    write_empty_project(&workspace.join("packages/alpha"), "alpha");
    write_empty_project(&workspace.join("packages/beta"), "beta");
    write_empty_project(&workspace.join("packages/experimental"), "experimental");
    fs::write(
        workspace.join("lev.toml"),
        r#"[workspace]
members = ["packages/*"]
exclude = ["packages/experimental"]
"#,
    )
    .unwrap();

    let listed = fixture
        .lev_at(&workspace.join("packages/alpha"))
        .arg("workspace")
        .arg("list")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&listed);
    let listed = json_data(&listed, "lev.cli.workspace.list/v1");
    assert_eq!(
        listed["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["packages/alpha", "packages/beta"]
    );
    assert_eq!(
        listed["root"].as_str().unwrap(),
        fs::canonicalize(&workspace).unwrap().to_string_lossy()
    );

    let synced = fixture
        .lev_at(&workspace)
        .arg("workspace")
        .arg("sync")
        .arg("--offline")
        .output()
        .unwrap();
    assert_success_ref(&synced);
    assert!(workspace.join("packages/alpha/lev.lock").is_file());
    assert!(workspace.join("packages/beta/lev.lock").is_file());
    assert!(workspace.join("lev-workspace.lock").is_file());
    assert_success(
        fixture
            .lev_at(&workspace)
            .arg("workspace")
            .arg("lock")
            .arg("--check")
            .output()
            .unwrap(),
    );

    fs::write(
        workspace.join("packages/alpha/lakefile.toml"),
        "[package]\nname = \"alpha-drifted\"\n",
    )
    .unwrap();
    let drifted = fixture
        .lev_at(&workspace)
        .arg("workspace")
        .arg("lock")
        .arg("--check")
        .output()
        .unwrap();
    assert!(!drifted.status.success());
    let stderr = String::from_utf8_lossy(&drifted.stderr);
    assert!(stderr.contains("packages/alpha"), "{stderr}");
    assert!(stderr.contains("configuration changed"), "{stderr}");
    assert_success(
        fixture
            .lev_at(&workspace)
            .arg("workspace")
            .arg("lock")
            .output()
            .unwrap(),
    );

    fs::remove_file(workspace.join("lev-workspace.lock")).unwrap();
    fs::write(&fixture.log, "").unwrap();
    let partial = fixture
        .lev_at(&workspace)
        .arg("workspace")
        .arg("sync")
        .arg("--update")
        .arg("--keep-going")
        .env("LEV_TEST_LAKE_UPDATE_FAIL_MEMBER", "alpha")
        .output()
        .unwrap();
    assert_eq!(partial.status.code(), Some(1));
    assert!(
        !workspace.join("lev-workspace.lock").exists(),
        "a partial sync must not publish an aggregate lock"
    );
    let updates = tagged_log_paths(&fixture.log, "lake-update");
    assert_eq!(
        updates,
        [
            workspace.join("packages/alpha"),
            workspace.join("packages/beta")
        ]
    );
}

#[test]
fn workspace_build_and_run_keep_going_and_honor_local_mode() {
    let fixture = Fixture::new();
    let workspace = fixture.root.join("monorepo");
    write_empty_project(&workspace.join("packages/alpha"), "alpha");
    write_empty_project(&workspace.join("packages/beta"), "beta");
    fs::write(
        workspace.join("lev.toml"),
        "[workspace]\nmembers = [\"packages/*\"]\n",
    )
    .unwrap();

    let build = fixture
        .lev_at(&workspace)
        .arg("workspace")
        .arg("build")
        .arg("--no-sync")
        .arg("--offline")
        .arg("--keep-going")
        .arg("--rehash")
        .arg("--")
        .arg("--wfail")
        .env("LEV_TEST_LAKE_BUILD_FAIL_MEMBER", "alpha")
        .env("LEV_TEST_LAKE_BUILD_EXIT", "37")
        .output()
        .unwrap();
    assert_eq!(build.status.code(), Some(37));
    let stderr = String::from_utf8_lossy(&build.stderr);
    assert!(
        stderr.contains("packages/alpha: build failed with exit code 37"),
        "{stderr}"
    );
    assert!(
        fs::read_to_string(&fixture.log)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("lake-build\t"))
            .all(|line| line.contains("lake --rehash build")),
        "workspace builds must forward --rehash before Lake's build subcommand"
    );
    assert_eq!(
        tagged_log_paths(&fixture.log, "lake-build"),
        [
            workspace.join("packages/alpha"),
            workspace.join("packages/beta")
        ]
    );

    let run_output = fixture.root.join("workspace-run.txt");
    let run = fixture
        .lev_at(&workspace)
        .arg("workspace")
        .arg("run")
        .arg("--no-sync")
        .arg("--offline")
        .arg("--keep-going")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(
            "printf '%s\\n' \"$PWD\" >> \"$LEV_TEST_WORKSPACE_OUT\"; \
             case \"$PWD\" in */alpha) exit 23;; esac",
        )
        .env("LEV_TEST_WORKSPACE_OUT", &run_output)
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(23));
    assert_eq!(
        fs::read_to_string(&run_output)
            .unwrap()
            .lines()
            .map(PathBuf::from)
            .collect::<Vec<_>>(),
        [
            workspace.join("packages/alpha"),
            workspace.join("packages/beta")
        ]
    );

    let local_output = fixture.root.join("workspace-local.txt");
    let local = fixture
        .lev_at(&workspace)
        .arg("--local")
        .arg("workspace")
        .arg("run")
        .arg("--no-sync")
        .arg("--offline")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg("printf '%s\\n' \"$PWD\" >> \"$LEV_TEST_WORKSPACE_OUT\"")
        .env("LEV_TEST_WORKSPACE_OUT", &local_output)
        .output()
        .unwrap();
    assert_success_ref(&local);
    let local_paths = fs::read_to_string(local_output)
        .unwrap()
        .lines()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    assert_eq!(local_paths.len(), 2);
    assert_ne!(local_paths[0], local_paths[1]);
    assert!(
        local_paths
            .iter()
            .all(|path| path.starts_with(fixture.cache.join("workspaces-v1")))
    );
}

#[test]
fn export_and_audit_report_locked_and_dirty_dependency_state() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "first");
    fixture.write_manifest(&revision);
    assert_success(fixture.lev().arg("sync").output().unwrap());

    let first = fixture
        .lev()
        .arg("export")
        .arg("--format")
        .arg("lev")
        .output()
        .unwrap();
    assert_success_ref(&first);
    let inventory: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(inventory["schema"], "lev.project-export/v1");
    assert_eq!(
        inventory["project"]["toolchain"],
        "leanprover/lean4:v4.test"
    );
    assert_eq!(inventory["packages"][0]["name"], "dep");

    let repeated = fixture
        .lev()
        .arg("export")
        .arg("--format")
        .arg("lev")
        .output()
        .unwrap();
    assert_success_ref(&repeated);
    assert_eq!(repeated.stdout, first.stdout);

    let bom_path = fixture.root.join("reports/project.cdx.json");
    assert_success(
        fixture
            .lev()
            .arg("export")
            .arg("--format")
            .arg("cyclonedx")
            .arg("--output")
            .arg(&bom_path)
            .output()
            .unwrap(),
    );
    let bom: serde_json::Value = serde_json::from_slice(&fs::read(&bom_path).unwrap()).unwrap();
    assert_eq!(bom["bomFormat"], "CycloneDX");
    assert_eq!(bom["components"][0]["name"], "dep");
    let refused = fixture
        .lev()
        .arg("export")
        .arg("--output")
        .arg(&bom_path)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("pass --force"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    fs::remove_file(&bom_path).unwrap();
    let mut first_racer = fixture.lev();
    first_racer
        .arg("export")
        .arg("--output")
        .arg(&bom_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut second_racer = fixture.lev();
    second_racer
        .arg("export")
        .arg("--output")
        .arg(&bom_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let first_racer = first_racer.spawn().unwrap();
    let second_racer = second_racer.spawn().unwrap();
    let first_racer = first_racer.wait_with_output().unwrap();
    let second_racer = second_racer.wait_with_output().unwrap();
    assert_ne!(
        first_racer.status.success(),
        second_racer.status.success(),
        "exactly one create-only export should win\nfirst: {}\nsecond: {}",
        String::from_utf8_lossy(&first_racer.stderr),
        String::from_utf8_lossy(&second_racer.stderr)
    );
    let raced: serde_json::Value = serde_json::from_slice(&fs::read(&bom_path).unwrap()).unwrap();
    assert_eq!(raced["schema"], "lev.project-export/v1");
    assert!(
        fs::read_dir(bom_path.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".lev-new-"))
    );

    let clean = fixture.lev().arg("audit").arg("--json").output().unwrap();
    assert_success_ref(&clean);
    let clean = json_data(&clean, "lev.cli.audit/v1");
    assert_eq!(clean["summary"]["errors"], 0);
    assert_eq!(clean["summary"]["warnings"], 0);

    fs::write(
        fixture.project.join(".lake/packages/dep/Dep.lean"),
        "def localWork := 99\n",
    )
    .unwrap();
    let dirty = fixture.lev().arg("audit").arg("--json").output().unwrap();
    assert_eq!(dirty.status.code(), Some(1));
    let dirty = json_data(&dirty, "lev.cli.audit/v1");
    assert_eq!(dirty["summary"]["errors"], 1);
    assert!(
        dirty["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| { finding["check"] == "git_clean" && finding["level"] == "error" })
    );

    assert_success(
        fixture
            .lev()
            .arg("audit")
            .arg("--no-checkouts")
            .output()
            .unwrap(),
    );
}

#[test]
fn publish_requires_release_provenance_and_preserves_lake_exit_codes() {
    let fixture = Fixture::new();
    write_empty_project(&fixture.project, "root");
    assert_success(fixture.lev().arg("sync").output().unwrap());
    fs::write(fixture.project.join(".gitignore"), ".lake/\n").unwrap();
    git(&fixture.project, &["init", "--initial-branch=main"]);
    git(
        &fixture.project,
        &["config", "user.email", "lev@example.invalid"],
    );
    git(&fixture.project, &["config", "user.name", "lev test"]);
    git(&fixture.project, &["add", "."]);
    git(&fixture.project, &["commit", "-m", "release"]);
    git(&fixture.project, &["tag", "v1.0.0"]);
    fs::write(&fixture.log, "").unwrap();

    let dry_run = fixture
        .lev()
        .arg("publish")
        .arg("v1.0.0")
        .arg("--no-sync")
        .arg("--offline")
        .arg("--dry-run")
        .arg("--")
        .arg("--wfail")
        .output()
        .unwrap();
    assert_success_ref(&dry_run);
    assert!(
        String::from_utf8_lossy(&dry_run.stdout).contains("would upload"),
        "{}",
        String::from_utf8_lossy(&dry_run.stdout)
    );
    assert_eq!(
        tagged_log_paths(&fixture.log, "lake-build"),
        std::slice::from_ref(&fixture.project)
    );
    assert!(tagged_log_paths(&fixture.log, "lake-upload").is_empty());

    let uploaded = fixture
        .lev()
        .arg("publish")
        .arg("v1.0.0")
        .arg("--no-sync")
        .arg("--offline")
        .arg("--no-build")
        .output()
        .unwrap();
    assert_success_ref(&uploaded);
    assert_eq!(
        tagged_log_paths(&fixture.log, "lake-upload"),
        std::slice::from_ref(&fixture.project)
    );

    let failed = fixture
        .lev()
        .arg("publish")
        .arg("v1.0.0")
        .arg("--no-sync")
        .arg("--offline")
        .arg("--no-build")
        .env("LEV_TEST_LAKE_UPLOAD_EXIT", "29")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(29));
    assert!(
        git_output(&fixture.project, &["status", "--porcelain=v1"]).is_empty(),
        "a failed Lake upload must not modify release inputs"
    );

    fs::write(fixture.project.join("uncommitted"), "dirty").unwrap();
    let dirty = fixture
        .lev()
        .arg("publish")
        .arg("v1.0.0")
        .arg("--no-sync")
        .arg("--offline")
        .arg("--no-build")
        .output()
        .unwrap();
    assert!(!dirty.status.success());
    assert!(
        String::from_utf8_lossy(&dirty.stderr).contains("local changes"),
        "{}",
        String::from_utf8_lossy(&dirty.stderr)
    );

    let local = fixture
        .lev()
        .arg("--local")
        .arg("publish")
        .arg("v1.0.0")
        .arg("--no-sync")
        .arg("--no-build")
        .output()
        .unwrap();
    assert!(!local.status.success());
    assert!(
        String::from_utf8_lossy(&local.stderr).contains("real Git worktree"),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
}

#[test]
fn project_independent_tools_install_run_list_remove_and_reuse() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let revision = fixture.commit("def answer := 42\n", "tool release");
    let manifest = fixture.root.join("tool-manifest.json");
    fs::write(&manifest, fixture.registry_manifest(&revision, &revision)).unwrap();

    let installed = fixture
        .lev_at(&outside)
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &manifest)
        .arg("tool")
        .arg("install")
        .arg("dep")
        .arg("--git")
        .arg(&fixture.remote)
        .arg("--rev")
        .arg(&revision)
        .arg("--lean")
        .arg("leanprover/lean4:v4.test")
        .arg("--exe")
        .arg("dep-tool")
        .arg("--name")
        .arg("demo")
        .output()
        .unwrap();
    assert_success_ref(&installed);
    assert!(
        String::from_utf8_lossy(&installed.stdout).contains("lev tool run demo"),
        "{}",
        String::from_utf8_lossy(&installed.stdout)
    );
    let install_log = fs::read_to_string(&fixture.log).unwrap();
    assert!(install_log.contains("lake-update"), "{install_log}");
    assert!(install_log.contains("lake-build"), "{install_log}");

    let args_file = fixture.root.join("tool-args.txt");
    let run = fixture
        .lev_at(&outside)
        .env("LEV_TEST_TOOL_ARGS_FILE", &args_file)
        .arg("tool")
        .arg("run")
        .arg("demo")
        .arg("--offline")
        .arg("--")
        .arg("first")
        .arg("--second")
        .output()
        .unwrap();
    assert_success_ref(&run);
    assert_eq!(fs::read_to_string(&args_file).unwrap(), "first\n--second\n");

    let listed = fixture
        .lev_at(&outside)
        .arg("tool")
        .arg("list")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&listed);
    let listed = json_data(&listed, "lev.cli.tool.list/v1");
    assert_eq!(listed[0]["name"], "demo");
    assert_eq!(listed[0]["package"], "dep");
    assert_eq!(listed[0]["executable"], "dep-tool");

    let protected = fixture
        .lev_at(&outside)
        .arg("tool")
        .arg("gc")
        .arg("--max-age-days")
        .arg("0")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&protected);
    let protected = json_data(&protected, "lev.cli.tool.gc/v1");
    assert_eq!(protected["installed_tools"], 1);
    assert_eq!(protected["candidates"].as_array().unwrap().len(), 0);

    assert_success(
        fixture
            .lev_at(&outside)
            .arg("tool")
            .arg("remove")
            .arg("demo")
            .output()
            .unwrap(),
    );
    fs::write(&fixture.log, "").unwrap();
    let reused = fixture
        .lev_at(&outside)
        .env("LEV_TEST_TOOL_ARGS_FILE", &args_file)
        .arg("tool")
        .arg("run")
        .arg("dep")
        .arg("--git")
        .arg(&fixture.remote)
        .arg("--rev")
        .arg(&revision)
        .arg("--lean")
        .arg("leanprover/lean4:v4.test")
        .arg("--exe")
        .arg("dep-tool")
        .arg("--offline")
        .arg("--")
        .arg("cached")
        .output()
        .unwrap();
    assert_success_ref(&reused);
    assert_eq!(fs::read_to_string(&args_file).unwrap(), "cached\n");
    let reuse_log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!reuse_log.contains("lake-update"), "{reuse_log}");
    assert!(reuse_log.contains("lake-exe"), "{reuse_log}");

    let dry_run = fixture
        .lev_at(&outside)
        .arg("tool")
        .arg("gc")
        .arg("--max-age-days")
        .arg("0")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&dry_run);
    let dry_run = json_data(&dry_run, "lev.cli.tool.gc/v1");
    assert_eq!(dry_run["installed_tools"], 0);
    assert_eq!(dry_run["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(dry_run["applied"], false);
    let environment = PathBuf::from(dry_run["candidates"][0]["path"].as_str().unwrap());
    assert!(environment.join(".lev-last-used").is_file());
    assert!(environment.is_dir(), "dry-run removed {environment:?}");

    let collected = fixture
        .lev_at(&outside)
        .arg("tool")
        .arg("gc")
        .arg("--max-age-days")
        .arg("0")
        .arg("--apply")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&collected);
    let collected = json_data(&collected, "lev.cli.tool.gc/v1");
    assert_eq!(collected["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(collected["applied"], true);
    assert!(!environment.exists());

    let missing = fixture
        .lev_at(&outside)
        .arg("tool")
        .arg("run")
        .arg("dep")
        .arg("--git")
        .arg(&fixture.remote)
        .arg("--rev")
        .arg(&revision)
        .arg("--lean")
        .arg("leanprover/lean4:v4.test")
        .arg("--exe")
        .arg("dep-tool")
        .arg("--offline")
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("is not available offline"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn tool_package_names_do_not_infer_registry_scope_or_revision() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("generic-tool");
    fs::create_dir_all(&outside).unwrap();
    let revision = fixture.commit("def answer := 42\n", "generic tool");
    let manifest = fixture.root.join("generic-tool-manifest.json");
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "url": fixture.remote.to_string_lossy(),
                "type": "git",
                "rev": revision,
                "name": "ordinary-package"
            }],
            "name": "lev_tool_host"
        }))
        .unwrap(),
    )
    .unwrap();

    assert_success(
        fixture
            .lev_at(&outside)
            .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &manifest)
            .arg("tool")
            .arg("install")
            .arg("ordinary-package")
            .arg("--lean")
            .arg("leanprover/lean4:v4.test")
            .arg("--name")
            .arg("generic")
            .output()
            .unwrap(),
    );

    let environment = fs::read_dir(fixture.data.join("tools-v1/environments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let lakefile = fs::read_to_string(environment.join("lakefile.toml")).unwrap();
    assert!(
        lakefile.contains("name = \"ordinary-package\""),
        "{lakefile}"
    );
    assert!(!lakefile.contains("scope ="), "{lakefile}");
    assert!(!lakefile.contains("rev ="), "{lakefile}");
}

#[test]
fn self_update_checks_a_configurable_release_channel_without_a_project() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("update-outside");
    fs::create_dir_all(&outside).unwrap();
    let (api, server) = self_update_server("v9.9.9");

    let checked = fixture
        .lev_at(&outside)
        .env("LEV_UPDATE_API_URL", &api)
        .arg("self")
        .arg("update")
        .arg("--repository")
        .arg("example/lev")
        .arg("--check")
        .output()
        .unwrap();
    server.join().unwrap();
    assert_success_ref(&checked);
    let stdout = String::from_utf8_lossy(&checked.stdout);
    assert!(stdout.contains("current: 1.0.0"), "{stdout}");
    assert!(stdout.contains("selected: v9.9.9"), "{stdout}");
    assert!(stdout.contains("update available"), "{stdout}");
}

#[test]
fn inline_scripts_resolve_once_then_run_and_check_offline() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("standalone");
    fs::create_dir_all(&outside).unwrap();
    let revision = fixture.commit("def answer := 42\n", "script dependency");
    let manifest = fixture.root.join("script-manifest.json");
    fs::write(&manifest, fixture.registry_manifest(&revision, &revision)).unwrap();
    let script = outside.join("Demo.lean");
    fs::write(
        &script,
        format!(
            r#"-- /// lev
-- lean = "leanprover/lean4:v4.test"
--
-- [[dependencies]]
-- name = "dep"
-- git = "{}"
-- rev = "{}"
-- ///

def main : IO Unit := IO.println "standalone"
"#,
            fixture.remote.display(),
            revision
        ),
    )
    .unwrap();
    let args_file = fixture.root.join("script-args.txt");

    let first = fixture
        .lev_at(&outside)
        .env("LEV_TEST_LAKE_UPDATE_MANIFEST", &manifest)
        .env("LEV_TEST_SCRIPT_ARGS_FILE", &args_file)
        .arg("script")
        .arg("run")
        .arg(&script)
        .arg("--")
        .arg("first")
        .arg("--second")
        .output()
        .unwrap();
    assert_success_ref(&first);
    let first_args = fs::read_to_string(&args_file).unwrap();
    assert!(first_args.starts_with("--run\n"), "{first_args}");
    assert!(
        first_args.contains(&script.to_string_lossy().to_string()),
        "{first_args}"
    );
    assert!(first_args.ends_with("first\n--second\n"), "{first_args}");
    let first_log = fs::read_to_string(&fixture.log).unwrap();
    assert!(first_log.contains("lake-update"), "{first_log}");
    assert!(first_log.contains("lake-script"), "{first_log}");

    fs::write(&fixture.log, "").unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&script)
        .unwrap()
        .write_all(b"\n-- source-only edit\n")
        .unwrap();
    let reused = fixture
        .lev_at(&outside)
        .env("LEV_TEST_SCRIPT_ARGS_FILE", &args_file)
        .arg("script")
        .arg("run")
        .arg(&script)
        .arg("--offline")
        .arg("--")
        .arg("cached")
        .output()
        .unwrap();
    assert_success_ref(&reused);
    assert!(
        fs::read_to_string(&args_file)
            .unwrap()
            .ends_with("cached\n")
    );
    let reuse_log = fs::read_to_string(&fixture.log).unwrap();
    assert!(!reuse_log.contains("lake-update"), "{reuse_log}");

    let checked = fixture
        .lev_at(&outside)
        .env("LEV_TEST_SCRIPT_ARGS_FILE", &args_file)
        .arg("script")
        .arg("check")
        .arg(&script)
        .arg("--offline")
        .output()
        .unwrap();
    assert_success_ref(&checked);
    let check_args = fs::read_to_string(&args_file).unwrap();
    assert!(!check_args.contains("--run"), "{check_args}");
    assert_eq!(check_args.trim(), script.to_string_lossy());

    let status = fixture
        .lev_at(&outside)
        .arg("cache")
        .arg("status")
        .output()
        .unwrap();
    assert_success_ref(&status);
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("script environments: 1"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );

    let dry_run = fixture
        .lev_at(&outside)
        .arg("cache")
        .arg("gc")
        .arg("--max-age-days")
        .arg("0")
        .output()
        .unwrap();
    assert_success_ref(&dry_run);
    assert!(
        String::from_utf8_lossy(&dry_run.stdout).contains("script-environment"),
        "{}",
        String::from_utf8_lossy(&dry_run.stdout)
    );

    let environment = fs::read_dir(fixture.cache.join("scripts-v1/environments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(environment.join(".lev-last-used").is_file());
    assert!(environment.is_dir(), "dry-run removed {environment:?}");

    let collected = fixture
        .lev_at(&outside)
        .arg("cache")
        .arg("gc")
        .arg("--max-age-days")
        .arg("0")
        .arg("--apply")
        .output()
        .unwrap();
    assert_success_ref(&collected);
    assert!(!environment.exists());

    let missing = fixture
        .lev_at(&outside)
        .arg("script")
        .arg("check")
        .arg(&script)
        .arg("--offline")
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("is not available offline"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn matrix_initialization_is_guided_and_preserves_existing_configuration() {
    let fixture = Fixture::new();
    fs::write(
        fixture.project.join("lev.toml"),
        r#"# Keep this project task.
[tasks]
check = ["lake", "build", "--wfail"]
"#,
    )
    .unwrap();

    let missing = fixture.lev().arg("matrix").output().unwrap();
    assert_eq!(missing.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(stderr.contains("lev matrix --init"), "{stderr}");
    assert!(stderr.contains("lev matrix --lean VERSION_A"), "{stderr}");

    let initialized = fixture
        .lev()
        .arg("matrix")
        .arg("--init")
        .arg("--lean")
        .arg("4.fixture-c")
        .arg("--lean")
        .arg("4.fixture-d")
        .arg("--")
        .arg("lake")
        .arg("build")
        .arg("--wfail")
        .output()
        .unwrap();
    assert_success_ref(&initialized);

    let source = fs::read_to_string(fixture.project.join("lev.toml")).unwrap();
    assert!(source.contains("# Keep this project task."), "{source}");
    assert!(
        source.contains(r#"check = ["lake", "build", "--wfail"]"#),
        "{source}"
    );
    assert!(
        source.contains(
            r#"toolchains = ["leanprover/lean4:v4.fixture-c", "leanprover/lean4:v4.fixture-d"]"#
        ),
        "{source}"
    );
    assert!(
        source.contains(r#"command = ["lake", "build", "--wfail"]"#),
        "{source}"
    );

    let repeated = fixture.lev().arg("matrix").arg("--init").output().unwrap();
    assert_eq!(repeated.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&repeated.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("lev.toml")).unwrap(),
        source
    );
}

#[test]
fn matrix_preflights_every_toolchain_before_creating_a_workspace() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "first");
    fixture.write_manifest(&revision);

    let command_output = fixture.root.join("matrix-command-ran");
    let output = fixture
        .lev()
        .arg("matrix")
        .arg("--offline")
        .arg("--lean")
        .arg("4.fixture-b")
        .arg("--lean")
        .arg("missing-toolchain")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg("touch \"$LEV_TEST_MATRIX_COMMAND_OUT\"")
        .env("LEV_TEST_MATRIX_COMMAND_OUT", &command_output)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown toolchain"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!command_output.exists());
    assert!(!fixture.cache.join("workspaces-v1").exists());
}

#[test]
fn matrix_deps_completions_and_cache_gc_work_end_to_end() {
    let fixture = Fixture::new();
    let revision = fixture.commit("def answer := 42\n", "first");
    fixture.write_manifest(&revision);
    fs::write(
        fixture.project.join("lev.toml"),
        r#"
[matrix]
toolchains = ["4.fixture-a", "4.fixture-d"]

[tasks]
smoke = ["sh", "-c", "printf task-ok > \"$LEV_TEST_TASK_OUT\""]
"#,
    )
    .unwrap();

    let task_output = fixture.root.join("task.txt");
    assert_success(
        fixture
            .lev()
            .arg("task")
            .arg("smoke")
            .arg("--no-sync")
            .arg("--offline")
            .env("LEV_TEST_TASK_OUT", &task_output)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_to_string(task_output).unwrap(), "task-ok");
    let tasks = fixture.lev().arg("task").output().unwrap();
    assert_success_ref(&tasks);
    assert!(String::from_utf8_lossy(&tasks.stdout).contains("smoke"));

    let matrix_output = fixture.root.join("matrix.txt");
    let output = fixture
        .lev()
        .arg("matrix")
        .arg("--keep-going")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg("printf '%s|%s\\n' \"$ELAN_TOOLCHAIN\" \"$PWD\" >> \"$LEV_TEST_MATRIX_OUT\"")
        .env("LEV_TEST_MATRIX_OUT", &matrix_output)
        .output()
        .unwrap();
    assert_success(output);
    let matrix = fs::read_to_string(matrix_output).unwrap();
    let rows = matrix.lines().collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "{matrix}");
    let first = rows[0].split_once('|').unwrap();
    let second = rows[1].split_once('|').unwrap();
    assert_eq!(first.0, "leanprover/lean4:v4.fixture-a");
    assert_eq!(second.0, "leanprover/lean4:v4.fixture-d");
    assert_ne!(first.1, second.1);
    assert!(Path::new(first.1).starts_with(fixture.cache.join("workspaces-v1")));
    assert!(Path::new(second.1).starts_with(fixture.cache.join("workspaces-v1")));
    assert!(
        fs::symlink_metadata(Path::new(first.1).join(".lake/packages"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(Path::new(second.1).join(".lake/packages"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("lean-toolchain")).unwrap(),
        "leanprover/lean4:v4.test\n"
    );
    let cache_status = fixture.lev().arg("cache").arg("status").output().unwrap();
    assert_success_ref(&cache_status);
    let cache_status = String::from_utf8_lossy(&cache_status.stdout);
    assert!(
        cache_status.contains("shared dependency environments: 2"),
        "{cache_status}"
    );
    assert!(
        cache_status.contains("local workspaces: 2"),
        "{cache_status}"
    );

    let deps = fixture.lev().arg("deps").arg("--json").output().unwrap();
    assert_success_ref(&deps);
    let deps = json_data(&deps, "lev.cli.deps/v1");
    assert_eq!(deps["packages"][0]["name"], "dep");

    let tree = fixture.lev().arg("tree").output().unwrap();
    assert_success_ref(&tree);
    let tree = String::from_utf8_lossy(&tree.stdout);
    assert!(tree.contains("root"), "{tree}");
    assert!(tree.contains("`- dep"), "{tree}");

    let why = fixture.lev().arg("why").arg("dep").output().unwrap();
    assert_success_ref(&why);
    assert_eq!(String::from_utf8_lossy(&why.stdout).trim(), "root -> dep");

    let tree_json = fixture.lev().arg("tree").arg("--json").output().unwrap();
    assert_success_ref(&tree_json);
    let tree_json = json_data(&tree_json, "lev.cli.tree/v1");
    assert_eq!(tree_json["root"], "root");

    let why_json = fixture
        .lev()
        .arg("why")
        .arg("dep")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&why_json);
    assert_eq!(
        json_data(&why_json, "lev.cli.why/v1"),
        json!(["root", "dep"])
    );

    let completions = Command::new(env!("CARGO_BIN_EXE_lev"))
        .arg("completions")
        .arg("bash")
        .output()
        .unwrap();
    assert_success_ref(&completions);
    assert!(String::from_utf8_lossy(&completions.stdout).contains("_lev"));

    let artifact_cache = fixture.cache.join("lake-v1/test-toolchain");
    fs::create_dir_all(artifact_cache.join("artifacts")).unwrap();
    fs::create_dir_all(artifact_cache.join("outputs/root")).unwrap();
    fs::write(
        artifact_cache.join("artifacts/0123456789abcdef.olean"),
        "cached",
    )
    .unwrap();
    fs::write(
        artifact_cache.join("artifacts/fedcba9876543210.ilean"),
        "orphan",
    )
    .unwrap();
    fs::write(
        artifact_cache.join("outputs/root/1111111111111111.json"),
        r#"{"schemaVersion":"2026-02-25","service":null,
            "data":"0123456789abcdef.olean"}"#,
    )
    .unwrap();
    let artifact_status = fixture
        .lev()
        .arg("cache")
        .arg("artifacts")
        .arg("status")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&artifact_status);
    let artifact_status = json_data(&artifact_status, "lev.cli.cache.artifacts.status/v1");
    assert_eq!(artifact_status["stats"]["artifacts"], 2);
    assert_eq!(artifact_status["stats"]["unreferenced_artifacts"], 1);
    let artifact_verify = fixture
        .lev()
        .arg("cache")
        .arg("artifacts")
        .arg("verify")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&artifact_verify);
    let artifact_verify = json_data(&artifact_verify, "lev.cli.cache.artifacts.verify/v1");
    assert_eq!(artifact_verify["missing"], json!([]));

    let artifact_gc = fixture
        .lev()
        .arg("cache")
        .arg("artifacts")
        .arg("gc")
        .arg("--max-age-days")
        .arg("0")
        .arg("--apply")
        .arg("--json")
        .output()
        .unwrap();
    assert_success_ref(&artifact_gc);
    let artifact_gc = json_data(&artifact_gc, "lev.cli.cache.artifacts.gc/v1");
    assert_eq!(artifact_gc["applied"], true);
    assert_eq!(artifact_gc["candidates"].as_array().unwrap().len(), 1);
    assert!(
        !artifact_cache
            .join("artifacts/fedcba9876543210.ilean")
            .exists()
    );
    assert!(
        artifact_cache
            .join("artifacts/0123456789abcdef.olean")
            .exists()
    );

    let orphan = fixture.cache.join("lake-v1/orphan");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("artifact"), "unused").unwrap();
    let dry_run = fixture
        .lev()
        .arg("cache")
        .arg("gc")
        .arg("--max-age-days")
        .arg("0")
        .output()
        .unwrap();
    assert_success_ref(&dry_run);
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("orphan"));
    assert!(orphan.exists());

    assert_success(
        fixture
            .lev()
            .arg("cache")
            .arg("gc")
            .arg("--max-age-days")
            .arg("0")
            .arg("--apply")
            .output()
            .unwrap(),
    );
    assert!(!orphan.exists());
    assert_success(fixture.lev().arg("cache").arg("verify").output().unwrap());
}
