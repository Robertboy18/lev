//! Parsing and safety checks for `lake-manifest.json`.
//!
//! Names, revisions, and paths are validated before reaching Git or the filesystem.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LakeManifest {
    #[serde(default)]
    pub version: Option<serde_json::Value>,

    #[serde(rename = "packagesDir", default = "default_packages_dir")]
    pub packages_dir: PathBuf,

    #[serde(default)]
    pub packages: Vec<ManifestPackage>,

    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManifestPackage {
    pub name: String,

    #[serde(rename = "type")]
    pub kind: String,

    #[serde(default)]
    pub url: Option<String>,

    #[serde(default)]
    pub rev: Option<String>,

    #[serde(default)]
    pub scope: Option<String>,

    #[serde(rename = "inputRev", default)]
    pub input_rev: Option<String>,

    #[serde(default)]
    pub inherited: bool,

    #[serde(rename = "subDir", default)]
    pub sub_dir: Option<PathBuf>,

    #[serde(default)]
    pub dir: Option<PathBuf>,

    #[serde(rename = "manifestFile", default)]
    pub manifest_file: Option<PathBuf>,

    #[serde(rename = "configFile", default)]
    pub config_file: Option<PathBuf>,
}

#[derive(Debug)]
pub struct GitPackage<'a> {
    /// Name exactly as serialized by Lake, retained for diagnostics.
    pub name: &'a str,
    /// Filesystem directory selected by Lake's `Name.toString (escape := false)`.
    pub dir_name: String,
    pub url: &'a str,
    pub rev: &'a str,
    /// Symbolic selector Lake resolved, such as a branch or release tag.
    pub input_rev: Option<&'a str>,
}

impl LakeManifest {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn packages_path(&self, project_root: &Path) -> Result<PathBuf> {
        let mut relative = PathBuf::new();
        for component in self.packages_dir.components() {
            match component {
                Component::Normal(value) => relative.push(value),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!(
                        "unsafe packagesDir in lake-manifest.json: {}",
                        self.packages_dir.display()
                    )
                }
            }
        }
        if relative.as_os_str().is_empty() {
            bail!("packagesDir in lake-manifest.json cannot be empty");
        }
        Ok(project_root.join(relative))
    }

    pub fn git_packages(&self) -> Result<Vec<GitPackage<'_>>> {
        let mut names = HashSet::new();
        let mut packages = Vec::new();
        for package in self.packages.iter().filter(|package| package.kind == "git") {
            let dir_name = package_directory_name(&package.name)?;
            if !names.insert(dir_name.clone()) {
                bail!(
                    "duplicate git package directory in lake-manifest.json: {}",
                    package.name
                );
            }
            let url = package
                .url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .with_context(|| {
                    format!(
                        "git package {} has no URL in lake-manifest.json",
                        package.name
                    )
                })?;
            let rev = package
                .rev
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .with_context(|| {
                    format!(
                        "git package {} has no revision in lake-manifest.json",
                        package.name
                    )
                })?;
            validate_revision(&package.name, rev)?;
            packages.push(GitPackage {
                name: &package.name,
                dir_name,
                url,
                rev,
                input_rev: package
                    .input_rev
                    .as_deref()
                    .filter(|value| !value.trim().is_empty()),
            });
        }
        Ok(packages)
    }
}

fn default_packages_dir() -> PathBuf {
    PathBuf::from(".lake/packages")
}

