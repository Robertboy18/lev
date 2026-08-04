//! Deterministic, source-only project bundles.
//!
//! Generated build trees and dependency checkouts are excluded. Every archive
//! carries a sorted manifest of the included paths and their content digests.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::core::atomic_file::rename_replace;
use crate::project::Project;
use crate::project::lockfile;

const BUNDLE_SCHEMA: &str = "lev.project-bundle/v1";
const BUNDLE_MANIFEST: &str = "lev-bundle.json";
const MAX_FILES: usize = 250_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const ZSTD_LEVEL: i32 = 9;

#[derive(Debug)]
pub struct BundleStats {
    pub files: usize,
    pub source_bytes: u64,
}

#[derive(Debug, Serialize)]
struct BundleManifest {
    schema: &'static str,
    project: String,
    toolchain: String,
    files: Vec<BundleFile>,
}

#[derive(Debug, Serialize)]
struct BundleFile {
    path: String,
    kind: BundleFileKind,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum BundleFileKind {
    File,
    Symlink,
}

struct SourceEntry {
    path: PathBuf,
    relative: PathBuf,
    portable: String,
    kind: BundleFileKind,
    size: u64,
    sha256: String,
    executable: bool,
    link_target: Option<PathBuf>,
}

struct DigestReader<R> {
    inner: R,
    digest: Sha256,
    bytes_read: u64,
}

impl<R> DigestReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            bytes_read: 0,
        }
    }

    fn finish(self) -> (u64, String) {
        (self.bytes_read, format!("{:x}", self.digest.finalize()))
    }
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.digest.update(&buffer[..count]);
        self.bytes_read = self
            .bytes_read
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("bundle byte count overflow"))?;
        Ok(count)
    }
}

pub fn create(project: &Project, destination: &Path, force: bool) -> Result<BundleStats> {
    lockfile::verify(project)?;
    let mut entries = Vec::new();
    collect_entries(&project.root, &project.root, destination, &mut entries)?;
    entries.sort_by(|left, right| left.portable.cmp(&right.portable));
    let source_bytes = entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size)
            .context("bundle source size overflow")
    })?;
    if source_bytes > MAX_TOTAL_BYTES {
        bail!("bundle sources exceed the 16 GiB safety limit");
    }

    let project_name = archive_root_name(&project.root)?;
    let manifest = BundleManifest {
        schema: BUNDLE_SCHEMA,
        project: project_name.clone(),
        toolchain: project.toolchain.clone(),
        files: entries
            .iter()
            .map(|entry| BundleFile {
                path: entry.portable.clone(),
                kind: entry.kind,
                size: entry.size,
                sha256: entry.sha256.clone(),
            })
            .collect(),
    };
    let mut manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("failed to serialize bundle manifest")?;
    manifest_bytes.push(b'\n');

    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    if destination.exists() && !force {
        bail!(
            "{} already exists; pass --force to replace it",
            destination.display()
        );
    }

    let (staged_path, staged_file) = create_staging_file(destination)?;
    let result = write_archive(staged_file, &project_name, &manifest_bytes, &entries)
        .and_then(|file| {
            file.sync_all()
                .with_context(|| format!("failed to sync {}", staged_path.display()))
        })
        .and_then(|()| publish(&staged_path, destination, force));
    if result.is_err() {
        let _ = fs::remove_file(&staged_path);
    }
    result?;

    Ok(BundleStats {
        files: entries.len(),
        source_bytes,
    })
}

fn collect_entries(
    root: &Path,
    directory: &Path,
    destination: &Path,
    entries: &mut Vec<SourceEntry>,
) -> Result<()> {
    let mut children = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", directory.display()))?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let path = child.path();
        let relative = path
            .strip_prefix(root)
            .context("bundle traversal escaped the project root")?;
        let relative = relative.to_owned();
        let file_type = child
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            if !excluded(&relative) {
                collect_entries(root, &path, destination, entries)?;
            }
            continue;
        }
        if path == destination
            || relative
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".lev.tar.zst"))
        {
            continue;
        }
        if entries.len() >= MAX_FILES {
            bail!("project contains more than {MAX_FILES} bundle entries");
        }
        if relative == Path::new(BUNDLE_MANIFEST) {
            bail!("{BUNDLE_MANIFEST} is reserved for lev bundle metadata");
        }
        validate_relative(&relative)?;
        let portable = relative
            .to_str()
            .map(|value| value.replace('\\', "/"))
            .with_context(|| format!("bundle path is not UTF-8: {}", relative.display()))?;

        if file_type.is_file() {
            let metadata = child
                .metadata()
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if metadata.len() > MAX_FILE_BYTES {
                bail!("bundle file exceeds 2 GiB: {}", relative.display());
            }
            entries.push(SourceEntry {
                sha256: hash_file(&path)?,
                path,
                relative,
                portable,
                kind: BundleFileKind::File,
                size: metadata.len(),
                executable: is_executable(&metadata),
                link_target: None,
            });
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            validate_link(&relative, &target)?;
            let target_bytes = target
                .to_str()
                .with_context(|| {
                    format!(
                        "bundle symlink target is not UTF-8: {} -> {}",
                        relative.display(),
                        target.display()
                    )
                })?
                .as_bytes()
                .to_vec();
            entries.push(SourceEntry {
                path,
                relative,
                portable,
                kind: BundleFileKind::Symlink,
                size: target_bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&target_bytes)),
                executable: false,
                link_target: Some(target),
            });
        } else {
            bail!("unsupported special file in bundle: {}", relative.display());
        }
    }
    Ok(())
}

