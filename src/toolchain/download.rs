//! Streaming installation from official Lean release archives.
//!
//! Downloads are bounded, hashed, and imported without keeping another copy.
//! Official GitHub metadata selects the native archive; publication into the
//! store happens only after the stream's size and digest have been checked.

use std::io::{self, Read};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::core::http_url::HttpUrl;
use crate::toolchain::store::{ArchiveProvenance, ImportResult, ToolchainStore};

const GITHUB_API: &str = "https://api.github.com/repos";
const MAX_ARCHIVE_DOWNLOAD: u64 = 8 * 1024 * 1024 * 1024;
const MAX_RELEASE_METADATA: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub release: String,
    pub name: String,
    pub url: String,
    pub bytes: u64,
    sha256: Option<String>,
}

impl ReleaseAsset {
    pub fn has_checksum(&self) -> bool {
        self.sha256.is_some()
    }
}

#[derive(Debug)]
pub struct DownloadedToolchain {
    pub imported: ImportResult,
    pub release: String,
    pub archive: String,
    pub compressed_bytes: u64,
    pub sha256: String,
    pub verified: bool,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

enum ReleaseQuery {
    Tag(String),
    Latest,
    First,
}

pub fn resolve(toolchain: &str) -> Result<Option<ReleaseAsset>> {
    let Some((repository, query)) = release_query(toolchain)? else {
        return Ok(None);
    };
    // The override exists for tests and controlled mirrors. When talking to
    // the official endpoint, the returned download URL is pinned to the
    // leanprover GitHub organization as an additional trust check.
    let api = std::env::var("LEV_GITHUB_API_URL").unwrap_or_else(|_| GITHUB_API.to_owned());
    let official_api = api.trim_end_matches('/') == GITHUB_API;
    let endpoint = match query {
        ReleaseQuery::Tag(tag) => {
            format!(
                "{}/{repository}/releases/tags/{tag}",
                api.trim_end_matches('/')
            )
        }
        ReleaseQuery::Latest => {
            format!("{}/{repository}/releases/latest", api.trim_end_matches('/'))
        }
        ReleaseQuery::First => format!(
            "{}/{repository}/releases?per_page=1",
            api.trim_end_matches('/')
        ),
    };

    let endpoint_policy = secure_url(&endpoint, "Lean release metadata URL")?;
    let mut request = ureq::get(&endpoint)
        .config()
        .https_only(endpoint_policy.is_https())
        .build()
        .header("Accept", "application/vnd.github+json")
        .header("Accept-Encoding", "identity")
        .header(
            "User-Agent",
            concat!(
                "lev/",
                env!("CARGO_PKG_VERSION"),
                " (Lean toolchain manager)"
            ),
        );
    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GH_TOKEN").ok())
        .filter(|token| !token.trim().is_empty())
    {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let mut response = request
        .call()
        .with_context(|| format!("failed to query Lean release metadata at {endpoint}"))?;
    let value: serde_json::Value =
        serde_json::from_reader(response.body_mut().as_reader().take(MAX_RELEASE_METADATA))
            .with_context(|| format!("failed to parse Lean release metadata from {endpoint}"))?;
    let release: GithubRelease = if value.is_array() {
        let mut releases: Vec<GithubRelease> =
            serde_json::from_value(value).context("failed to parse Lean release list")?;
        releases
            .drain(..)
            .next()
            .context("Lean release list is empty")?
    } else {
        serde_json::from_value(value).context("failed to parse Lean release")?
    };
    let asset = select_asset(release)?;
    if official_api && !asset.url.starts_with("https://github.com/leanprover/") {
        bail!(
            "official Lean release metadata returned an unexpected asset URL: {}",
            asset.url
        );
    }
    Ok(Some(asset))
}

pub fn download(
    store: &ToolchainStore,
    toolchain: &str,
    asset: &ReleaseAsset,
    allow_unverified: bool,
    progress: impl FnMut(u64),
) -> Result<DownloadedToolchain> {
    if asset.bytes == 0 || asset.bytes > MAX_ARCHIVE_DOWNLOAD {
        bail!(
            "refusing implausible Lean archive size {} for {}",
            asset.bytes,
            asset.name
        );
    }
    if asset.sha256.is_none() && !allow_unverified {
        bail!(
            "{} has no SHA-256 digest in the official release metadata; use elan or pass --allow-unverified",
            asset.name
        );
    }

    let asset_policy = secure_url(&asset.url, "Lean release archive URL")?;
    let mut response = ureq::get(&asset.url)
        .config()
        .https_only(asset_policy.is_https())
        .build()
        .header("Accept", "application/octet-stream")
        .header("Accept-Encoding", "identity")
        .header(
            "User-Agent",
            concat!(
                "lev/",
                env!("CARGO_PKG_VERSION"),
                " (Lean toolchain manager)"
            ),
        )
        .call()
        .with_context(|| format!("failed to download {}", asset.url))?;
    install_reader(
        store,
        toolchain,
        asset,
        allow_unverified,
        response.body_mut().as_reader(),
        progress,
    )
}

fn install_reader<F: FnMut(u64)>(
    store: &ToolchainStore,
    toolchain: &str,
    asset: &ReleaseAsset,
    allow_unverified: bool,
    reader: impl Read,
    progress: F,
) -> Result<DownloadedToolchain> {
    // Decompression feeds the content-addressed store while DigestReader sees
    // the compressed bytes exactly as downloaded. No second archive copy is
    // written to disk.
    let mut reader = DigestReader::new(reader, progress);
    let decoder = zstd::stream::read::Decoder::new(&mut reader)
        .with_context(|| format!("failed to open Zstandard archive {}", asset.name))?;
    let pending = store
        .prepare_tar(toolchain, decoder)
        .with_context(|| format!("failed to import {}", asset.name))?;

    // A decoder may finish before its buffered input is exhausted. Drain the
    // underlying stream so byte-count and digest checks cover the full HTTP
    // response, including trailing data.
    io::copy(&mut reader, &mut io::sink())
        .with_context(|| format!("failed to finish reading {}", asset.name))?;
    let (actual_sha256, downloaded_bytes) = reader.finish();
    if downloaded_bytes != asset.bytes {
        bail!(
            "downloaded {} bytes for {}, expected {}",
            downloaded_bytes,
            asset.name,
            asset.bytes
        );
    }
    let verified = match &asset.sha256 {
        Some(expected) if expected == &actual_sha256 => true,
        Some(expected) => {
            bail!(
                "SHA-256 mismatch for {}: expected {}, received {}",
                asset.name,
                expected,
                actual_sha256
            )
        }
        None if allow_unverified => false,
        None => bail!("{} has no trusted SHA-256 digest", asset.name),
    };

    let imported = pending.publish(ArchiveProvenance {
        name: asset.name.clone(),
        url: asset.url.clone(),
        sha256: actual_sha256.clone(),
        verified,
    })?;
    Ok(DownloadedToolchain {
        imported,
        release: asset.release.clone(),
        archive: asset.name.clone(),
        compressed_bytes: downloaded_bytes,
        sha256: actual_sha256,
        verified,
    })
}

/// Map official selector shapes to upstream release feeds without a version list.
///
/// Tag syntax is validated generically, so future Lean releases work without a
/// lev update or a hardcoded catalog.
fn release_query(toolchain: &str) -> Result<Option<(&'static str, ReleaseQuery)>> {
    let Some((origin, channel)) = toolchain.split_once(':') else {
        return Ok(None);
    };
    match origin {
        "leanprover/lean4" => {
            if channel == "stable" {
                return Ok(Some(("leanprover/lean4", ReleaseQuery::Latest)));
            }
            if valid_stable_tag(channel) {
                return Ok(Some((
                    "leanprover/lean4",
                    ReleaseQuery::Tag(channel.to_owned()),
                )));
            }
        }
        "leanprover/lean4-nightly" => {
            if channel == "nightly" {
                return Ok(Some(("leanprover/lean4-nightly", ReleaseQuery::First)));
            }
            if valid_nightly_tag(channel) {
                return Ok(Some((
                    "leanprover/lean4-nightly",
                    ReleaseQuery::Tag(channel.to_owned()),
                )));
            }
        }
        _ => {}
    }
    Ok(None)
}

fn valid_stable_tag(value: &str) -> bool {
    value.starts_with('v')
        && value[1..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ".+-".contains(ch))
}

fn valid_nightly_tag(value: &str) -> bool {
    value.strip_prefix("nightly-").is_some_and(|date| {
        date.len() == 10
            && date
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
    })
}

fn select_asset(release: GithubRelease) -> Result<ReleaseAsset> {
    let platform = release_platform()?;
    let suffix = format!("-{platform}.tar.zst");
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name.ends_with(&suffix))
        .with_context(|| {
            format!(
                "Lean release {} has no archive for {}/{}",
                release.tag_name,
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
    let sha256 = asset.digest.as_deref().map(parse_sha256).transpose()?;
    Ok(ReleaseAsset {
        release: release.tag_name,
        name: asset.name,
        url: asset.browser_download_url,
        bytes: asset.size,
        sha256,
    })
}

fn release_platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux"),
        ("linux", "aarch64") => Ok("linux_aarch64"),
        ("linux", "x86") => Ok("linux_x86"),
        ("macos", "x86_64") => Ok("darwin"),
        ("macos", "aarch64") => Ok("darwin_aarch64"),
        ("windows", "x86_64") => Ok("windows"),
        (os, arch) => bail!("Lean does not publish a supported archive for {os}/{arch}"),
    }
}

fn parse_sha256(value: &str) -> Result<String> {
    let digest = value
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported release digest {value:?}"))?;
    if !crate::core::hex::is_sha256(digest) {
        bail!("invalid SHA-256 release digest {value:?}");
    }
    Ok(digest.to_owned())
}

fn secure_url(value: &str, subject: &str) -> Result<HttpUrl> {
    let url = HttpUrl::parse(value, subject)?;
    url.require_secure(subject, false)?;
    Ok(url)
}

struct DigestReader<R, F> {
    inner: R,
    hash: Sha256,
    bytes: u64,
    progress: F,
}

impl<R, F> DigestReader<R, F> {
    fn new(inner: R, progress: F) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            bytes: 0,
            progress,
        }
    }

