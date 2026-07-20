//! Immutable-object transport for local directories and HTTP(S).
//!
//! Local writes are create-only; HTTP writes use `If-None-Match: *`. Higher
//! layers validate object signatures and digests. Plain HTTP is limited to
//! loopback unless the caller explicitly allows it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand_core::{OsRng, RngCore};

use crate::core::atomic_file::create_real_directory;
use crate::core::file_hash;
use crate::core::http_url::HttpUrl;

pub(crate) enum ObjectTransport {
    Directory {
        root: PathBuf,
    },
    Http {
        base: String,
        token: Option<String>,
        https_only: bool,
    },
}

impl ObjectTransport {
    /// Parse a directory, `file://` path, or HTTP(S) base URL.
    pub(crate) fn parse(value: &str, allow_insecure_http: bool) -> Result<Self> {
        if value.starts_with("https://") || value.starts_with("http://") {
            let url = HttpUrl::parse(value, "remote object URL")?;
            if url.has_query() {
                bail!("remote object base URL must not contain a query string");
            }
            url.require_secure("remote object URL", allow_insecure_http)?;
            return Ok(Self::Http {
                base: value.trim_end_matches('/').to_owned(),
                token: std::env::var("LEV_REMOTE_TOKEN")
                    .ok()
                    .filter(|token| !token.trim().is_empty()),
                https_only: url.is_https(),
            });
        }
        if let Some(path) = value.strip_prefix("file://") {
            if path.is_empty() {
                bail!("file remote has an empty path");
            }
            return Ok(Self::Directory {
                root: absolute_path(Path::new(path))?,
            });
        }
        if value.contains("://") {
            bail!("unsupported remote object URL scheme in {value:?}");
        }
        Ok(Self::Directory {
            root: absolute_path(Path::new(value))?,
        })
    }

    /// Fetch an object into a newly created destination.
    ///
    /// Returns `false` for a missing object. A response larger than `maximum`
    /// is rejected and the partial destination is removed.
    pub(crate) fn fetch(&self, object: &str, destination: &Path, maximum: u64) -> Result<bool> {
        validate_object_path(object)?;
        match self {
            Self::Directory { root } => {
                let source = secure_directory_object(root, object, false)?;
                match fs::symlink_metadata(&source) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        let input = File::open(&source)
                            .with_context(|| format!("failed to read {}", source.display()))?;
                        copy_bounded(input, destination, maximum)?;
                        Ok(true)
                    }
                    Ok(_) => bail!("remote object {} is not a regular file", source.display()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error)
                        .with_context(|| format!("failed to inspect {}", source.display())),
                }
            }
            Self::Http {
                base,
                token,
                https_only,
            } => {
                let url = format!("{base}/{object}");
                let mut request = ureq::get(&url)
                    .config()
                    .https_only(*https_only)
                    .build()
                    .header("Accept", "application/octet-stream")
                    .header("Accept-Encoding", "identity")
                    .header("User-Agent", user_agent());
                if let Some(token) = token {
                    request = request.header("Authorization", format!("Bearer {token}"));
                }
                let mut response = match request.call() {
                    Ok(response) => response,
                    Err(ureq::Error::StatusCode(404)) => return Ok(false),
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("failed to download remote object {url}"));
                    }
                };
                copy_bounded(response.body_mut().as_reader(), destination, maximum)?;
                Ok(true)
            }
        }
    }

    /// Publish only if absent. `false` means another writer won the name.
    pub(crate) fn publish_if_absent(&self, object: &str, source: &Path) -> Result<bool> {
        validate_object_path(object)?;
        match self {
            Self::Directory { root } => {
                let destination = secure_directory_object(root, object, true)?;
                let parent = destination
                    .parent()
                    .context("remote object has no parent")?;
                let temporary = unique_sibling(parent, ".lev-upload");
                copy_file_synced(source, &temporary)?;
                let result = match fs::hard_link(&temporary, &destination) {
                    Ok(()) => Ok(true),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                    Err(error) => Err(error).with_context(|| {
                        format!("failed to publish remote object {}", destination.display())
                    }),
                };
                let _ = fs::remove_file(&temporary);
                result
            }
            Self::Http {
                base,
                token,
                https_only,
            } => {
                let url = format!("{base}/{object}");
                let input = File::open(source)
                    .with_context(|| format!("failed to read {}", source.display()))?;
                let mut request = ureq::put(&url)
                    .config()
                    .https_only(*https_only)
                    .build()
                    .header("Content-Type", "application/octet-stream")
                    .header("If-None-Match", "*")
                    .header("User-Agent", user_agent());
                if let Some(token) = token {
                    request = request.header("Authorization", format!("Bearer {token}"));
                }
                match request.send(input) {
                    Ok(_) => Ok(true),
                    Err(ureq::Error::StatusCode(409 | 412)) => Ok(false),
                    Err(error) => {
                        Err(error).with_context(|| format!("failed to upload remote object {url}"))
                    }
                }
            }
        }
    }

    /// Publish a small immutable object, accepting an identical existing value.
    pub(crate) fn publish_immutable(
        &self,
        object: &str,
        source: &Path,
        maximum: u64,
    ) -> Result<bool> {
        let temporary = unique_sibling(
            source.parent().unwrap_or_else(|| Path::new(".")),
            ".lev-existing",
        );
        if self.fetch(object, &temporary, maximum)? {
            let matches = files_equal(source, &temporary)?;
            let _ = fs::remove_file(&temporary);
            if matches {
                return Ok(false);
            }
            bail!("remote object {object} already exists with different content");
        }
        if self.publish_if_absent(object, source)? {
            return Ok(true);
        }
        if !self.fetch(object, &temporary, maximum)? {
            bail!("remote object {object} disappeared during concurrent publication");
        }
        let matches = files_equal(source, &temporary)?;
        let _ = fs::remove_file(&temporary);
        if !matches {
            bail!("remote object {object} was concurrently published with different content");
        }
        Ok(false)
    }
}