fn excluded(relative: &Path) -> bool {
    relative.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(
                    name.to_str(),
                    Some(
                        ".git"
                            | ".lake"
                            | "_build"
                            | "_target"
                            | "build"
                            | "lake-packages"
                            | "lean_packages"
                            | "target"
                    )
                )
        )
    })
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.components().count() > 64 {
        bail!("bundle path is too deep: {}", path.display());
    }
    if path.as_os_str().len() > 4096 {
        bail!("bundle path is too long: {}", path.display());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsafe bundle path: {}", path.display());
    }
    Ok(())
}

fn validate_link(relative: &Path, target: &Path) -> Result<()> {
    if target.is_absolute() {
        bail!(
            "bundle symlink has an absolute target: {} -> {}",
            relative.display(),
            target.display()
        );
    }
    let mut depth = relative.parent().map_or(0_i64, |path| {
        path.components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count() as i64
    });
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "bundle symlink escapes the project: {} -> {}",
                    relative.display(),
                    target.display()
                )
            }
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn archive_root_name(root: &Path) -> Result<String> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("project root has no portable directory name")?;
    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.is_empty() || matches!(normalized.as_str(), "." | "..") {
        bail!("project root has no safe archive name");
    }
    Ok(normalized)
}

fn write_archive(
    output: File,
    root_name: &str,
    manifest: &[u8],
    entries: &[SourceEntry],
) -> Result<File> {
    let encoder = zstd::stream::write::Encoder::new(output, ZSTD_LEVEL)
        .context("failed to start bundle compression")?;
    let mut archive = tar::Builder::new(encoder);

    let manifest_path = Path::new(root_name).join(BUNDLE_MANIFEST);
    append_bytes(&mut archive, &manifest_path, manifest, false)?;
    for entry in entries {
        let archive_path = Path::new(root_name).join(&entry.relative);
        match entry.kind {
            BundleFileKind::File => {
                let file = File::open(&entry.path)
                    .with_context(|| format!("failed to read {}", entry.path.display()))?;
                let mut reader = DigestReader::new(file);
                let mut header = deterministic_header(entry.size, entry.executable);
                archive
                    .append_data(&mut header, &archive_path, &mut reader)
                    .with_context(|| format!("failed to archive {}", entry.relative.display()))?;
                let (bytes_read, digest) = reader.finish();
                let current_size = fs::metadata(&entry.path)
                    .with_context(|| format!("failed to inspect {}", entry.path.display()))?
                    .len();
                if bytes_read != entry.size || current_size != entry.size || digest != entry.sha256
                {
                    bail!(
                        "{} changed while the bundle was being created; retry",
                        entry.relative.display()
                    );
                }
            }
            BundleFileKind::Symlink => {
                let mut header = deterministic_header(0, false);
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name(
                    entry
                        .link_target
                        .as_deref()
                        .context("bundle symlink has no target")?,
                )?;
                header.set_cksum();
                archive
                    .append_data(&mut header, &archive_path, io::empty())
                    .with_context(|| format!("failed to archive {}", entry.relative.display()))?;
            }
        }
    }
    let encoder = archive
        .into_inner()
        .context("failed to finish bundle archive")?;
    encoder
        .finish()
        .context("failed to finish bundle compression")
}

fn append_bytes(
    archive: &mut tar::Builder<zstd::Encoder<'static, File>>,
    path: &Path,
    contents: &[u8],
    executable: bool,
) -> Result<()> {
    let mut header = deterministic_header(contents.len() as u64, executable);
    archive
        .append_data(&mut header, path, contents)
        .with_context(|| format!("failed to archive {}", path.display()))
}

fn deterministic_header(size: u64, executable: bool) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(if executable { 0o755 } else { 0o644 });
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    header
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn create_staging_file(destination: &Path) -> Result<(PathBuf, File)> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .context("bundle destination has no file name")?;
    for attempt in 0..32_u32 {
        let mut staged_name = OsString::from(".");
        staged_name.push(name);
        staged_name.push(format!(".lev-tmp-{}-{attempt}", std::process::id()));
        let staged = parent.join(staged_name);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)
        {
            Ok(file) => return Ok((staged, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", staged.display()));
            }
        }
    }
    bail!(
        "failed to reserve a staging file beside {}",
        destination.display()
    )
}

fn publish(staged: &Path, destination: &Path, force: bool) -> Result<()> {
    if force {
        rename_replace(staged, destination)
            .with_context(|| format!("failed to replace {}", destination.display()))
    } else {
        let result = fs::hard_link(staged, destination).with_context(|| {
            format!(
                "failed to create {}; it may already exist (pass --force to replace it)",
                destination.display()
            )
        });
        if result.is_ok() {
            fs::remove_file(staged)
                .with_context(|| format!("failed to remove {}", staged.display()))?;
        }
        result
    }
}
