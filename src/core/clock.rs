//! Wall-clock helpers for cache timestamps.
//!
//! Unknown or pre-epoch times map to zero, which keeps age-based cleanup
//! conservative.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::core::atomic_file;

/// Current Unix time in whole seconds, saturating at the epoch.
pub(crate) fn now_seconds() -> u64 {
    since_epoch(SystemTime::now()).as_secs()
}

/// Current Unix time in nanoseconds, saturating at the epoch.
pub(crate) fn now_nanos() -> u128 {
    since_epoch(SystemTime::now()).as_nanos()
}

/// A path's modification time in seconds, or epoch zero when unavailable.
pub(crate) fn modified_seconds(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(since_epoch)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Atomically record the current Unix timestamp in a cache marker.
pub(crate) fn write_timestamp(path: &Path) -> Result<()> {
    atomic_file::replace(path, format!("{}\n", now_seconds()).as_bytes())
}

fn since_epoch(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO)
}
