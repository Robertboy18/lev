//! Shared filesystem, transport, platform, and output helpers.
//!
//! Core code has no project or package-manager policy.

pub(crate) mod atomic_file;
pub(crate) mod bounded_io;
pub(crate) mod clock;
pub(crate) mod file_hash;
pub(crate) mod hex;
pub(crate) mod http_url;
pub(crate) mod json_output;
pub(crate) mod object_transport;
pub(crate) mod platform;
pub(crate) mod platform_dirs;
pub(crate) mod process;
pub(crate) mod signing;
