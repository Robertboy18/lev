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

    fn restore(&self) {
        for snapshot in &self.snapshots {
            match &snapshot.contents {
                Some(contents) => {
                    let _ = replace(&snapshot.path, contents);
                }
                None => {
                    let _ = fs::remove_file(&snapshot.path);
                }
            }
        }
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.committed {
            self.restore();
        }
    }
}