fn validate_object_path(path: &str) -> Result<()> {
    let segments = split_relative_path(path, "remote object path")?;
    if segments
        .iter()
        .any(|segment| segment.chars().any(char::is_control))
    {
        bail!("remote object path contains a control character");
    }
    Ok(())
}

pub(crate) fn split_relative_path<'a>(path: &'a str, label: &str) -> Result<Vec<&'a str>> {
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains('\\') {
        bail!("unsafe {label} {path:?}");
    }
    let segments = path.split('/').collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
    {
        bail!("unsafe {label} {path:?}");
    }
    Ok(segments)
}

fn secure_directory_object(root: &Path, object: &str, create_parent: bool) -> Result<PathBuf> {
    let segments = split_relative_path(object, "remote object path")?;
    if create_parent {
        fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
    }
    let mut current = root.to_owned();
    for segment in &segments[..segments.len() - 1] {
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => bail!(
                "remote object ancestor {} is not a real directory",
                current.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound && create_parent => {
                create_real_directory(&current)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut missing = root.to_owned();
                for remaining in &segments {
                    missing.push(remaining);
                }
                return Ok(missing);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    current.push(segments.last().expect("nonempty validated path"));
    Ok(current)
}

fn copy_bounded(input: impl Read, destination: &Path, maximum: u64) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let result = (|| -> Result<()> {
        let mut output = create_new_file(destination)?;
        let copied = io::copy(&mut input.take(maximum.saturating_add(1)), &mut output)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        if copied > maximum {
            bail!("remote object exceeds the {maximum}-byte download limit");
        }
        output
            .sync_all()
            .with_context(|| format!("failed to sync {}", destination.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn copy_file_synced(source: &Path, destination: &Path) -> Result<()> {
    let mut input =
        File::open(source).with_context(|| format!("failed to read {}", source.display()))?;
    let mut output = create_new_file(destination)?;
    io::copy(&mut input, &mut output)
        .with_context(|| format!("failed to write {}", destination.display()))?;
    output
        .sync_all()
        .with_context(|| format!("failed to sync {}", destination.display()))
}

fn create_new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata =
        fs::metadata(left).with_context(|| format!("failed to inspect {}", left.display()))?;
    let right_metadata =
        fs::metadata(right).with_context(|| format!("failed to inspect {}", right.display()))?;
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    Ok(file_hash::sha256(left)? == file_hash::sha256(right)?)
}

fn unique_sibling(parent: &Path, prefix: &str) -> PathBuf {
    let mut random = [0_u8; 12];
    OsRng.fill_bytes(&mut random);
    let suffix = crate::cache::lowercase_hex(&random);
    parent.join(format!("{prefix}-{}-{suffix}", std::process::id()))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("failed to determine current directory")?
            .join(path)
    };
    if path.exists() {
        fs::canonicalize(&path).with_context(|| format!("failed to resolve {}", path.display()))
    } else {
        Ok(path)
    }
}

fn user_agent() -> &'static str {
    concat!(
        "lev/",
        env!("CARGO_PKG_VERSION"),
        " (authenticated immutable object transport)"
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::ObjectTransport;

    #[test]
    fn directory_objects_are_create_only_and_bounded() {
        let temp = tempdir().unwrap();
        let remote = temp.path().join("remote");
        let transport = ObjectTransport::parse(remote.to_str().unwrap(), false).unwrap();
        let source = temp.path().join("source");
        fs::write(&source, "payload").unwrap();
        assert!(
            transport
                .publish_if_absent("objects/sha256/value", &source)
                .unwrap()
        );
        assert!(
            !transport
                .publish_if_absent("objects/sha256/value", &source)
                .unwrap()
        );
        let destination = temp.path().join("destination");
        assert!(
            transport
                .fetch("objects/sha256/value", &destination, 7)
                .unwrap()
        );
        assert_eq!(fs::read_to_string(destination).unwrap(), "payload");
    }

    #[test]
    fn rejects_traversal_and_non_loopback_plain_http() {
        let temp = tempdir().unwrap();
        let transport = ObjectTransport::parse(temp.path().to_str().unwrap(), false).unwrap();
        let destination = temp.path().join("destination");
        assert!(transport.fetch("../outside", &destination, 10).is_err());
        assert!(ObjectTransport::parse("http://example.com/cache", false).is_err());
        assert!(ObjectTransport::parse("http://127.0.0.1:1234/cache", false).is_ok());
        assert!(ObjectTransport::parse("http://[::1]:1234/cache", false).is_ok());
        assert!(ObjectTransport::parse("http://example.com/cache", true).is_ok());
        assert!(ObjectTransport::parse("https://user@example.com/cache", false).is_err());
        assert!(ObjectTransport::parse("https://example.com/cache?key=value", false).is_err());
    }
}
