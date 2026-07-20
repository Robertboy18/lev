//! Checksum-verified updates from lev's GitHub release channel.
//!
//! The updater accepts only the asset for the current platform, checks the
//! advertised byte count and detached SHA-256 file, then atomically replaces
//! a regular executable. Redirected download URLs still have to use HTTPS.

use std::fs;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::cli::{SelfCommand, SelfUninstallArgs, SelfUpdateArgs};
use crate::core::atomic_file::replace_executable;
use crate::core::bounded_io;
use crate::core::http_url::HttpUrl;

use super::AppContext;

const GITHUB_API: &str = "https://api.github.com";
const MAX_RELEASE_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

pub(super) fn self_command(context: &AppContext, command: SelfCommand) -> Result<i32> {
    match command {
        SelfCommand::Update(args) => update(context, args),
        SelfCommand::Uninstall(args) => uninstall(context, args),
    }
}

fn uninstall(context: &AppContext, args: SelfUninstallArgs) -> Result<i32> {
    let executable = std::env::current_exe().context("failed to locate the running lev binary")?;
    require_regular_executable(&executable)?;
    let cargo = cargo_uninstaller(&executable);
    let data = context.store.root.parent().unwrap_or(&context.store.root);

    if args.dry_run {
        println!("executable: {}", executable.display());
        match &cargo {
            Some(cargo) => println!(
                "removal: {} uninstall --root {} {}",
                cargo.program.display(),
                cargo.root.display(),
                env!("CARGO_PKG_NAME")
            ),
            None => println!("removal: delete this standalone executable"),
        }
        println!("cache retained: {}", context.cache.root.display());
        println!("data retained: {}", data.display());
        return Ok(0);
    }

    if let Some(cargo) = cargo {
        uninstall_with_cargo(&cargo)?;
    } else {
        uninstall_standalone(&executable)?;
    }

    #[cfg(windows)]
    context.info(format!(
        "scheduled uninstall of {} after this process exits",
        executable.display()
    ));
    #[cfg(not(windows))]
    context.info(format!("uninstalled {}", executable.display()));
    context.info(format!("kept cache at {}", context.cache.root.display()));
    context.info(format!("kept data at {}", data.display()));
    Ok(0)
}

fn update(context: &AppContext, args: SelfUpdateArgs) -> Result<i32> {
    let repository = release_repository(args.repository.as_deref())?;
    let release = fetch_release(&repository, args.version.as_deref())?;
    let selected = release.tag_name.trim_start_matches('v');
    let current = env!("CARGO_PKG_VERSION");
    if selected.is_empty() || release.tag_name.contains(char::is_whitespace) {
        bail!("release channel returned an invalid version tag");
    }

    println!("current: {current}");
    println!("selected: {}", release.tag_name);
    if selected == current && !args.force {
        println!("lev is already current");
        return Ok(0);
    }
    if args.check {
        println!("update available");
        return Ok(0);
    }
    if cfg!(windows) {
        bail!(
            "in-place self-update is not supported on Windows; run the released install.ps1 instead"
        );
    }

    // Asset names are fixed by platform rather than accepted from release
    // metadata. This keeps a compromised or malformed response from choosing
    // an unrelated executable that happens to have a valid checksum.
    let asset_name = release_asset_name()?;
    let checksum_name = format!("{asset_name}.sha256");
    let asset = find_asset(&release, asset_name)?;
    let checksum = find_asset(&release, &checksum_name)?;
    if asset.size > MAX_BINARY_BYTES {
        bail!(
            "release binary {} exceeds the 256 MiB safety limit",
            asset.name
        );
    }
    let checksum_bytes = download(&checksum.browser_download_url, MAX_CHECKSUM_BYTES)?;
    let expected = parse_checksum(&checksum_bytes, asset_name)?;
    let binary = download(&asset.browser_download_url, MAX_BINARY_BYTES)?;
    if binary.len() as u64 != asset.size {
        bail!(
            "release binary {} has {} bytes, expected {}",
            asset.name,
            binary.len(),
            asset.size
        );
    }
    let observed = format!("{:x}", Sha256::digest(&binary));
    if observed != expected {
        bail!(
            "release binary {} failed SHA-256 verification: expected {expected}, found {observed}",
            asset.name
        );
    }

    let executable = std::env::current_exe().context("failed to locate the running lev binary")?;
    require_regular_executable(&executable)?;
    replace_executable(&executable, &binary)?;
    context.info(format!(
        "updated {} to {} (SHA-256 verified)",
        executable.display(),
        release.tag_name
    ));
    Ok(0)
}

fn release_repository(requested: Option<&str>) -> Result<String> {
    let value = requested
        .map(str::to_owned)
        .or_else(|| std::env::var("LEV_UPDATE_REPOSITORY").ok())
        .or_else(compiled_repository)
        .with_context(
            || "this development build has no release repository; pass --repository owner/name",
        )?;
    validate_repository(&value)?;
    Ok(value)
}

