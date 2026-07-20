//! Atomic file and directory publication helpers.
//!
//! Replacements use sibling staging files. Create-only writes use hard links,
//! so readers never see partial contents.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand_core::{OsRng, RngCore};

const MAX_STAGE_ATTEMPTS: usize = 32;

/// Replace `path` with one complete byte sequence.
pub(crate) fn replace(path: &Path, contents: &[u8]) -> Result<()> {
    let staged = stage(path, contents, false)?;
    let result = rename_replace(&staged, path)
        .with_context(|| format!("failed to replace {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// Replace a program image with durable bytes and executable Unix mode.
pub(crate) fn replace_executable(path: &Path, contents: &[u8]) -> Result<()> {
    let staged = stage(path, contents, true)?;
    let prepared = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
                .with_context(|| format!("failed to mark {} executable", staged.display()))?;
            fs::File::open(&staged)
                .and_then(|file| file.sync_all())
                .with_context(|| {
                    format!("failed to sync executable mode for {}", staged.display())
                })?;
        }
        Ok(())
    })();
    if let Err(error) = prepared {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    let result = rename_replace(&staged, path)
        .with_context(|| format!("failed to replace executable {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// Publish an already prepared sibling path as `destination`.
#[cfg(not(windows))]
pub(crate) fn rename_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

/// Atomically replace a Windows destination with its prepared sibling.
#[cfg(windows)]
pub(crate) fn rename_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    const MAX_REPLACE_ATTEMPTS: u32 = 8;
    static REPLACE_LOCK: Mutex<()> = Mutex::new(());

    // Removing the destination first creates a race: another writer can
    // publish between remove_file and rename. atomicwrites wraps Windows'
    // replace-existing move without introducing unsafe code into lev.
    //
    // Windows can still report a transient sharing violation when two
    // replacements overlap. Serialize writers in this process and briefly
    // retry permission errors caused by another lev process.
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

/// Publish a durable file without replacing an existing destination.
pub(crate) fn create(path: &Path, contents: &[u8]) -> Result<()> {
    let staged = stage(path, contents, true)?;
    let result = fs::hard_link(&staged, path).with_context(|| {
        format!(
            "failed to create {}; it may already exist (pass --force to replace it)",
            path.display()
        )
    });
    let _ = fs::remove_file(&staged);
    result
}

/// Create a new file beneath trusted parent directories.
pub(crate) fn create_new_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))
}

/// Create one directory, accepting a racing directory but rejecting anything else.
pub(crate) fn create_real_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                bail!("{} appeared but is not a real directory", path.display())
            }
        }
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
}

/// Durably write a new file in a trusted staging tree.
pub(crate) fn write_new_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = create_new_file(path)?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

/// Copy `source` to `destination` only when their byte contents differ.
pub(crate) fn copy_if_changed(source: &Path, destination: &Path) -> Result<()> {
    let contents =
        fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    if fs::read(destination).ok().as_deref() == Some(contents.as_slice()) {
        return Ok(());
    }
    replace(destination, &contents)
}

fn stage(destination: &Path, contents: &[u8], sync: bool) -> Result<PathBuf> {
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
        let suffix = crate::cache::lowercase_hex(&random);
        let mut staged_name = OsString::from(".");
        staged_name.push(name);
        staged_name.push(".lev-tmp-");
        staged_name.push(suffix);
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
        let result = (|| {
            file.write_all(contents)
                .with_context(|| format!("failed to write {}", staged.display()))?;
            if sync {
                file.sync_all()
                    .with_context(|| format!("failed to sync {}", staged.display()))?;
            }
            Ok(())
        })();
        drop(file);
        if let Err(error) = result {
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
    use std::sync::{Arc, Barrier};
    use std::thread;

    use tempfile::tempdir;

    #[cfg(unix)]
    use super::replace_executable;
    use super::{create, create_real_directory, replace};

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
    fn create_is_race_free_and_never_overwrites_an_existing_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("export.json");

        create(&path, b"first").unwrap();
        let error = create(&path, b"second").unwrap_err().to_string();

        assert!(error.contains("already exist"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"first");
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
    fn real_directory_creation_accepts_a_racing_directory_but_not_a_file() {
        let temp = tempdir().unwrap();
        let directory = temp.path().join("objects");
        let barrier = Arc::new(Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let directory = directory.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    create_real_directory(&directory)
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert!(directory.is_dir());

        let file = temp.path().join("not-a-directory");
        fs::write(&file, b"occupied").unwrap();
        assert!(create_real_directory(&file).is_err());
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
    fn executable_replacement_publishes_synced_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        let path = temp.path().join("lev");
        fs::write(&path, b"old").unwrap();

        replace_executable(&path, b"new binary").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new binary");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
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

    use std::path::Path;
}
