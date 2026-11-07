//! External kernels: invocation, sandbox, exit codes and identity.
//!
//! Each run goes through a `bwrap` sandbox modeled on Lake's
//! `runExternalKernel` (`Lake/CLI/Check.lean`): the root is bound read-only,
//! home directories, `/tmp`, `/run` and `/var` are covered with tmpfs, only the
//! toolchain, the slice and the config are bound back (read-only), the
//! environment is cleared and every namespace, the network included, is
//! unshared. Core dumps are off for every kernel. con-leche and con-ron
//! decline every non-standard axiom (exit 2), so they cannot check slices.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::hash::Node;
use crate::cache::file_digest;

/// A kernel that can check slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KernelKind {
    /// `leanchecker --from-export`, Lean's C++ kernel through `Lean.Replay`.
    Official,
    /// `leanchecker-paranoid --from-export`: the same checker, statically linked.
    Paranoid,
    /// `lean4lean --import`.
    Lean4lean,
    /// `nanoda_bin` with a config that permits exactly the slice's axioms.
    Nanoda,
}

impl KernelKind {
    pub const ALL: [KernelKind; 4] = [
        KernelKind::Official,
        KernelKind::Paranoid,
        KernelKind::Lean4lean,
        KernelKind::Nanoda,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Paranoid => "paranoid",
            Self::Lean4lean => "lean4lean",
            Self::Nanoda => "nanoda",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    fn binary(self) -> &'static str {
        match self {
            Self::Official => "leanchecker",
            Self::Paranoid => "leanchecker-paranoid",
            Self::Lean4lean => "lean4lean",
            Self::Nanoda => "nanoda_bin",
        }
    }

    /// Argument and config template; part of the kernel identity.
    fn template(self) -> &'static str {
        match self {
            Self::Official => "official/1: leanchecker --from-export SLICE; LEAN_ABORT_ON_PANIC=1",
            Self::Paranoid => {
                "paranoid/1: leanchecker-paranoid --from-export SLICE; LEAN_ABORT_ON_PANIC=1"
            }
            Self::Lean4lean => "lean4lean/1: lean4lean --import SLICE; no LEAN_ABORT_ON_PANIC",
            Self::Nanoda => {
                "nanoda/1: nanoda_bin CONFIG; one thread, nat and string extensions, \
                 permitted = standard axioms, axiomized imports and own axioms, hard error, \
                 no axiom printing"
            }
        }
    }
}

/// What a kernel said about a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Accept,
    Reject,
    /// The kernel declined the input without judging it.
    Decline,
    /// The run failed: a timeout, a signal, an unexpected exit code, or no run.
    Error,
}

/// One kernel run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Run {
    pub(crate) verdict: Verdict,
    pub(crate) exit_code: Option<i32>,
    pub(crate) seconds: f64,
    pub(crate) message: String,
}

/// A located kernel binary with its identity.
#[derive(Debug, Clone)]
pub(crate) struct Kernel {
    pub(crate) kind: KernelKind,
    pub(crate) exe: PathBuf,
    /// Toolchain root, bound into the sandbox for the kernel's libraries.
    pub(crate) prefix: PathBuf,
    /// SHA-256 of the binary (and the toolchain libraries it loads) plus the template.
    pub(crate) identity: String,
}

/// Where leanchecker finds the toolchain's shared libraries: its runpath on
/// Linux and macOS (`lib`, then `lib/lean`), and its own directory on Windows.
const LIBRARY_DIRS: [&str; 3] = ["lib", "lib/lean", "bin"];

/// Shared libraries on Linux, macOS and Windows.
const LIBRARY_EXTENSIONS: [&str; 3] = ["so", "dylib", "dll"];

impl Kernel {
    pub(crate) fn locate(kind: KernelKind, toolchain: &Path) -> Result<Self> {
        let exe = toolchain.join("bin").join(kind.binary());
        if !exe.is_file() {
            bail!(
                "the {} kernel {} does not exist",
                kind.as_str(),
                exe.display()
            );
        }
        let mut node = Node::new("lev-kernel/2");
        node.text(kind.as_str())
            .text(kind.template())
            .text(&file_digest(&exe)?);
        // leanchecker is a small front end over the toolchain's shared libraries.
        if kind == KernelKind::Official {
            for (library, digest) in shared_libraries(toolchain)? {
                node.text(&library).text(&digest);
            }
        }
        Ok(Self {
            kind,
            exe,
            prefix: toolchain.to_path_buf(),
            identity: crate::core::hex::lowercase(&node.finish()),
        })
    }

