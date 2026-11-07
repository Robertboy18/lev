//! Verdict cache: one small JSON file per (kernel identity, slice key).
//!
//! Files live at `DIR/<identity>/<key[..2]>/<key>.json` and are published
//! atomically. Accept and reject verdicts are cached; declines and errors
//! (timeouts, signals) are not, so they are retried on the next run. The
//! reference's pins (the full hashes of the pinned constants and of
//! everything they depend on) are kept at `DIR/reference/<digest>.json`,
//! keyed by the reference's bytes, so warm runs do not parse the reference
//! again.
//! A hit sets the file's modification time, so age-based collection removes
//! only what no recent run used.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::hash::Hash;
use super::kernel::{Kernel, Run, Verdict};
use super::link::Pins;
use super::slice::View;
use crate::core::atomic_file;
use crate::core::hex;

const RECORD_FORMAT: &str = "lev-verdict/1";
const PINS_FORMAT: &str = "lev-pins/2";

/// Reference hashes, cached under the reference's content digest.
#[derive(Debug, Serialize, Deserialize)]
struct PinsRecord {
    format: String,
    digest: String,
    pins: Pins,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    format: String,
    kernel: String,
    identity: String,
    key: String,
    module: String,
    view: View,
    #[serde(flatten)]
    run: Run,
}

pub(crate) struct Cache {
    dir: Option<PathBuf>,
}

impl Cache {
    /// A cache in `dir`, or no cache at all.
    pub(crate) fn new(dir: Option<&Path>) -> Result<Self> {
        if let Some(dir) = dir {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        Ok(Self {
            dir: dir.map(Path::to_path_buf),
        })
    }

    fn path(&self, kernel: &Kernel, key: &Hash) -> Option<PathBuf> {
        let key = hex::lowercase(key);
        Some(
            self.dir
                .as_ref()?
                .join(&kernel.identity)
                .join(&key[..2])
                .join(format!("{key}.json")),
        )
    }

    /// A cached accept or reject verdict; anything unreadable is a miss.
    pub(crate) fn get(&self, kernel: &Kernel, key: &Hash) -> Option<Run> {
        let path = self.path(kernel, key)?;
        let bytes = fs::read(&path).ok()?;
        let record: Record = serde_json::from_slice(&bytes).ok()?;
        let matches = record.format == RECORD_FORMAT
            && record.identity == kernel.identity
            && record.key == hex::lowercase(key)
            && matches!(record.run.verdict, Verdict::Accept | Verdict::Reject);
        if matches {
            mark_used(&path);
        }
        matches.then_some(record.run)
    }

    pub(crate) fn put(
        &self,
        kernel: &Kernel,
        key: &Hash,
        module: &str,
        view: View,
        run: &Run,
    ) -> Result<()> {
        let Some(path) = self.path(kernel, key) else {
            return Ok(());
        };
        if !matches!(run.verdict, Verdict::Accept | Verdict::Reject) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let record = Record {
            format: RECORD_FORMAT.to_owned(),
            kernel: kernel.kind.as_str().to_owned(),
            identity: kernel.identity.clone(),
            key: hex::lowercase(key),
            module: module.to_owned(),
            view,
            run: run.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&record)?;
        bytes.push(b'\n');
        atomic_file::replace(&path, &bytes)
    }

    fn pins_path(&self, digest: &str) -> Option<PathBuf> {
        Some(
            self.dir
                .as_ref()?
                .join("reference")
                .join(format!("{digest}.json")),
        )
    }

    /// Pins computed earlier from a reference with this content digest.
    pub(crate) fn pins(&self, digest: &str) -> Option<Pins> {
        let path = self.pins_path(digest)?;
        let bytes = fs::read(&path).ok()?;
        let record: PinsRecord = serde_json::from_slice(&bytes).ok()?;
        let matches = record.format == PINS_FORMAT && record.digest == digest;
        if matches {
            mark_used(&path);
        }
        matches.then_some(record.pins)
    }

    pub(crate) fn put_pins(&self, digest: &str, pins: &Pins) -> Result<()> {
        let Some(path) = self.pins_path(digest) else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let record = PinsRecord {
            format: PINS_FORMAT.to_owned(),
            digest: digest.to_owned(),
            pins: pins.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&record)?;
        bytes.push(b'\n');
        atomic_file::replace(&path, &bytes)
    }
}

/// Set a cache file's modification time to now. Best effort: a read-only
/// cache still serves hits.
fn mark_used(path: &Path) {
    let _ = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()));
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::super::kernel::{Kernel, KernelKind, Run, Verdict};
    use super::super::slice::View;
    use super::Cache;

    fn kernel(identity: &str) -> Kernel {
        Kernel {
            kind: KernelKind::Official,
            exe: PathBuf::from("/bin/true"),
            prefix: PathBuf::from("/"),
            identity: identity.repeat(32),
        }
    }

    fn run(verdict: Verdict) -> Run {
        Run {
            verdict,
            exit_code: Some(1),
            seconds: 1.5,
            message: "kernel output".to_owned(),
        }
    }

    #[test]
    fn caches_accept_and_reject_but_not_declines_or_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(Some(dir.path())).expect("cache");
        let official = kernel("ab");
        for (byte, verdict, cached) in [
            (1, Verdict::Accept, true),
            (2, Verdict::Reject, true),
            (3, Verdict::Decline, false),
            (4, Verdict::Error, false),
        ] {
            let key = [byte; 32];
            assert!(cache.get(&official, &key).is_none());
            cache
                .put(&official, &key, "M", View::Exported, &run(verdict))
                .expect("put");
            assert_eq!(
                cache.get(&official, &key),
                cached.then(|| run(verdict)),
                "{verdict:?}"
            );
        }
        // Another kernel identity never sees these verdicts.
        assert!(cache.get(&kernel("cd"), &[1; 32]).is_none());
    }

    #[test]
    fn unreadable_records_are_misses_and_no_directory_means_no_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(Some(dir.path())).expect("cache");
        let official = kernel("ab");
        let key = [9; 32];
        cache
            .put(&official, &key, "M", View::Closure, &run(Verdict::Accept))
            .expect("put");
        let path = cache.path(&official, &key).expect("path");
        fs::write(&path, b"{ not json").expect("corrupt");
        assert!(cache.get(&official, &key).is_none());

        let none = Cache::new(None).expect("cache");
        none.put(&official, &key, "M", View::Closure, &run(Verdict::Accept))
            .expect("put");
        assert!(none.get(&official, &key).is_none());
    }

    #[test]
    fn reference_pins_round_trip_by_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pins = super::super::fixture::pins();
        let cache = Cache::new(Some(&dir.path().join("cache"))).expect("cache");
        assert!(cache.pins("digest").is_none());
        cache.put_pins("digest", &pins).expect("put");
        assert_eq!(cache.pins("digest"), Some(pins));
        assert!(cache.pins("other").is_none());
    }
}
