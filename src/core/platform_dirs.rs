//! The platform's default cache directory for lev.
//!
//! `--cache-dir` and `LEV_CACHE_DIR` take precedence; see `cache::CacheLayout`.

use std::ffi::OsString;
use std::path::PathBuf;

/// Return the platform's default root for rebuildable lev cache data.
pub(crate) fn cache_root() -> Option<PathBuf> {
    nonempty_env("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .map(|root| root.join("lev"))
        .or_else(native_cache_root)
}

fn nonempty_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(windows)]
fn native_cache_root() -> Option<PathBuf> {
    nonempty_env("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("lev").join("cache"))
}

#[cfg(target_os = "macos")]
fn native_cache_root() -> Option<PathBuf> {
    nonempty_env("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Caches").join("lev"))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn native_cache_root() -> Option<PathBuf> {
    nonempty_env("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".cache").join("lev"))
}