    /// Check one slice file. `axioms` are the axioms the slice may declare.
    pub(crate) fn run(
        &self,
        slice: &Path,
        axioms: &[String],
        work: &Path,
        sandbox: &Sandbox,
        timeout: Duration,
    ) -> Result<Run> {
        let stem = slice
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let kind = self.kind.as_str();
        let mut readable = vec![self.prefix.clone(), slice.to_path_buf()];
        let args: Vec<OsString> = match self.kind {
            // Lake passes --silent too, which only hides the verdict line and,
            // with it, the kernel's error on a rejection (LeanChecker.lean).
            KernelKind::Official | KernelKind::Paranoid => {
                vec!["--from-export".into(), slice.into()]
            }
            KernelKind::Lean4lean => vec!["--import".into(), slice.into()],
            KernelKind::Nanoda => {
                let config = work.join(format!("{stem}.nanoda.json"));
                fs::write(&config, nanoda_config(slice, axioms)?)
                    .with_context(|| format!("failed to write {}", config.display()))?;
                readable.push(config.clone());
                vec![config.into()]
            }
        };
        let env: &[(&str, &str)] = match self.kind {
            // As Lake runs it: a panic is never an acceptance.
            KernelKind::Official | KernelKind::Paranoid => &[("LEAN_ABORT_ON_PANIC", "1")],
            // Its rejection path panics while formatting the error; aborting hangs it.
            KernelKind::Lean4lean | KernelKind::Nanoda => &[],
        };
        let mut command = sandbox.command(&self.exe, &args, env, &readable, work);
        let stdout_path = work.join(format!("{stem}.{kind}.out"));
        let stderr_path = work.join(format!("{stem}.{kind}.err"));
        command
            .stdin(Stdio::null())
            .stdout(File::create(&stdout_path)?)
            .stderr(File::create(&stderr_path)?);

        let started = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start the {kind} kernel"))?;
        let mut pause = Duration::from_millis(1);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(50));
        };
        let seconds = started.elapsed().as_secs_f64();
        let output = [tail(&stderr_path)?, tail(&stdout_path)?]
            .into_iter()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = fs::remove_file(&stdout_path);
        let _ = fs::remove_file(&stderr_path);
        let Some(status) = status else {
            return Ok(Run {
                verdict: Verdict::Error,
                exit_code: None,
                seconds,
                message: format!("timed out after {} s", timeout.as_secs()),
            });
        };
        Ok(Run {
            verdict: verdict(self.kind, status),
            exit_code: status.code(),
            seconds,
            message: output,
        })
    }
}

/// Every shared library in [`LIBRARY_DIRS`], by its path in the toolchain,
/// with the SHA-256 of what it points to. Files that several links name are
/// read once.
fn shared_libraries(toolchain: &Path) -> Result<Vec<(String, String)>> {
    let mut digests: HashMap<PathBuf, String> = HashMap::new();
    let mut libraries = Vec::new();
    for dir in LIBRARY_DIRS {
        let path = toolchain.join(dir);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to list {}", path.display()));
            }
        };
        for entry in entries {
            let file = entry
                .with_context(|| format!("failed to list {}", path.display()))?
                .path();
            let library = file
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| LIBRARY_EXTENSIONS.contains(&extension));
            if !library || !file.is_file() {
                continue;
            }
            let target = fs::canonicalize(&file)
                .with_context(|| format!("failed to resolve {}", file.display()))?;
            let digest = match digests.entry(target) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => {
                    let digest = file_digest(entry.key())?;
                    entry.insert(digest).clone()
                }
            };
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            libraries.push((format!("{dir}/{name}"), digest));
        }
    }
    libraries.sort();
    Ok(libraries)
}

/// Map an exit status to a verdict.
///
/// leanchecker and lean4lean exit 1 both for a kernel rejection and for input
/// they cannot parse. nanoda panics (101) on type errors and returns an error
/// (1) for unpermitted axioms and malformed input; 2 is its "declined"
/// back-reference check. Exit codes from 128 up are signals relayed by bwrap.
pub(crate) fn verdict(kind: KernelKind, status: ExitStatus) -> Verdict {
    match (kind, status.code()) {
        (_, Some(0)) => Verdict::Accept,
        (_, Some(1)) => Verdict::Reject,
        (KernelKind::Nanoda, Some(101)) => Verdict::Reject,
        (KernelKind::Nanoda, Some(2)) => Verdict::Decline,
        _ => Verdict::Error,
    }
}