    fn finish(self) -> (String, u64) {
        (format!("{:x}", self.hash.finalize()), self.bytes)
    }
}

impl<R: Read, F: FnMut(u64)> Read for DigestReader<R, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hash.update(&buffer[..read]);
        self.bytes = self
            .bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("download size overflow"))?;
        if self.bytes > MAX_ARCHIVE_DOWNLOAD {
            return Err(io::Error::other(
                "Lean archive exceeded the 8 GiB download safety limit",
            ));
        }
        (self.progress)(self.bytes);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{
        GithubAsset, GithubRelease, ReleaseAsset, ReleaseQuery, install_reader, release_query,
        select_asset,
    };
    use crate::toolchain::store::ToolchainStore;

    #[test]
    fn release_queries_accept_future_versions_without_a_catalog() {
        let Some((repository, ReleaseQuery::Tag(tag))) =
            release_query("leanprover/lean4:v99.123.456-rc7").unwrap()
        else {
            panic!("future release did not select the tag endpoint");
        };
        assert_eq!(repository, "leanprover/lean4");
        assert_eq!(tag, "v99.123.456-rc7");

        assert!(
            release_query("vendor/toolchain:future-channel")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn selects_the_native_tar_zst_asset_and_parses_its_digest() {
        let platform = super::release_platform().unwrap();
        let selected = select_asset(GithubRelease {
            tag_name: "v4.test".to_owned(),
            assets: vec![
                GithubAsset {
                    name: format!("lean-4.test-{platform}.zip"),
                    browser_download_url: "https://example.invalid/lean.zip".to_owned(),
                    size: 10,
                    digest: None,
                },
                GithubAsset {
                    name: format!("lean-4.test-{platform}.tar.zst"),
                    browser_download_url: "https://example.invalid/lean.tar.zst".to_owned(),
                    size: 20,
                    digest: Some(format!("sha256:{}", "a".repeat(64))),
                },
            ],
        })
        .unwrap();
        assert!(selected.name.ends_with(".tar.zst"));
        assert_eq!(selected.sha256.as_deref(), Some("a".repeat(64).as_str()));
    }

    #[test]
    fn streams_a_verified_archive_directly_into_the_store() {
        let temp = tempdir().unwrap();
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        let tar = test_toolchain_tar();
        let compressed = zstd::stream::encode_all(Cursor::new(tar), 1).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(&compressed));
        let asset = ReleaseAsset {
            release: "v4.test".to_owned(),
            name: "lean-4.test-linux.tar.zst".to_owned(),
            url: "https://example.invalid/lean.tar.zst".to_owned(),
            bytes: compressed.len() as u64,
            sha256: Some(sha256.clone()),
        };

        let downloaded = install_reader(
            &store,
            "leanprover/lean4:v4.test",
            &asset,
            false,
            Cursor::new(compressed),
            |_| {},
        )
        .unwrap();
        assert!(downloaded.verified);
        assert_eq!(downloaded.sha256, sha256);
        assert_eq!(
            fs::read_to_string(downloaded.imported.view.join("bin/lean")).unwrap(),
            "#!/bin/sh\n"
        );
        assert_eq!(
            fs::read_to_string(downloaded.imported.view.join("lib/shared")).unwrap(),
            "same bytes"
        );
        store.verify().unwrap();
    }

    #[test]
    fn digest_failure_does_not_publish_a_toolchain_view() {
        let temp = tempdir().unwrap();
        let store = ToolchainStore {
            root: temp.path().join("store"),
        };
        let compressed = zstd::stream::encode_all(Cursor::new(test_toolchain_tar()), 1).unwrap();
        let asset = ReleaseAsset {
            release: "v4.test".to_owned(),
            name: "lean-4.test-linux.tar.zst".to_owned(),
            url: "https://example.invalid/lean.tar.zst".to_owned(),
            bytes: compressed.len() as u64,
            sha256: Some("0".repeat(64)),
        };

        let error = install_reader(
            &store,
            "leanprover/lean4:v4.test",
            &asset,
            false,
            Cursor::new(compressed),
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("SHA-256 mismatch"));
        let stats = store.stats().unwrap();
        assert_eq!(stats.manifests, 0);
        assert_eq!(stats.views, 0);
    }

    fn test_toolchain_tar() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            append_directory(&mut builder, "lean-4.test-linux/");
            append_directory(&mut builder, "lean-4.test-linux/bin/");
            append_file(
                &mut builder,
                "lean-4.test-linux/bin/lean",
                b"#!/bin/sh\n",
                0o755,
            );
            append_directory(&mut builder, "lean-4.test-linux/lib/");
            append_file(
                &mut builder,
                "lean-4.test-linux/lib/shared",
                b"same bytes",
                0o644,
            );
            builder.finish().unwrap();
        }
        bytes
    }

    fn append_directory(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::dir());
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new([]))
            .unwrap();
    }

    fn append_file(
        builder: &mut tar::Builder<&mut Vec<u8>>,
        path: &str,
        contents: &[u8],
        mode: u32,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::file());
        header.set_mode(mode);
        header.set_size(contents.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(contents))
            .unwrap();
    }
}
