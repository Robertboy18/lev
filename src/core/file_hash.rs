//! Streaming hashes for files used by content-addressed stores.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Compute a file's lowercase SHA-256 digest.
pub(crate) fn sha256(path: &Path) -> Result<String> {
    Ok(sha256_with_size(path)?.0)
}

/// Compute a file's lowercase SHA-256 digest and observed byte count.
pub(crate) fn sha256_with_size(path: &Path) -> Result<(String, u64)> {
    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .context("file size overflow while hashing")?;
    }
    Ok((format!("{:x}", digest.finalize()), bytes))
}