pub(crate) fn nanoda_config(slice: &Path, axioms: &[String]) -> Result<Vec<u8>> {
    let mut permitted: Vec<&str> = super::link::STANDARD_AXIOMS.to_vec();
    permitted.extend(axioms.iter().map(String::as_str));
    permitted.sort_unstable();
    permitted.dedup();
    let config = serde_json::json!({
        "export_file_path": slice,
        "use_stdin": false,
        "num_threads": 1,
        "nat_extension": true,
        "string_extension": true,
        "permitted_axioms": permitted,
        "unpermitted_axiom_hard_error": true,
        "print_success_message": true,
        "print_axioms": false,
    });
    Ok(serde_json::to_vec(&config)?)
}

/// How kernel processes are isolated.
#[derive(Debug, Clone)]
pub(crate) enum Sandbox {
    Bwrap(PathBuf),
    None,
}

impl Sandbox {
    /// `bwrap` from `PATH`; there is no silent fallback to running unsandboxed.
    pub(crate) fn bwrap() -> Result<Self> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join("bwrap"))
            .find(|candidate| candidate.is_file())
            .map(Self::Bwrap)
            .context(
                "the exporter and the kernels run in a bwrap sandbox and bwrap is not \
                 on PATH; install bubblewrap, or pass --no-sandbox to run them unsandboxed",
            )
    }

    /// `/bin/sh -c 'ulimit -c 0; exec "$@"'` around the kernel, inside the sandbox.
    pub(crate) fn command(
        &self,
        exe: &Path,
        args: &[OsString],
        env: &[(&str, &str)],
        readable: &[PathBuf],
        work: &Path,
    ) -> Command {
        let mut inner: Vec<OsString> = vec![
            "/bin/sh".into(),
            "-c".into(),
            "ulimit -c 0; exec \"$@\"".into(),
            "lev-kernel".into(),
            exe.into(),
        ];
        inner.extend(args.iter().cloned());
        match self {
            Self::Bwrap(bwrap) => {
                let env = env
                    .iter()
                    .map(|(name, value)| (*name, OsStr::new(value)))
                    .collect::<Vec<_>>();
                let mut command = Command::new(bwrap);
                command
                    .args(sandbox_args(&env, readable, &[], Path::new("/tmp")))
                    .arg("--")
                    .args(inner);
                command
            }
            Self::None => {
                let mut command = Command::new(&inner[0]);
                command
                    .args(&inner[1..])
                    .env_clear()
                    .env("HOME", work)
                    .envs(env.iter().copied())
                    .current_dir(work);
                command
            }
        }
    }
}

