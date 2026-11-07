//! Small filesystem, process, and output helpers.
//!
//! Core code knows nothing about projects or caches.

pub(crate) mod atomic_file;
pub(crate) mod clock;
pub(crate) mod hex;
pub(crate) mod json_output;
pub(crate) mod lock;
pub(crate) mod platform_dirs;
pub(crate) mod process;
