//! Native per-user cache and data roots.
//!
//! These are platform defaults; explicit lev overrides are handled elsewhere.

use std::ffi::OsString;
use std::path::PathBuf;

/// Return the platform's default root for rebuildable lev cache data.
pub fn cache_root() -> Option<PathBuf> {
    nonempty_env("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .map(|root| root.join("lev"))
        .or_else(native_cache_root)
}

/// Return the platform's default root for persistent lev application data.
pub fn data_root() -> Option<PathBuf> {
    nonempty_env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .map(|root| root.join("lev"))
        .or_else(native_data_root)
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

#[cfg(windows)]
fn native_data_root() -> Option<PathBuf> {
    nonempty_env("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("lev").join("data"))
}

#[cfg(target_os = "macos")]
fn native_cache_root() -> Option<PathBuf> {
    nonempty_env("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Caches").join("lev"))
}

#[cfg(target_os = "macos")]
fn native_data_root() -> Option<PathBuf> {
    nonempty_env("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support").join("lev"))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn native_cache_root() -> Option<PathBuf> {
    nonempty_env("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".cache").join("lev"))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn native_data_root() -> Option<PathBuf> {
    nonempty_env("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local").join("share").join("lev"))
}
