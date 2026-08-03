//! Rollback guards for multi-file project changes.
//!
//! Files are captured before an external command runs and restored on failure.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::core::atomic_file::replace;

/// Original contents of one file participating in a command transaction.
struct FileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

/// Rollback guard for a known set of project files.
pub(super) struct FileTransaction {
    snapshots: Vec<FileSnapshot>,
    committed: bool,
}

impl FileTransaction {
    /// Capture each path before mutation.
    pub(super) fn capture<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Result<Self> {
        let snapshots = paths
            .into_iter()
            .map(|path| {
                let contents = if path.exists() {
                    Some(
                        fs::read(path)
                            .with_context(|| format!("failed to snapshot {}", path.display()))?,
                    )
                } else {
                    None
                };
                Ok(FileSnapshot {
                    path: path.clone(),
                    contents,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            snapshots,
            committed: false,
        })
    }

    /// Keep the new files when the operation succeeds.
    pub(super) fn commit(&mut self) {
        self.committed = true;
    }

    /// Restore every snapshot while retaining rollback protection.
    ///
    /// A caller can continue a guarded operation after this method. Unless it
    /// later commits, dropping the transaction restores the snapshots again.
    pub(super) fn restore_now(&self) -> Result<()> {
        for snapshot in &self.snapshots {
            restore_snapshot(snapshot)?;
        }
        Ok(())
    }

    fn restore_best_effort(&self) {
        for snapshot in &self.snapshots {
            let _ = restore_snapshot(snapshot);
        }
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.committed {
            self.restore_best_effort();
        }
    }
}

fn restore_snapshot(snapshot: &FileSnapshot) -> Result<()> {
    match &snapshot.contents {
        Some(contents) => replace(&snapshot.path, contents),
        None => match fs::remove_file(&snapshot.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to restore {}", snapshot.path.display()))
            }
        },
    }
}
