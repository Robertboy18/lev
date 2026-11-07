#![cfg(unix)]

//! End-to-end test of `lev check` on a tiny Lake project.
//!
//! The project's library has the modules Demo.A, Demo.B (imports A),
//! Demo.C (imports B) and the root Demo (imports C). They are `prelude`
//! files, so the export holds only `Init.Prelude` besides them. The test runs
//! the real elan, Lake, lev-export and kernels, and is skipped when elan, the
//! toolchain or bwrap is missing. It listens on no port.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The toolchain lev-export is written against; `LEV_CHECK_TOOLCHAIN`
/// overrides it.
const TOOLCHAIN: &str = "leanprover/lean4:v4.36.0-rc2";

fn toolchain() -> String {
    env::var("LEV_CHECK_TOOLCHAIN").unwrap_or_else(|_| TOOLCHAIN.to_owned())
}

/// Why the test cannot run here, if it cannot.
fn missing_prerequisite(toolchain: &str) -> Option<String> {
    let on_path = |program: &str| {
        env::var_os("PATH")
            .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    if !on_path("bwrap") {
        return Some("bwrap is not on PATH".to_owned());
    }
    if !on_path("elan") {
        return Some("elan is not on PATH".to_owned());
    }
    let installed = Command::new("elan")
        .args(["which", "lean"])
        .env("ELAN_TOOLCHAIN", toolchain)
        .output()
        .is_ok_and(|output| output.status.success());
    (!installed).then(|| format!("the toolchain {toolchain} is not installed"))
}

const LAKEFILE: &str = r#"name = "demo"
defaultTargets = ["Demo"]

[[lean_lib]]
name = "Demo"
"#;

const B: &str = r#"prelude
import Demo.A

namespace B

def quad (n : Nat) : Nat := A.double (A.double n)

theorem quad_eq (n : Nat) : Eq (quad n) (quad n) :=
  have _h := A.double_eq n
  rfl

def origin : A.Point := { x := 0, y := 0 }

def favorite : A.Color := A.Color.green

end B
"#;

const C: &str = r#"prelude
import Demo.B

namespace C

def octo (n : Nat) : Nat := B.quad (B.quad n)

theorem origin_x : Eq B.origin.x 0 := rfl

theorem octo_self (n : Nat) : Eq (octo n) (octo n) := B.quad_eq (B.quad n)

end C
"#;

/// Module A with a theorem, a definition, an inductive type and a structure;
/// `theorem` is the line declaring `A.double_eq`.
fn module_a(theorem: &str) -> String {
    format!(
        r#"prelude
import Init.Prelude

namespace A

inductive Color where
  | red
  | green
  | blue

structure Point where
  x : Nat
  y : Nat

def double (n : Nat) : Nat := Nat.add n n

{theorem}

end A
"#
    )
}

struct Project {
    root: PathBuf,
    cache: PathBuf,
}

impl Project {
    fn new(dir: &Path, toolchain: &str) -> Self {
        let root = dir.join("demo");
        fs::create_dir_all(root.join("Demo")).unwrap();
        fs::write(root.join("lean-toolchain"), format!("{toolchain}\n")).unwrap();
        fs::write(root.join("lakefile.toml"), LAKEFILE).unwrap();
        fs::write(root.join("Demo.lean"), "prelude\nimport Demo.C\n").unwrap();
        fs::write(root.join("Demo/B.lean"), B).unwrap();
        fs::write(root.join("Demo/C.lean"), C).unwrap();
        Self {
            root,
            cache: dir.join("cache"),
        }
    }

    /// Write module A with `theorem`, run `lev check --json`, and return the
    /// engine's report after checking that the development was accepted.
    fn check(&self, theorem: &str) -> Value {
        fs::write(self.root.join("Demo/A.lean"), module_a(theorem)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_lev"))
            .arg("--project")
            .arg(&self.root)
            .arg("--cache-dir")
            .arg(&self.cache)
            .args(["check", "--json", "--jobs", "2"])
            .args(["--kernel", "official", "--kernel", "nanoda"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let envelope: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(envelope["schema"], "lev.cli.check/v2");
        let data = &envelope["data"];
        assert_eq!(
            data["roots"],
            serde_json::json!(["Demo.A", "Demo.B", "Demo.C", "Demo"])
        );
        let report = data["check"].clone();
        assert_eq!(report["verdict"], "accept", "{report:#}");
        assert_eq!(report["violations"], serde_json::json!([]), "{report:#}");
        assert_eq!(report["fallbacks"], serde_json::json!([]), "{report:#}");
        eprintln!(
            "{} kernel runs, {} reused; seconds {}",
            report["slices"]["checked"], report["slices"]["reused"], data["seconds"]
        );
        report
    }
}

/// Modules with at least one kernel run that did not come from the cache.
fn rechecked(report: &Value) -> BTreeSet<String> {
    report["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|result| {
            result["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|attempt| attempt["cached"] == false)
        })
        .map(|result| result["module"].as_str().unwrap().to_owned())
        .collect()
}

fn modules(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn check_rechecks_only_the_slices_an_edit_changes() {
    let toolchain = toolchain();
    if let Some(reason) = missing_prerequisite(&toolchain) {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new()
        .prefix("lev-check-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let project = Project::new(temp.path(), &toolchain);

    let cold = project.check("theorem double_eq (n : Nat) : Eq (double n) (Nat.add n n) := rfl");
    assert_eq!(cold["checked_modules"], 5);
    assert_eq!(
        rechecked(&cold),
        modules(&["Demo", "Demo.A", "Demo.B", "Demo.C", "Init.Prelude"])
    );

    // A new proof of the same statement: only A's own slice changes.
    let proof = project.check(
        "theorem double_eq (n : Nat) : Eq (double n) (Nat.add n n) := Eq.refl (Nat.add n n)",
    );
    assert_eq!(rechecked(&proof), modules(&["Demo.A"]));

    // A new statement: B, which uses the theorem, is rechecked too. C sees
    // only B's statements, so its slice stays the same.
    let statement =
        project.check("theorem double_eq (n : Nat) : Eq (Nat.add n n) (double n) := rfl");
    assert_eq!(rechecked(&statement), modules(&["Demo.A", "Demo.B"]));

    let status = Command::new(env!("CARGO_BIN_EXE_lev"))
        .arg("--cache-dir")
        .arg(&project.cache)
        .args(["cache", "status", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    let check = &status["data"]["check_cache"];
    assert_eq!(check["exporters"]["entries"], 1, "{check}");
    assert_eq!(check["references"]["entries"], 1, "{check}");
    assert!(
        check["verdicts"]["entries"].as_u64().unwrap() >= 10,
        "{check}"
    );
    assert_eq!(check["scratch"]["entries"], 0, "{check}");
}
