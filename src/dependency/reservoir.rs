//! Bounded, cacheable Reservoir metadata lookup.
//!
//! Release selection requires an exact Lean toolchain match.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cache::{CacheLayout, digest};
use crate::core::atomic_file::replace as atomic_write;
use crate::core::bounded_io;
use crate::core::clock::now_seconds;
use crate::core::http_url::HttpUrl;
use crate::project::manifest::{ManifestPackage, package_directory_name, validate_package_name};

const RESERVOIR_API: &str = "https://reservoir.lean-lang.org/api/v1";
const CACHE_VERSION: u32 = 2;
const CACHE_TTL: u64 = 60 * 60;
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// One metadata registry selected by project configuration.
///
/// Credentials are referenced by environment-variable name so repository
/// configuration remains safe to commit. The actual token is read only while
/// deriving an authenticated request and its cache partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservoirSource {
    pub name: String,
    pub api_url: String,
    pub token_env: Option<String>,
}

impl Default for ReservoirSource {
    fn default() -> Self {
        Self {
            name: "reservoir".to_owned(),
            api_url: RESERVOIR_API.to_owned(),
            token_env: None,
        }
    }
}

impl ReservoirSource {
    pub fn new(name: &str, api_url: &str, token_env: Option<&str>) -> Result<Self> {
        if !valid_component(name) {
            bail!("invalid registry name: {name:?}");
        }
        let api_url = validate_api_url(api_url)?;
        let token_env = token_env.map(validate_environment_name).transpose()?;
        Ok(Self {
            name: name.to_owned(),
            api_url,
            token_env,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservoirPackage {
    pub owner: String,
    pub name: String,
}

impl ReservoirPackage {
    pub fn from_manifest(package: &ManifestPackage) -> Option<Self> {
        // Lake preserves source-level `«...»` escaping in JSON, while
        // Reservoir package URLs use the unescaped spelling.
        let name = package_directory_name(&package.name).ok()?;
        let owner = package
            .scope
            .as_deref()
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .or_else(|| package.url.as_deref().and_then(github_owner))?;
        if !valid_component(&owner) || !valid_component(&name) {
            return None;
        }
        Some(Self { owner, name })
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservoirVersion {
    pub version: String,
    pub revision: String,
    pub date: String,
    pub tag: Option<String>,
    pub toolchain: String,
    /// Exact package graph observed when Reservoir built this release.
    ///
    /// `Option` is intentional. An omitted graph means the server cannot
    /// support lev's exact-revision prefetch fast path, while an explicitly
    /// empty graph proves that the release has no dependencies.
    #[serde(default)]
    pub dependencies: Option<Vec<ReservoirDependency>>,
}

/// One dependency in Reservoir's flattened release snapshot.
///
/// Reservoir uses Lake's field names, including `type` and `inputRev`. lev
/// needs only immutable Git entries for prefetching, but retaining the other
/// source metadata keeps the parsed response faithful and makes validation
/// errors attributable to the package that supplied them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservoirDependency {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub scope: Option<String>,
    pub rev: String,
    #[serde(rename = "inputRev", default)]
    pub input_rev: Option<String>,
    pub url: String,
    #[serde(default)]
    pub transitive: bool,
}

#[derive(Debug, Clone)]
pub struct VersionSet {
    pub versions: Vec<ReservoirVersion>,
    pub from_cache: bool,
}

#[derive(Debug, Clone)]
pub struct ReservoirClient {
    cache: CacheLayout,
    offline: bool,
    refresh: bool,
    source: ReservoirSource,
}

impl ReservoirClient {
    pub fn new(cache: &CacheLayout, offline: bool, refresh: bool) -> Self {
        Self {
            cache: cache.clone(),
            offline,
            refresh,
            source: ReservoirSource::default(),
        }
    }

    pub fn with_source(mut self, source: ReservoirSource) -> Self {
        self.source = source;
        self
    }

    pub fn versions(&self, package: &ReservoirPackage) -> Result<VersionSet> {
        let cached = self.read_cache(package)?;
        if !self.refresh {
            if let Some(entry) = &cached {
                if self.offline || now_seconds().saturating_sub(entry.fetched_at) < CACHE_TTL {
                    return Ok(VersionSet {
                        versions: entry.response.data.clone(),
                        from_cache: true,
                    });
                }
            }
        }
        if self.offline {
            bail!(
                "no cached Reservoir metadata is available for {}",
                package.full_name()
            );
        }

        let response = self.fetch(package)?;
        self.write_cache(package, &response)?;
        Ok(VersionSet {
            versions: response.data,
            from_cache: false,
        })
    }

    fn fetch(&self, package: &ReservoirPackage) -> Result<VersionsResponse> {
        let api = self.effective_api_url()?;
        let endpoint = format!(
            "{}/packages/{}/{}/versions",
            api, package.owner, package.name
        );
        let endpoint_policy = HttpUrl::parse(&endpoint, "registry URL")?;
        let mut request = ureq::get(&endpoint)
            .config()
            .https_only(endpoint_policy.is_https())
            .build()
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity")
            .header(
                "User-Agent",
                concat!("lev/", env!("CARGO_PKG_VERSION"), " (Lean package manager)"),
            );
        if let Some(token) = self.auth_token()? {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = request.call().with_context(|| {
            format!(
                "failed to query registry {} for {} at {endpoint}",
                self.source.name,
                package.full_name(),
            )
        })?;
        let bytes = bounded_io::read_to_end(
            response.body_mut().as_reader(),
            MAX_RESPONSE_BYTES,
            format!("Reservoir response from {endpoint}"),
        )?;
        let parsed: VersionsResponse = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse Reservoir response from {endpoint}"))?;
        parsed.validate(package)?;
        Ok(parsed)
    }

    fn read_cache(&self, package: &ReservoirPackage) -> Result<Option<CacheEntry>> {
        let source_key = self.source_key()?;
        let path = self.cache_path(package, &source_key);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let entry: CacheEntry = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if entry.version != CACHE_VERSION
            || entry.source_key != source_key
            || entry.package != *package
        {
            return Ok(None);
        }
        entry.response.validate(package)?;
        Ok(Some(entry))
    }

    fn write_cache(&self, package: &ReservoirPackage, response: &VersionsResponse) -> Result<()> {
        let root = self.cache.reservoir_root();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        let source_key = self.source_key()?;
        let entry = CacheEntry {
            version: CACHE_VERSION,
            source_key: source_key.clone(),
            package: package.clone(),
            fetched_at: now_seconds(),
            response: response.clone(),
        };
        atomic_write(
            &self.cache_path(package, &source_key),
            &serde_json::to_vec(&entry)?,
        )
    }

    fn cache_path(&self, package: &ReservoirPackage, source_key: &str) -> PathBuf {
        self.cache.reservoir_root().join(format!(
            "{}.json",
            digest(format!("{source_key}\0{}", package.full_name()).as_bytes())
        ))
    }

    fn effective_api_url(&self) -> Result<String> {
        match std::env::var("LEV_RESERVOIR_API_URL") {
            Ok(value) => validate_api_url(&value),
            Err(std::env::VarError::NotPresent) => Ok(self.source.api_url.clone()),
            Err(error) => Err(error).context("failed to read LEV_RESERVOIR_API_URL"),
        }
    }

    fn auth_token(&self) -> Result<Option<String>> {
        if let Some(name) = &self.source.token_env {
            let token = std::env::var(name).with_context(|| {
                format!(
                    "registry {} requires environment variable {name}",
                    self.source.name
                )
            })?;
            if token.trim().is_empty() {
                bail!(
                    "registry {} requires non-empty environment variable {name}",
                    self.source.name
                );
            }
            return Ok(Some(token));
        }
        Ok(std::env::var("LEV_RESERVOIR_TOKEN")
            .ok()
            .filter(|token| !token.trim().is_empty()))
    }

    fn source_key(&self) -> Result<String> {
        let api = self.effective_api_url()?;
        let credential = self
            .auth_token()?
            .map(|token| digest(token.as_bytes()))
            .unwrap_or_else(|| "anonymous".to_owned());
        Ok(digest(
            format!("{}\0{api}\0{credential}", self.source.name).as_bytes(),
        ))
    }
}

pub fn compatible_release<'a>(
    versions: &'a [ReservoirVersion],
    toolchain: &str,
) -> Option<&'a ReservoirVersion> {
    versions
        .iter()
        .find(|version| version.toolchain == toolchain && version.tag.is_some())
        .or_else(|| {
            versions
                .iter()
                .find(|version| version.toolchain == toolchain)
        })
}

pub fn latest_release(versions: &[ReservoirVersion]) -> Option<&ReservoirVersion> {
    versions
        .iter()
        .find(|version| version.tag.is_some())
        .or_else(|| versions.first())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionsResponse {
    #[serde(rename = "schemaVersion")]
    schema_version: String,
    data: Vec<ReservoirVersion>,
}

impl VersionsResponse {
    fn validate(&self, package: &ReservoirPackage) -> Result<()> {
        if !self.schema_version.starts_with("1.") {
            bail!(
                "Reservoir returned unsupported schema {} for {}",
                self.schema_version,
                package.full_name()
            );
        }
        for version in &self.data {
            if !canonical_revision(&version.revision) {
                bail!(
                    "Reservoir returned an invalid revision for {}: {:?}",
                    package.full_name(),
                    version.revision
                );
            }
            if version.toolchain.trim().is_empty()
                || version.toolchain.contains(char::is_whitespace)
            {
                bail!(
                    "Reservoir returned an invalid toolchain for {}: {:?}",
                    package.full_name(),
                    version.toolchain
                );
            }
            if let Some(dependencies) = &version.dependencies {
                let mut names = std::collections::HashSet::new();
                for dependency in dependencies {
                    validate_package_name(&dependency.name).with_context(|| {
                        format!(
                            "Reservoir returned an invalid dependency name for {}",
                            package.full_name()
                        )
                    })?;
                    if !names.insert(package_directory_name(&dependency.name)?) {
                        bail!(
                            "Reservoir returned duplicate dependency {} for {}",
                            dependency.name,
                            package.full_name()
                        );
                    }
                    if dependency.kind == "git" {
                        if dependency.url.trim().is_empty() {
                            bail!(
                                "Reservoir returned an empty Git URL for dependency {} of {}",
                                dependency.name,
                                package.full_name()
                            );
                        }
                        if !canonical_revision(&dependency.rev) {
                            bail!(
                                "Reservoir returned an invalid revision for dependency {} of {}: {:?}",
                                dependency.name,
                                package.full_name(),
                                dependency.rev
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    version: u32,
    source_key: String,
    package: ReservoirPackage,
    fetched_at: u64,
    response: VersionsResponse,
}

fn github_owner(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let (owner, _) = path.trim_end_matches('/').split_once('/')?;
    valid_component(owner).then(|| owner.to_owned())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_api_url(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let url = HttpUrl::parse(value, "registry URL")?;
    url.require_secure("registry URL", false)?;
    Ok(value.to_owned())
}

fn validate_environment_name(value: &str) -> Result<String> {
    let mut bytes = value.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic());
    if !valid_start || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()) {
        bail!("invalid registry token environment variable: {value:?}");
    }
    Ok(value.to_owned())
}

fn canonical_revision(revision: &str) -> bool {
    crate::core::hex::is_git_object_id(revision)
}

#[cfg(test)]
mod tests {
    use crate::project::manifest::LakeManifest;

    use super::{
        ReservoirPackage, ReservoirVersion, VersionsResponse, compatible_release, latest_release,
    };

    #[test]
    fn identifies_registry_and_github_packages_and_selects_compatible_releases() {
        let manifest: LakeManifest = serde_json::from_str(
            r#"{
                "packages": [
                    {
                        "name": "solver",
                        "type": "git",
                        "url": "https://github.com/example-org/solver.git",
                        "rev": "0123456789abcdef0123456789abcdef01234567"
                    },
                    {
                        "name": "utilities",
                        "type": "git",
                        "scope": "package-owner",
                        "url": "https://example.invalid/utilities",
                        "rev": "0123456789abcdef0123456789abcdef01234567"
                    },
                    {
                        "name": "«documentation»",
                        "type": "git",
                        "url": "https://github.com/docs-org/documentation.git",
                        "rev": "0123456789abcdef0123456789abcdef01234567"
                    }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(
            ReservoirPackage::from_manifest(&manifest.packages[0])
                .unwrap()
                .full_name(),
            "example-org/solver"
        );
        assert_eq!(
            ReservoirPackage::from_manifest(&manifest.packages[1])
                .unwrap()
                .full_name(),
            "package-owner/utilities"
        );
        assert_eq!(
            ReservoirPackage::from_manifest(&manifest.packages[2])
                .unwrap()
                .full_name(),
            "docs-org/documentation"
        );

        let versions: Vec<ReservoirVersion> = serde_json::from_str(
            r#"[
                {
                    "version": "0.0.0",
                    "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "date": "2026-02-01T00:00:00Z",
                    "tag": "v4.fixture-c",
                    "toolchain": "leanprover/lean4:v4.fixture-c",
                    "dependencies": []
                },
                {
                    "version": "0.0.0",
                    "revision": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "date": "2026-01-01T00:00:00Z",
                    "tag": "v4.fixture-b",
                    "toolchain": "leanprover/lean4:v4.fixture-b"
                }
            ]"#,
        )
        .unwrap();
        assert_eq!(
            compatible_release(&versions, "leanprover/lean4:v4.fixture-b")
                .unwrap()
                .tag
                .as_deref(),
            Some("v4.fixture-b")
        );
        assert_eq!(
            latest_release(&versions).unwrap().tag.as_deref(),
            Some("v4.fixture-c")
        );
        assert_eq!(versions[0].dependencies.as_deref().map(<[_]>::len), Some(0));
        assert!(versions[1].dependencies.is_none());
    }

    #[test]
    fn validates_flattened_dependency_snapshots_before_they_become_git_inputs() {
        let package = ReservoirPackage {
            owner: "example".to_owned(),
            name: "root".to_owned(),
        };
        let valid: VersionsResponse = serde_json::from_str(
            r#"{
                "schemaVersion": "1.2.0",
                "data": [{
                    "version": "1.0.0",
                    "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "date": "2026-01-01T00:00:00Z",
                    "tag": "v1.0.0",
                    "toolchain": "leanprover/lean4:v4.fixture-c",
                    "dependencies": [{
                        "type": "git",
                        "name": "child",
                        "scope": "example",
                        "rev": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "inputRev": "main",
                        "url": "https://example.invalid/child",
                        "transitive": false
                    }]
                }]
            }"#,
        )
        .unwrap();
        valid.validate(&package).unwrap();

        let mut invalid = valid;
        invalid.data[0].dependencies.as_mut().unwrap()[0].rev = "main".to_owned();
        let error = invalid.validate(&package).unwrap_err().to_string();
        assert!(error.contains("invalid revision"), "{error}");
        assert!(error.contains("child"), "{error}");
    }
}
