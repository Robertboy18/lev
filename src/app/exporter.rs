//! lev-export for `lev check`: its build per toolchain, the reference export
//! of the toolchain's own `Init`, and its run over a project.
//!
//! The exporter's Lean sources are compiled into lev. A check writes them to
//! an entry of lev's cache named after the toolchain and a hash of the
//! sources, and builds them there with `lake build` through elan. Everything
//! that loads `.olean` files, Lake's view of the project included, runs in the
//! same bwrap sandbox as the kernels unless `--no-sandbox` is given: the
//! project and the toolchain are read-only, only the output directory is
//! writable, and there is no network.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

use super::AppContext;
use crate::cache::check::{self as check_cache, CheckLayout};
use crate::cache::digest;
use crate::check::{Sandbox, sandbox_args};
use crate::core::lock;
use crate::core::process::{checked_output, describe};
use crate::project::Project;

/// The exporter package: its configuration and its one module.
const SOURCES: [(&str, &str); 2] = [
    (
        "lakefile.toml",
        include_str!("../../lean/export/lakefile.toml"),
    ),
    (
        "LevExport.lean",
        include_str!("../../lean/export/LevExport.lean"),
    ),
];

/// A built lev-export.
pub(super) struct Exporter {
    /// Cache entry name, shared by the build and its reference.
    pub(super) name: String,
    pub(super) binary: PathBuf,
}

/// The cache entry name for `toolchain`: the toolchain in file-name-safe
/// form and 16 hex digits of a digest over the toolchain and the sources.
pub(super) fn entry_name(toolchain: &str) -> String {
    let mut material = b"lev-export-build/1".to_vec();
    for part in
        std::iter::once(toolchain).chain(SOURCES.iter().flat_map(|(name, text)| [*name, *text]))
    {
        material.push(0);
        material.extend_from_slice(part.as_bytes());
    }
    let key = digest(&material);
    let readable = toolchain
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("{readable}-{}", &key[..16])
}

/// The exporter binary and the reference for `toolchain`, where an earlier
/// check built them.
pub(super) fn built(layout: &CheckLayout, toolchain: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let name = entry_name(toolchain);
    let dir = layout.exporters().join(&name);
    let binary = binary_in(&dir);
    let exporter =
        (binary.is_file() && dir.join(check_cache::LAST_USED).is_file()).then_some(binary);
    let reference = layout.references().join(&name);
    let reference = reference
        .join("manifest.json")
        .is_file()
        .then_some(reference);
    (exporter, reference)
}

fn binary_in(dir: &Path) -> PathBuf {
    dir.join(".lake")
        .join("build")
        .join("bin")
        .join(format!("lev-export{}", std::env::consts::EXE_SUFFIX))
}

/// The exporter for `toolchain`, built now unless an earlier check built it.
pub(super) fn ensure(
    context: &AppContext,
    layout: &CheckLayout,
    toolchain: &str,
) -> Result<Exporter> {
    let name = entry_name(toolchain);
    let dir = layout.exporters().join(&name);
    let binary = binary_in(&dir);
    let built = || binary.is_file() && dir.join(check_cache::LAST_USED).is_file();
    if !built() {
        let _lock = lock::exclusive(&layout.entry_lock(&name))?;
        if !built() {
            build(context, &dir, toolchain)?;
        }
    }
    check_cache::touch(&dir)?;
    Ok(Exporter { name, binary })
}

fn build(context: &AppContext, dir: &Path, toolchain: &str) -> Result<()> {
    // A directory without the stamp is an interrupted or failed build.
    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("failed to remove {}", dir.display()))?;
    }
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let toolchain_file = dir.join("lean-toolchain");
    fs::write(&toolchain_file, format!("{toolchain}\n"))
        .with_context(|| format!("failed to write {}", toolchain_file.display()))?;
    for (name, text) in SOURCES {
        fs::write(dir.join(name), text)
            .with_context(|| format!("failed to write {}", dir.join(name).display()))?;
    }
    context.info(format!("building lev-export for {toolchain}"));
    let mut command = context.elan_run(toolchain, "lake", false);
    command.arg("build").current_dir(dir).stdin(Stdio::null());
    context.detail(format!("running {}", describe(&command)));
    checked_output(&mut command).with_context(|| {
        format!(
            "lev-export did not build with {toolchain}; \
             its Lean code may not support this toolchain"
        )
    })?;
    if !binary_in(dir).is_file() {
        bail!(
            "lake build in {} succeeded but wrote no lev-export binary",
            dir.display()
        );
    }
    check_cache::touch(dir)
}

/// The reference for the checks: lev-export's export of the toolchain's own
/// `Init`, made once per exporter build.
pub(super) fn reference(
    context: &AppContext,
    layout: &CheckLayout,
    exporter: &Exporter,
    toolchain: &Path,
    sandbox: &Sandbox,
    jobs: usize,
) -> Result<PathBuf> {
    let dir = layout.references().join(&exporter.name);
    let ready = || dir.join("manifest.json").is_file();
    if !ready() {
        let _lock = lock::exclusive(&layout.entry_lock(&format!("reference-{}", exporter.name)))?;
        if !ready() {
            context.info("exporting the toolchain's Init as the reference");
            let scratch = scratch_dir(layout, "reference-")?;
            let out = scratch.path().join("export");
            let mut args = vec![OsString::from("--out"), out.clone().into()];
            args.extend(["--jobs".into(), jobs.to_string().into(), "Init".into()]);
            let run = Run {
                program: exporter.binary.clone(),
                args,
                env: vec![
                    ("LEAN_PATH", toolchain.join("lib").join("lean").into()),
                    ("LEAN_SYSROOT", toolchain.into()),
                ],
                readable: vec![toolchain.to_owned(), exporter.binary.clone()],
                writable: vec![scratch.path().to_owned()],
                cwd: scratch.path().to_owned(),
            };
            let output = run.output(sandbox)?;
            context.detail(String::from_utf8_lossy(&output.stderr).trim_end());
            if dir.exists() {
                fs::remove_dir_all(&dir)
                    .with_context(|| format!("failed to remove {}", dir.display()))?;
            }
            let references = layout.references();
            fs::create_dir_all(&references)
                .with_context(|| format!("failed to create {}", references.display()))?;
            fs::rename(&out, &dir)
                .with_context(|| format!("failed to move the reference to {}", dir.display()))?;
        }
    }
    check_cache::touch(&dir)?;
    Ok(dir)
}

