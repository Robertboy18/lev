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
    }

    let workflow = root.join(".github/workflows/ci.yml");
    if workflow.is_file() {
        let source = fs::read_to_string(workflow).unwrap();
        assert!(source.contains("vars.LEV_CI_TOOLCHAINS"));
        assert!(!source.contains("matrix.lean"));
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
fn help_lists_the_four_commands_and_their_topics() {
    let lev = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_lev"))
            .args(args)
            .output()
            .unwrap()
    };

    let help = lev(&["--help"]);
    assert_success(&help);
    let help = stdout(&help);
    for command in ["inspect", "check", "cache", "doctor"] {
        assert!(help.contains(&format!("  {command} ")), "{help}");
    }
    for removed in ["sync", "build", "verify", "shake", "toolchain"] {
        assert!(!help.contains(&format!("  {removed} ")), "{help}");
    }

    let inspect = lev(&["inspect", "--help"]);
    assert_success(&inspect);
    assert!(
        stdout(&inspect).contains("  build "),
        "{}",
        stdout(&inspect)
    );

    let cache = lev(&["cache", "--help"]);
    assert_success(&cache);
    let cache = stdout(&cache);
    assert!(
        cache.contains("  status ") && cache.contains("  gc "),
        "{cache}"
    );

    let doctor = lev(&["doctor", "-h"]);
    assert_success(&doctor);
    assert!(stdout(&doctor).contains("Usage: lev doctor"));

    // A topic is required, so a bare `lev inspect` prints its help.
    let bare = lev(&["inspect"]);
    assert_eq!(bare.status.code(), Some(2));
    assert!(
        stderr(&bare).contains("Usage: lev inspect"),
        "{}",
        stderr(&bare)
    );
}

#[test]
fn check_exits_two_when_it_cannot_check() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();
    let output = fixture
        .lev_at(&outside)
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("no Lean project found"),
        "{}",
        stderr(&output)
    );

    // The fake toolchain cannot build lev-export.
    let output = fixture
        .lev()
        .args(["check", "--no-build", "--no-sandbox", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("lev-export did not build"),
        "{}",
        stderr(&output)
    );
}
