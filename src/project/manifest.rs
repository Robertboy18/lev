//! The parts of `lake-manifest.json` that locate a workspace's packages.
//!
//! Package names and paths are checked before they are joined onto the
//! project root.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::Package;

#[derive(Debug, Clone, Deserialize)]
pub struct LakeManifest {
    #[serde(rename = "packagesDir", default = "default_packages_dir")]
    pub packages_dir: PathBuf,

    #[serde(default)]
    pub packages: Vec<ManifestPackage>,

    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestPackage {
    pub name: String,

    #[serde(rename = "type")]
    pub kind: String,

    #[serde(default)]
    pub dir: Option<PathBuf>,

    #[serde(rename = "subDir", default)]
    pub sub_dir: Option<PathBuf>,
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

    /// The directory of every listed package, in manifest order.
    ///
    /// Git packages live in `packagesDir` under their unescaped name; path
    /// packages live at their `dir`, relative to the project root.
    pub fn package_roots(&self, project_root: &Path) -> Result<Vec<Package>> {
        let packages_dir = self.packages_path(project_root)?;
        self.packages
            .iter()
            .map(|package| {
                let mut root = if package.kind == "path" {
                    let dir = package.dir.as_deref().with_context(|| {
                        format!(
                            "path package {} has no dir in lake-manifest.json",
                            package.name
                        )
                    })?;
                    project_root.join(dir)
                } else {
                    packages_dir.join(package_directory_name(&package.name)?)
                };
                if let Some(sub_dir) = &package.sub_dir {
                    if sub_dir.as_os_str().is_empty()
                        || !sub_dir.components().all(|component| {
                            matches!(component, Component::Normal(_) | Component::CurDir)
                        })
                    {
                        bail!(
                            "package {} has an unsafe subDir in lake-manifest.json: {}",
                            package.name,
                            sub_dir.display()
                        );
                    }
                    root.push(sub_dir);
                }
                Ok(Package {
                    name: package.name.clone(),
                    root,
                })
            })
            .collect()
    }
}

fn default_packages_dir() -> PathBuf {
    PathBuf::from(".lake/packages")
}

/// Convert Lake's JSON `Name` spelling into its checkout directory name.
///
/// Lake writes names with source-level `«...»` escaping but disables that
/// escaping when it picks the directory, so `scope.«package-name»` is checked
/// out at `scope.package-name`.
fn package_directory_name(name: &str) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LakeManifest, package_directory_name};

    #[test]
    fn reads_the_current_lake_manifest_shape() {
        let manifest: LakeManifest = serde_json::from_str(
            r#"{
                "version": "1.2.0",
                "name": "root",
                "packagesDir": ".lake/packages",
                "packages": [
                    {
                        "name": "solver",
                        "type": "git",
                        "url": "https://example.invalid/solver.git",
                        "rev": "0123456789abcdef0123456789abcdef01234567"
                    },
                    {"name": "local", "type": "path", "dir": "vendor/local"}
                ]
            }"#,
        )
        .unwrap();

        let roots = manifest.package_roots(Path::new("/project")).unwrap();
        assert_eq!(roots[0].root, Path::new("/project/.lake/packages/solver"));
        assert_eq!(roots[1].root, Path::new("/project/vendor/local"));
        assert_eq!(manifest.name.as_deref(), Some("root"));
    }

    #[test]
    fn rejects_paths_that_escape_the_project() {
        let manifest: LakeManifest =
            serde_json::from_str(r#"{"packagesDir":"../shared","packages":[]}"#).unwrap();
        assert!(manifest.packages_path(Path::new("/project")).is_err());

        let manifest: LakeManifest =
            serde_json::from_str(r#"{"packages":[{"name":"../victim","type":"git"}]}"#).unwrap();
        assert!(manifest.package_roots(Path::new("/project")).is_err());

        let manifest: LakeManifest =
            serde_json::from_str(r#"{"packages":[{"name":"dep","type":"git","subDir":"../up"}]}"#)
                .unwrap();
        assert!(manifest.package_roots(Path::new("/project")).is_err());
    }

    #[test]
    fn converts_escaped_lean_names_to_lake_directories() {
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
