//! Stable native platform identity shared by lock and cache formats.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Return the host identifier used for platform-sensitive Lean state.
pub fn host_id() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

pub fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid platform identifier: {value:?}");
    }
    Ok(())
}

/// Preserve readable/executable bits while removing write permissions.
#[cfg(unix)]
pub(crate) fn read_only_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o555
}

/// Non-Unix stores use the platform read-only flag rather than Unix modes.
#[cfg(not(unix))]
pub(crate) fn read_only_mode(_metadata: &fs::Metadata) -> u32 {
    0o444
}

/// Apply a read-only file or directory mode on the current platform.
#[cfg(unix)]
pub(crate) fn set_read_only_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn set_read_only_mode(path: &Path, _mode: u32) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

/// Recreate a symlink, using the source target type where Windows requires it.
#[cfg(unix)]
pub(crate) fn create_symlink_from_source(
    target: &Path,
    destination: &Path,
    _source: &Path,
) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)
        .with_context(|| format!("failed to create symlink {}", destination.display()))
}

#[cfg(windows)]
pub(crate) fn create_symlink_from_source(
    target: &Path,
    destination: &Path,
    source: &Path,
) -> Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
    .with_context(|| format!("failed to create symlink {}", destination.display()))
}