/// Convert Lake's JSON `Name` spelling into its materialized directory name.
///
/// Lake serializes names with source-level `«...»` escaping but explicitly
/// disables that escaping when choosing the checkout directory. Dots inside a
/// quoted component remain literal dots in the resulting single directory
/// name. Parsing this representation, instead of trimming only the outer
/// quotes, also handles names such as `scope.«package-name»`.
pub(crate) fn package_directory_name(name: &str) -> Result<String> {
    if name.is_empty() || name == "[anonymous]" {
        bail!("unsafe package name in lake-manifest.json: {name:?}");
    }

    let mut remaining = name;
    let mut directory = String::with_capacity(name.len());
    loop {
        if let Some(quoted) = remaining.strip_prefix('«') {
            let end = quoted.find('»').with_context(|| {
                format!("invalid quoted package name in lake-manifest.json: {name:?}")
            })?;
            directory.push_str(&quoted[..end]);
            remaining = &quoted[end + '»'.len_utf8()..];
        } else {
            let end = remaining.find('.').unwrap_or(remaining.len());
            let component = &remaining[..end];
            if component.is_empty() || component.contains(['«', '»']) {
                bail!("invalid package name in lake-manifest.json: {name:?}");
            }
            directory.push_str(component);
            remaining = &remaining[end..];
        }

        if remaining.is_empty() {
            break;
        }
        let Some(rest) = remaining.strip_prefix('.') else {
            bail!("invalid package name in lake-manifest.json: {name:?}");
        };
        if rest.is_empty() {
            bail!("invalid package name in lake-manifest.json: {name:?}");
        }
        directory.push('.');
        remaining = rest;
    }

    let path = Path::new(&directory);
    if directory.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        bail!("unsafe package name in lake-manifest.json: {name:?}");
    }
    Ok(directory)
}

pub(crate) fn validate_package_name(name: &str) -> Result<()> {
    package_directory_name(name).map(|_| ())
}

pub(crate) fn validate_revision(package: &str, revision: &str) -> Result<()> {
    if !crate::core::hex::is_git_object_id(revision) {
        bail!("git package {package} has a non-canonical locked revision: {revision:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LakeManifest, package_directory_name};

    #[test]
    fn parses_current_lake_manifest_shape() {
        let manifest: LakeManifest = serde_json::from_str(
            r#"{
                "version": "1.2.0",
                "packagesDir": ".lake/packages",
                "packages": [
                    {
                        "name": "solver",
                        "type": "git",
                        "url": "https://example.invalid/solver.git",
                        "rev": "0123456789abcdef0123456789abcdef01234567"
                    },
                    {
                        "name": "local",
                        "type": "path"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest.packages_path(Path::new("/project")).unwrap(),
            Path::new("/project/.lake/packages")
        );
        let packages = manifest.git_packages().unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "solver");
        assert_eq!(packages[0].dir_name, "solver");
        assert_eq!(packages[0].input_rev, None);

        let legacy: LakeManifest = serde_json::from_str(
            r#"{
                "version": 7,
                "packagesDir": ".lake/packages",
                "packages": []
            }"#,
        )
        .unwrap();
        assert_eq!(legacy.version, Some(serde_json::json!(7)));
    }

    #[test]
    fn rejects_paths_that_escape_the_project() {
        let manifest: LakeManifest =
            serde_json::from_str(r#"{"packagesDir":"../shared","packages":[]}"#).unwrap();
        assert!(manifest.packages_path(Path::new("/project")).is_err());
    }

    #[test]
    fn rejects_package_name_traversal() {
        let manifest: LakeManifest = serde_json::from_str(
            r#"{
                "packages": [{
                    "name": "../victim",
                    "type": "git",
                    "url": "https://example.invalid/repo",
                    "rev": "abc"
                }]
            }"#,
        )
        .unwrap();
        assert!(manifest.git_packages().is_err());
    }

    #[test]
    fn converts_escaped_lean_names_to_safe_lake_directories() {
        assert_eq!(
            package_directory_name("«documentation-tool»").unwrap(),
            "documentation-tool"
        );
        assert_eq!(
            package_directory_name("scope.«package-name»").unwrap(),
            "scope.package-name"
        );
        assert_eq!(
            package_directory_name("scope.«package.with.dots»").unwrap(),
            "scope.package.with.dots"
        );

        for invalid in ["", "[anonymous]", "«unterminated", "name.", "«..»", "«a/b»"] {
            assert!(
                package_directory_name(invalid).is_err(),
                "{invalid:?} must not become a checkout path"
            );
        }
    }
}
