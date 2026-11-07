use super::*;

fn problems(report: &Value) -> Vec<String> {
    report["problems"]
        .as_array()
        .unwrap()
        .iter()
        // Whether the host has bwrap is outside the fixture's control.
        .filter(|problem| !problem["message"].as_str().unwrap().contains("bwrap"))
        .map(|problem| {
            format!(
                "{}: {}",
                problem["severity"].as_str().unwrap(),
                problem["message"].as_str().unwrap()
            )
        })
        .collect()
}

#[test]
fn doctor_states_each_fact_once() {
    let fixture = Fixture::new();
    for kernel in ["leanchecker", "lean4lean"] {
        executable(
            &fixture.toolchain.join("bin").join(kernel),
            "#!/bin/sh\nexit 0\n",
        );
    }

    let output = fixture.lev().args(["doctor", "--json"]).output().unwrap();
    assert_success(&output);
    let report = json_data(&output, "lev.cli.doctor/v2");
    assert_eq!(report["lev_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        PathBuf::from(report["project"]["root"].as_str().unwrap()),
        fixture.project
    );
    assert_eq!(report["project"]["toolchain"], TEST_TOOLCHAIN);
    assert_eq!(report["elan"], "elan fake");
    assert_eq!(report["lean"], format!("Lean fake ({TEST_TOOLCHAIN})"));
    assert_eq!(report["lake"], format!("Lake fake ({TEST_TOOLCHAIN})"));
    assert!(
        report["git"].as_str().unwrap().starts_with("git version"),
        "{report}"
    );
    assert_eq!(
        report["kernels"],
        json!({
            "lean4lean": fixture.toolchain.join("bin/lean4lean"),
            "leanchecker": fixture.toolchain.join("bin/leanchecker"),
            "leanchecker-paranoid": null,
            "nanoda_bin": null,
        })
    );
    assert_eq!(
        report["lake_cache"],
        json!({
            "path": fixture.lake_cache(),
            "source": "toolchain",
            "exists": false,
            "same_filesystem": true,
        })
    );
    assert_eq!(
        PathBuf::from(report["lev_cache"]["path"].as_str().unwrap()),
        fixture.cache
    );
    assert_eq!(report["lev_cache"]["registered_projects"], 0);
    assert_eq!(problems(&report), Vec::<String>::new());
    assert!(
        !fixture.cache.exists(),
        "doctor must not create cache state"
    );

    let text = fixture.lev().arg("doctor").output().unwrap();
    assert_success(&text);
    let text = stdout(&text);
    assert!(text.starts_with("lev 2."), "{text}");
    assert!(
        text.contains(&format!("lean        Lean fake ({TEST_TOOLCHAIN})\n")),
        "{text}"
    );
    assert!(
        text.contains(
            "kernels     lean4lean, leanchecker (missing leanchecker-paranoid, nanoda_bin)\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("(toolchain default, same filesystem as the project, not created yet)"),
        "{text}"
    );
    assert!(text.contains("(0 registered projects)"), "{text}");
}

#[test]
fn doctor_reports_a_missing_toolchain_and_an_explicit_lake_cache() {
    let fixture = Fixture::new();
    fs::write(
        fixture.project.join("lean-toolchain"),
        "leanprover/lean4:v4.absent\n",
    )
    .unwrap();
    let shared = fixture.root.join("shared-lake-cache");

    let output = fixture
        .lev()
        .args(["doctor", "--json"])
        .env("LAKE_CACHE_DIR", &shared)
        .output()
        .unwrap();
    assert_success(&output);
    let report = json_data(&output, "lev.cli.doctor/v2");
    assert!(report["lean"].is_null());
    assert!(report["lake"].is_null());
    assert_eq!(report["kernels"], json!({}));
    assert_eq!(report["lake_cache"]["path"], json!(shared));
    assert_eq!(report["lake_cache"]["source"], "environment");
    // One missing toolchain is one problem, not one per fact it hides.
    let problems = problems(&report);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("error: "), "{problems:?}");
    assert!(problems[0].contains("not installed"), "{problems:?}");
}

#[test]
fn doctor_outside_a_project_reports_only_machine_facts() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();

    let output = fixture
        .lev_at(&outside)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert_success(&output);
    let report = json_data(&output, "lev.cli.doctor/v2");
    assert!(report["project"].is_null());
    assert!(report["lean"].is_null());
    assert!(report["lake_cache"].is_null());
    assert_eq!(report["elan"], "elan fake");
    assert_eq!(problems(&report), Vec::<String>::new());

    let text = stdout(&fixture.lev_at(&outside).arg("doctor").output().unwrap());
    assert!(text.contains("project     none found\n"), "{text}");
    assert!(!text.contains("\nlean "), "{text}");
}

#[test]
fn doctor_reports_an_unreadable_registry_without_failing() {
    let fixture = Fixture::new();
    let registry = fixture.cache.join("projects-v2");
    fs::create_dir_all(&registry).unwrap();
    fs::write(registry.join("broken.json"), b"{not json").unwrap();

    let output = fixture.lev().args(["doctor", "--json"]).output().unwrap();
    assert_success(&output);
    let report = json_data(&output, "lev.cli.doctor/v2");
    assert!(report["lev_cache"]["registered_projects"].is_null());
    let problems = problems(&report);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("warning: "), "{problems:?}");
    assert!(problems[0].contains("broken.json"), "{problems:?}");
}