/// A new directory below the check cache's scratch space, removed on drop.
pub(super) fn scratch_dir(layout: &CheckLayout, prefix: &str) -> Result<tempfile::TempDir> {
    let scratch = layout.scratch();
    fs::create_dir_all(&scratch)
        .with_context(|| format!("failed to create {}", scratch.display()))?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&scratch)
        .with_context(|| format!("failed to create a directory in {}", scratch.display()))
}

/// The project as the Lake commands of a check see it.
pub(super) struct Workspace {
    root: PathBuf,
    lake: PathBuf,
    /// The toolchain and every package directory, bound read-only.
    readable: Vec<PathBuf>,
}

impl Workspace {
    pub(super) fn new(project: &Project, toolchain: &Path) -> Result<Self> {
        let mut readable = vec![toolchain.to_owned(), project.root.clone()];
        for package in project.packages()? {
            // A package checked out through a symbolic link is bound at both
            // places, so the link resolves inside the sandbox too.
            if let Ok(target) = fs::canonicalize(&package.root) {
                readable.push(target);
                readable.push(package.root);
            }
        }
        readable.sort();
        readable.dedup();
        Ok(Self {
            root: project.root.clone(),
            lake: toolchain
                .join("bin")
                .join(format!("lake{}", std::env::consts::EXE_SUFFIX)),
            readable,
        })
    }

    /// The modules of the root package's default targets, the modules
    /// `lake check` exports: `lake query :defaultModules`.
    pub(super) fn default_modules(&self, sandbox: &Sandbox) -> Result<Vec<String>> {
        let args = ["query", "--json", ":defaultModules"].map(OsString::from);
        let output = self
            .lake(args.to_vec(), Vec::new(), Vec::new())
            .output(sandbox)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("[]");
        let modules: Vec<String> = serde_json::from_str(line).with_context(|| {
            format!("`lake query --json :defaultModules` printed {line:?}, not a list of modules")
        })?;
        if modules.is_empty() {
            bail!(
                "the root package has no default target with Lean modules, so there is nothing \
                 to check; list a library in `defaultTargets`"
            );
        }
        Ok(modules)
    }

    /// `lake env <lev-export> --out <out> --jobs <jobs> <roots>` in the project.
    pub(super) fn export(
        &self,
        context: &AppContext,
        exporter: &Exporter,
        roots: &[String],
        out: &Path,
        jobs: usize,
        sandbox: &Sandbox,
    ) -> Result<()> {
        let mut args = vec![
            OsString::from("env"),
            exporter.binary.clone().into(),
            "--out".into(),
            out.into(),
            "--jobs".into(),
            jobs.to_string().into(),
        ];
        args.extend(roots.iter().map(OsString::from));
        let run = self.lake(args, vec![exporter.binary.clone()], vec![out.to_owned()]);
        let output = run.output(sandbox)?;
        context.detail(String::from_utf8_lossy(&output.stderr).trim_end());
        Ok(())
    }

    fn lake(&self, args: Vec<OsString>, readable: Vec<PathBuf>, writable: Vec<PathBuf>) -> Run {
        Run {
            program: self.lake.clone(),
            args,
            // Lake finds git and the toolchain's tools on the caller's PATH.
            env: vec![("PATH", std::env::var_os("PATH").unwrap_or_default())],
            readable: self.readable.iter().cloned().chain(readable).collect(),
            writable,
            cwd: self.root.clone(),
        }
    }
}

/// One exporter or Lake process.
struct Run {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(&'static str, OsString)>,
    readable: Vec<PathBuf>,
    writable: Vec<PathBuf>,
    cwd: PathBuf,
}

impl Run {
    /// Run to completion, in the sandbox unless there is none, with panics
    /// aborting as `lake check` sets it. A failure is an error that quotes
    /// the output.
    fn output(&self, sandbox: &Sandbox) -> Result<Output> {
        let abort = ("LEAN_ABORT_ON_PANIC", OsString::from("1"));
        let env = self
            .env
            .iter()
            .chain(std::iter::once(&abort))
            .map(|(name, value)| (*name, value.as_os_str()))
            .collect::<Vec<_>>();
        let mut command = match sandbox {
            Sandbox::Bwrap(bwrap) => {
                let mut command = Command::new(bwrap);
                command
                    .args(sandbox_args(
                        &env,
                        &self.readable,
                        &self.writable,
                        &self.cwd,
                    ))
                    .args([
                        "--",
                        "/bin/sh",
                        "-c",
                        "ulimit -c 0; exec \"$@\"",
                        "lev-sandbox",
                    ])
                    .arg(&self.program)
                    .args(&self.args);
                command
            }
            Sandbox::None => {
                let mut command = Command::new(&self.program);
                command
                    .args(&self.args)
                    .envs(env.iter().copied())
                    .current_dir(&self.cwd);
                command
            }
        };
        command.stdin(Stdio::null());
        checked_output(&mut command)
    }
}