/// bwrap arguments in the order Lake's `buildSandboxArgs` uses: everything
/// read-only, home directories and scratch space covered, `readable` bound
/// back read-only and `writable` read-write, a cleared environment with
/// `env`, no network, and `chdir` as the working directory.
pub(crate) fn sandbox_args(
    env: &[(&str, &OsStr)],
    readable: &[PathBuf],
    writable: &[PathBuf],
    chdir: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
        "/home",
        "--tmpfs",
        "/root",
        "--tmpfs",
        "/run/user",
        "--tmpfs",
        "/tmp",
        "--dir",
        "/tmp/home",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--clearenv",
        "--tmpfs",
        "/run",
        "--tmpfs",
        "/var",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for path in readable {
        args.extend(["--ro-bind".into(), path.into(), path.into()]);
    }
    for path in writable {
        args.extend(["--bind".into(), path.into(), path.into()]);
    }
    for (name, value) in env {
        args.extend(["--setenv".into(), (*name).into(), (*value).into()]);
    }
    args.extend(
        [
            "--setenv",
            "HOME",
            "/tmp/home",
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--chdir",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args.push(chdir.into());
    args
}

/// The last few KiB of a kernel's output.
fn tail(path: &Path) -> Result<String> {
    const LIMIT: u64 = 4096;
    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let length = file.metadata()?.len();
    if length > LIMIT {
        file.seek(SeekFrom::Start(length - LIMIT))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{Kernel, KernelKind, nanoda_config, sandbox_args};

    #[test]
    fn the_official_identity_covers_the_shared_libraries_of_every_platform() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toolchain = dir.path();
        fs::create_dir_all(toolchain.join("bin")).expect("bin");
        fs::create_dir_all(toolchain.join("lib/lean")).expect("lib");
        fs::write(toolchain.join("bin/leanchecker"), "launcher").expect("launcher");
        let identity = || {
            Kernel::locate(KernelKind::Official, toolchain)
                .expect("kernel")
                .identity
        };
        for library in [
            "lib/lean/libleanshared.so",
            "lib/lean/libleanshared.dylib",
            "lib/libleanshared.dylib",
            "bin/libleanshared.dll",
        ] {
            let before = identity();
            let path = toolchain.join(library);
            fs::write(&path, "one").expect("library");
            let added = identity();
            assert_ne!(added, before, "{library}");
            fs::write(&path, "two").expect("library");
            assert_ne!(identity(), added, "{library}");
        }
        // Other files do not count, and nanoda loads no shared library.
        let current = identity();
        fs::write(toolchain.join("lib/lean/Init.olean"), "olean").expect("olean");
        assert_eq!(identity(), current);
        fs::write(toolchain.join("bin/nanoda_bin"), "nanoda").expect("nanoda");
        let nanoda = || {
            Kernel::locate(KernelKind::Nanoda, toolchain)
                .expect("kernel")
                .identity
        };
        let before = nanoda();
        fs::write(toolchain.join("lib/lean/libleanshared.so"), "three").expect("library");
        assert_eq!(nanoda(), before);
    }

    #[cfg(unix)]
    #[test]
    fn exit_codes_map_to_verdicts() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        use super::{Verdict, verdict};

        let code = |code: i32| ExitStatus::from_raw(code << 8);
        let cases = [
            (KernelKind::Official, code(0), Verdict::Accept),
            (KernelKind::Official, code(1), Verdict::Reject),
            (KernelKind::Paranoid, code(2), Verdict::Error),
            (KernelKind::Official, code(134), Verdict::Error),
            (KernelKind::Lean4lean, code(1), Verdict::Reject),
            (KernelKind::Nanoda, code(101), Verdict::Reject),
            (KernelKind::Nanoda, code(1), Verdict::Reject),
            (KernelKind::Nanoda, code(2), Verdict::Decline),
            (
                KernelKind::Lean4lean,
                ExitStatus::from_raw(9),
                Verdict::Error,
            ),
        ];
        for (kind, status, expected) in cases {
            assert_eq!(verdict(kind, status), expected, "{kind:?} {status:?}");
        }
    }

    #[test]
    fn sandbox_arguments_follow_lake() {
        let slice = PathBuf::from("/work/0-exported.ndjson");
        let args: Vec<String> = sandbox_args(
            &[("LEAN_ABORT_ON_PANIC", OsStr::new("1"))],
            &[slice],
            &[PathBuf::from("/out")],
            Path::new("/project"),
        )
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
        assert_eq!(args[..3], ["--ro-bind", "/", "/"]);
        let has = |window: &[&str]| args.windows(window.len()).any(|w| w == window);
        assert!(has(&["--tmpfs", "/home"]));
        assert!(has(&["--tmpfs", "/tmp"]));
        assert!(has(&["--tmpfs", "/var"]));
        assert!(has(&[
            "--ro-bind",
            "/work/0-exported.ndjson",
            "/work/0-exported.ndjson"
        ]));
        assert!(has(&["--bind", "/out", "/out"]));
        assert!(has(&["--chdir", "/project"]));
        assert!(has(&["--setenv", "LEAN_ABORT_ON_PANIC", "1"]));
        assert!(has(&["--clearenv"]));
        assert!(has(&["--unshare-all"]));
        assert!(has(&["--die-with-parent"]));
        assert!(!has(&["--share-net"]));
        let clear = args.iter().position(|arg| arg == "--clearenv");
        let setenv = args.iter().position(|arg| arg == "--setenv");
        assert!(clear < setenv);
    }

    #[test]
    fn nanoda_permits_exactly_the_standard_and_axiomized_names() {
        let config: serde_json::Value = serde_json::from_slice(
            &nanoda_config(
                Path::new("/work/s.ndjson"),
                &["Foo.t".to_owned(), "propext".to_owned()],
            )
            .expect("config"),
        )
        .expect("json");
        assert_eq!(
            config["permitted_axioms"],
            serde_json::json!(["Classical.choice", "Foo.t", "Quot.sound", "propext"])
        );
        assert_eq!(config["export_file_path"], "/work/s.ndjson");
        assert_eq!(config["unpermitted_axiom_hard_error"], true);
        assert_eq!(config["use_stdin"], false);
        assert_eq!(config["num_threads"], 1);
        assert_eq!(config["nat_extension"], true);
        assert_eq!(config["string_extension"], true);
        assert_eq!(config["print_success_message"], true);
        assert!(config.get("unsafe_permit_all_axioms").is_none());
    }

    #[test]
    fn kernel_names_round_trip() {
        for kind in KernelKind::ALL {
            assert_eq!(KernelKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(KernelKind::parse("con-leche"), None);
    }
}
