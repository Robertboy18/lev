//! Whole-file replacement through a sibling staging file.
//!
//! Readers see either the old contents or the new ones, never a partial write.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand_core::{OsRng, RngCore};

use crate::core::hex;

const MAX_STAGE_ATTEMPTS: usize = 32;

/// Replace `path` with one complete byte sequence.
pub(crate) fn replace(path: &Path, contents: &[u8]) -> Result<()> {
    let staged = stage(path, contents)?;
    let result = rename_replace(&staged, path)
        .with_context(|| format!("failed to replace {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

#[cfg(not(windows))]
fn rename_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

/// Replace a Windows destination with its prepared sibling.
#[cfg(windows)]
fn rename_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    const MAX_REPLACE_ATTEMPTS: u32 = 8;
    static REPLACE_LOCK: Mutex<()> = Mutex::new(());

    // Removing the destination first would let another writer publish between
    // the removal and the rename. atomicwrites wraps Windows' replace-existing
    // move instead. Overlapping replacements can still report a sharing
    // violation, so serialize writers in this process and retry briefly.
    let _guard = REPLACE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for attempt in 0..MAX_REPLACE_ATTEMPTS {
        match atomicwrites::replace_atomic(source, destination) {
            Ok(()) => return Ok(()),
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && attempt + 1 < MAX_REPLACE_ATTEMPTS =>
            {
                thread::sleep(Duration::from_millis(1_u64 << attempt.min(5)));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the bounded replacement loop always returns")
}

/// Write `contents` to a fresh `.NAME.lev-tmp-<16 hex>` file beside `destination`.
fn stage(destination: &Path, contents: &[u8]) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("unnamed"));

    for _ in 0..MAX_STAGE_ATTEMPTS {
        let mut random = [0_u8; 8];
        OsRng.fill_bytes(&mut random);
        let mut staged_name = OsString::from(".");
        staged_name.push(name);
        staged_name.push(".lev-tmp-");
        staged_name.push(hex::lowercase(&random));
        let staged = parent.join(staged_name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", staged.display()));
            }
        };
        let written = file
            .write_all(contents)
            .with_context(|| format!("failed to write {}", staged.display()));
        drop(file);
        if let Err(error) = written {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
        return Ok(staged);
    }
    bail!(
        "failed to allocate a staging file beside {}",
        destination.display()
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use tempfile::tempdir;

    use super::replace;

    #[test]
    fn replacement_publishes_complete_contents_and_cleans_staging_files() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        replace(&path, b"new contents").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new contents");
        assert_no_staging_files(temp.path());
    }

    #[test]
    fn concurrent_replacements_use_independent_staging_files() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("record.json");
        let barrier = Arc::new(Barrier::new(8));
        let workers = (0..8)
            .map(|index| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let contents = format!("writer-{index}");
                    barrier.wait();
                    replace(&path, contents.as_bytes()).unwrap();
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().unwrap();
        }

        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.starts_with("writer-"), "{contents}");
        assert_no_staging_files(temp.path());
    }

    #[test]
    fn failed_replacement_removes_its_staging_file() {
        let temp = tempdir().unwrap();
        let destination = temp.path().join("directory");
        fs::create_dir(&destination).unwrap();

        replace(&destination, b"cannot replace a directory").unwrap_err();

        assert!(destination.is_dir());
        assert_no_staging_files(temp.path());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_non_utf8_destination_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = tempdir().unwrap();
        let name = OsString::from_vec(b"state-\xff.json".to_vec());
        let path = temp.path().join(name);

        // Some Unix filesystems, including the default macOS temporary
        // filesystem, reject non-UTF-8 names before lev can exercise them.
        if fs::write(&path, b"old").is_err() {
            return;
        }
        replace(&path, b"contents").unwrap();

        assert_eq!(fs::read(path).unwrap(), b"contents");
        assert_no_staging_files(temp.path());
    }

    fn assert_no_staging_files(root: &Path) {
        let names = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(
            names
                .iter()
                .all(|name| !name.to_string_lossy().contains(".lev-tmp-")),
            "staging files remain: {names:?}"
        );
    }
}
