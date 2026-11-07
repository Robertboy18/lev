//! Advisory file locks.
//!
//! A lock is held for as long as the returned file stays open. Processes that
//! do not take the lock, such as a direct `lake build`, are not excluded.

use std::fs::{self, File, OpenOptions};
use std::path::Path;

use anyhow::{Context, Result};
use fs2::FileExt;

/// Take a shared lock on `path`, creating the lock file if needed.
pub(crate) fn shared(path: &Path) -> Result<File> {
    let file = open(path)?;
    FileExt::lock_shared(&file).with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(file)
}

/// Take an exclusive lock on `path`, creating the lock file if needed.
pub(crate) fn exclusive(path: &Path) -> Result<File> {
    let file = open(path)?;
    FileExt::lock_exclusive(&file).with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(file)
}

fn open(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))
}

#[cfg(test)]
mod tests {
    use fs2::FileExt;
    use tempfile::tempdir;

    use super::{exclusive, open, shared};

    #[test]
    fn a_shared_lock_excludes_an_exclusive_one() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("locks/cache.lock");
        let first = shared(&path).unwrap();
        let second = shared(&path).unwrap();
        let competing = open(&path).unwrap();
        assert!(FileExt::try_lock_exclusive(&competing).is_err());

        drop(first);
        drop(second);
        let _exclusive = exclusive(&path).unwrap();
        let competing = open(&path).unwrap();
        assert!(FileExt::try_lock_shared(&competing).is_err());
    }
}