fn compiled_repository() -> Option<String> {
    // Release builds may override the repository for a fork. Development
    // builds fall back to Cargo metadata so self-update remains testable.
    let configured = option_env!("LEV_RELEASE_REPOSITORY")
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    configured.or_else(|| {
        let repository = env!("CARGO_PKG_REPOSITORY");
        repository
            .strip_prefix("https://github.com/")
            .map(|value| value.trim_end_matches('/'))
            .map(|value| value.strip_suffix(".git").unwrap_or(value).to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn validate_repository(value: &str) -> Result<()> {
    let Some((owner, name)) = value.split_once('/') else {
        bail!("release repository must use owner/name form");
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || ![owner, name].into_iter().all(|part| {
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        bail!("invalid release repository: {value:?}");
    }
    Ok(())
}

fn fetch_release(repository: &str, version: Option<&str>) -> Result<GithubRelease> {
    let api = std::env::var("LEV_UPDATE_API_URL").unwrap_or_else(|_| GITHUB_API.to_owned());
    validate_url(&api)?;
    let endpoint = if let Some(version) = version {
        if version.trim().is_empty() || version.contains(char::is_whitespace) {
            bail!("update version must be non-empty and contain no whitespace");
        }
        let tag = if version.starts_with('v') {
            version.to_owned()
        } else {
            format!("v{version}")
        };
        format!(
            "{}/repos/{repository}/releases/tags/{tag}",
            api.trim_end_matches('/')
        )
    } else {
        format!(
            "{}/repos/{repository}/releases/latest",
            api.trim_end_matches('/')
        )
    };
    let bytes = github_get(&endpoint, MAX_RELEASE_METADATA_BYTES)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse release metadata from {endpoint}"))
}

fn github_get(url: &str, limit: u64) -> Result<Vec<u8>> {
    let policy = validate_url(url)?;
    let mut request = ureq::get(url)
        .config()
        .https_only(policy.is_https())
        .build()
        .header("Accept", "application/vnd.github+json")
        .header(
            "User-Agent",
            concat!("lev/", env!("CARGO_PKG_VERSION"), " (self-update)"),
        );
    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
    {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request
        .call()
        .with_context(|| format!("failed to fetch {url}"))?;
    bounded_io::read_to_end(
        response.body_mut().as_reader(),
        limit,
        format!("response from {url}"),
    )
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    let policy = validate_url(url)?;
    let mut response = ureq::get(url)
        .config()
        .https_only(policy.is_https())
        .build()
        .header(
            "User-Agent",
            concat!("lev/", env!("CARGO_PKG_VERSION"), " (self-update)"),
        )
        .call()
        .with_context(|| format!("failed to download release asset {url}"))?;
    bounded_io::read_to_end(
        response.body_mut().as_reader(),
        limit,
        format!("response from {url}"),
    )
}

fn find_asset<'a>(release: &'a GithubRelease, name: &str) -> Result<&'a GithubAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .with_context(|| {
            format!(
                "release {} does not contain required asset {name}",
                release.tag_name
            )
        })
}

fn parse_checksum(bytes: &[u8], expected_name: &str) -> Result<String> {
    // Accept the two common sha256sum filename forms, but require exactly one
    // record naming the asset selected above.
    let text = std::str::from_utf8(bytes).context("release checksum is not UTF-8")?;
    let mut fields = text.split_whitespace();
    let digest = fields.next().context("release checksum file is empty")?;
    let name = fields
        .next()
        .context("release checksum file has no asset name")?
        .trim_start_matches('*');
    if fields.next().is_some() || !crate::core::hex::is_sha256(digest) || name != expected_name {
        bail!("release checksum file is malformed or names another asset");
    }
    Ok(digest.to_owned())
}

fn validate_url(value: &str) -> Result<HttpUrl> {
    let url = HttpUrl::parse(value, "update URL")?;
    url.require_secure("update URL", false)?;
    Ok(url)
}

fn require_regular_executable(path: &Path) -> Result<()> {
    // Replacing or removing a symlink could modify an unexpected target
    // outside lev's installation directory, so self-management refuses it.
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!(
            "refusing to modify a non-regular executable: {}",
            path.display()
        );
    }
    Ok(())
}

struct CargoUninstaller {
    program: PathBuf,
    root: PathBuf,
}

/// Use Cargo when its install metadata owns the executable's binary root.
///
/// Besides deleting the binary, `cargo uninstall` updates Cargo's own install
/// records. The metadata check distinguishes Cargo roots, including custom
/// `--root` installs, from a standalone binary copied into an arbitrary `bin`.
fn cargo_uninstaller(executable: &Path) -> Option<CargoUninstaller> {
    let expected_name = format!("lev{}", std::env::consts::EXE_SUFFIX);
    if executable.file_name()? != std::ffi::OsStr::new(&expected_name) {
        return None;
    }
    let bin = executable.parent()?;
    let root = bin.parent()?.to_owned();
    if !root.join(".crates2.json").is_file() && !root.join(".crates.toml").is_file() {
        return None;
    }
    let sibling = bin.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    let program = if sibling.is_file() {
        sibling
    } else {
        PathBuf::from(format!("cargo{}", std::env::consts::EXE_SUFFIX))
    };
    Some(CargoUninstaller { program, root })
}

#[cfg(not(windows))]
fn uninstall_with_cargo(cargo: &CargoUninstaller) -> Result<()> {
    let status = Command::new(&cargo.program)
        .arg("uninstall")
        .arg("--root")
        .arg(&cargo.root)
        .arg(env!("CARGO_PKG_NAME"))
        .status()
        .with_context(|| format!("failed to start {}", cargo.program.display()))?;
    if !status.success() {
        bail!(
            "{} uninstall {} failed with {status}",
            cargo.program.display(),
            env!("CARGO_PKG_NAME")
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn uninstall_standalone(executable: &Path) -> Result<()> {
    fs::remove_file(executable)
        .with_context(|| format!("failed to remove {}", executable.display()))
}

#[cfg(windows)]
fn uninstall_with_cargo(cargo: &CargoUninstaller) -> Result<()> {
    let script = concat!(
        "$ErrorActionPreference = 'Stop'; ",
        "$parentProcessId = [int]$env:LEV_UNINSTALL_PARENT_PID; ",
        "Wait-Process -Id $parentProcessId -ErrorAction SilentlyContinue; ",
        "& $env:LEV_UNINSTALL_CARGO uninstall ",
        "--root $env:LEV_UNINSTALL_ROOT $env:LEV_UNINSTALL_PACKAGE; ",
        "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }"
    );
    powershell(script)
        .env("LEV_UNINSTALL_CARGO", &cargo.program)
        .env("LEV_UNINSTALL_ROOT", &cargo.root)
        .env("LEV_UNINSTALL_PACKAGE", env!("CARGO_PKG_NAME"))
        .env("LEV_UNINSTALL_PARENT_PID", std::process::id().to_string())
        .spawn()
        .with_context(|| format!("failed to schedule {}", cargo.program.display()))?;
    Ok(())
}

#[cfg(windows)]
fn uninstall_standalone(executable: &Path) -> Result<()> {
    let script = concat!(
        "$ErrorActionPreference = 'Stop'; ",
        "$parentProcessId = [int]$env:LEV_UNINSTALL_PARENT_PID; ",
        "$target = $env:LEV_UNINSTALL_EXECUTABLE; ",
        "Wait-Process -Id $parentProcessId -ErrorAction SilentlyContinue; ",
        "for ($attempt = 0; $attempt -lt 100; $attempt++) { ",
        "if (-not (Test-Path -LiteralPath $target -PathType Leaf)) { exit 0 }; ",
        "try { Remove-Item -LiteralPath $target -Force -ErrorAction Stop; exit 0 } ",
        "catch { Start-Sleep -Milliseconds 50 } ",
        "}; ",
        "exit 1"
    );
    powershell(script)
        .env("LEV_UNINSTALL_EXECUTABLE", executable)
        .env("LEV_UNINSTALL_PARENT_PID", std::process::id().to_string())
        .spawn()
        .with_context(|| format!("failed to schedule removal of {}", executable.display()))?;
    Ok(())
}

#[cfg(windows)]
fn powershell(script: &str) -> Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]);
    // Environment values preserve arbitrary Windows paths without asking
    // PowerShell's native argument parser to quote them a second time.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    command
}

fn release_asset_name() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("lev-linux-x86_64"),
        ("macos", "aarch64") => Ok("lev-macos-arm64"),
        ("windows", "x86_64") => Ok("lev-windows-x86_64.exe"),
        (os, arch) => bail!("lev does not publish a self-update binary for {os}/{arch}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_checksum, validate_repository};

    #[test]
    fn validates_release_identity_and_checksum_file() {
        validate_repository("owner/lev").unwrap();
        assert!(validate_repository("owner").is_err());
        assert!(validate_repository("owner/repo/extra").is_err());

        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum(
                format!("{digest}  lev-linux-x86_64\n").as_bytes(),
                "lev-linux-x86_64"
            )
            .unwrap(),
            digest
        );
        assert!(
            parse_checksum(
                format!("{}  another-file\n", "a".repeat(64)).as_bytes(),
                "lev-linux-x86_64"
            )
            .is_err()
        );
    }
}
