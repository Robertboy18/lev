//! Size-limited reads for streams and local metadata.
//!
//! Limits are checked while bytes are read, not from possibly stale metadata.

use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

/// Read at most `maximum` bytes and reject a source with additional data.
pub(crate) fn read_to_end(
    reader: impl Read,
    maximum: u64,
    source: impl Display,
) -> Result<Vec<u8>> {
    let source = source.to_string();
    let mut bytes = Vec::new();
    reader
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {source}"))?;
    if bytes.len() as u64 > maximum {
        bail!("{source} exceeds the {maximum}-byte safety limit");
    }
    Ok(bytes)
}

/// Read a local file while enforcing its limit against the consumed bytes.
pub(crate) fn read_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    read_to_end(file, maximum, path.display())
}

/// Read and decode one bounded JSON file with path-specific diagnostics.
pub(crate) fn read_json_file<T>(path: &Path, maximum: u64) -> Result<T>
where
    T: DeserializeOwned,
{
    let bytes = read_file(path, maximum)?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}
