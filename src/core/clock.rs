//! Wall-clock helpers for cache ages.
//!
//! Unknown or pre-epoch times map to zero, so an unreadable time makes an entry
//! look old rather than new; callers pair this with reachability checks.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Current Unix time in whole seconds, saturating at the epoch.
pub(crate) fn now_seconds() -> u64 {
    since_epoch(SystemTime::now()).as_secs()
}

/// A path's modification time in seconds, or zero when unavailable.
pub(crate) fn modified_seconds(path: &Path) -> u64 {
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(since_epoch)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// The Unix time `days` days before now.
pub(crate) fn cutoff(days: u64) -> u64 {
    now_seconds().saturating_sub(days.saturating_mul(86_400))
}

fn since_epoch(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO)
}
