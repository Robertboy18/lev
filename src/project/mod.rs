//! Lean project discovery and the packages of its Lake workspace.
//!
//! The nearest `lean-toolchain` file marks the project root.

pub(crate) mod build_diagnostics;
pub(crate) mod build_trace;
pub(crate) mod import_graph;
pub(crate) mod manifest;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use self::manifest::LakeManifest;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub toolchain: String,
}

/// One package of a Lake workspace: the root package or a manifest dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub root: PathBuf,
}

impl Package {
    /// Where Lake writes the package's build: `.lake/build`. lev does not
    /// evaluate lakefiles, so a lakefile that mentions `buildDir` is an
    /// error rather than a guess.
    pub fn build_dir(&self) -> Result<PathBuf> {
        for name in ["lakefile.lean", "lakefile.toml"] {
            let path = self.root.join(name);
            match fs::read_to_string(&path) {
                Ok(text) if text.contains("buildDir") => bail!(
                    "{} may set buildDir, and lev reads traces only below .lake/build",
                    path.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to read {}", path.display()));
                }
            }
        }
        Ok(self.root.join(".lake/build"))
    }
}

impl Project {
    pub fn discover(start: &Path) -> Result<Self> {
        let mut current = absolute(start)?;
        if current.is_file() {
            current.pop();
        }

        loop {
            if current.join("lean-toolchain").is_file() {
                return Self::load(current);
            }
            if !current.pop() {
                break;
            }
        }

        bail!(
            "no Lean project found from {}; expected a lean-toolchain file",
            start.display()
        )
    }

    pub fn load(root: PathBuf) -> Result<Self> {
        let file = root.join("lean-toolchain");
        let toolchain = fs::read_to_string(&file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        let toolchain = toolchain.trim();
        if toolchain.lines().count() != 1 {
            bail!("{} must contain exactly one toolchain", file.display());
        }

        Ok(Self {
            toolchain: normalize_toolchain(toolchain)
                .with_context(|| format!("invalid toolchain in {}", file.display()))?,
            root,
        })
    }

    /// The root package followed by every package that `lake-manifest.json`
    /// lists. Without a manifest the workspace is just the root package.
    pub fn packages(&self) -> Result<Vec<Package>> {
        let manifest_path = self.root.join("lake-manifest.json");
        let manifest = if manifest_path.is_file() {
            Some(LakeManifest::read(&manifest_path)?)
        } else {
            None
        };
        let root_name = manifest
            .as_ref()
            .and_then(|manifest| manifest.name.clone())
            .or_else(|| {
                self.root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "root".to_owned());
        let mut packages = vec![Package {
            name: root_name,
            root: self.root.clone(),
        }];
        if let Some(manifest) = manifest {
            packages.extend(manifest.package_roots(&self.root)?);
        }
        Ok(packages)
    }
}

pub fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()
            .context("failed to determine the current directory")?
            .join(path))
    }
}

/// Expand version shorthand such as `v4.x` into a full elan toolchain name.
///
/// Complete identifiers containing `/` or `:` pass through unchanged.
pub fn normalize_toolchain(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("Lean toolchain cannot be empty");
    }
    if value.chars().any(char::is_whitespace) {
        bail!("Lean toolchain cannot contain whitespace: {value:?}");
    }
    if value.contains('/') || value.contains(':') {
        return Ok(value.to_owned());
    }
    if value == "stable" || value == "beta" {
        return Ok(format!("leanprover/lean4:{value}"));
    }
    if value == "nightly" || value.starts_with("nightly-") {
        return Ok(format!("leanprover/lean4-nightly:{value}"));
    }
    let version = value.strip_prefix('v').unwrap_or(value);
    if version
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        return Ok(format!("leanprover/lean4:v{version}"));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{Package, Project, normalize_toolchain};

    #[test]
    fn discovers_the_nearest_project() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "outer\n").unwrap();
        let nested = temp.path().join("nested");
        let child = nested.join("src");
        fs::create_dir_all(&child).unwrap();
        fs::write(nested.join("lean-toolchain"), "inner\n").unwrap();

        let project = Project::discover(&child).unwrap();
        assert_eq!(project.root, nested);
        assert_eq!(project.toolchain, "inner");
    }

    #[test]
    fn requires_a_nonempty_single_line_toolchain() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "\n").unwrap();
        assert!(Project::discover(temp.path()).is_err());

        fs::write(temp.path().join("lean-toolchain"), "one\ntwo\n").unwrap();
        assert!(Project::discover(temp.path()).is_err());
    }

    #[test]
    fn canonicalizes_version_shorthand() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "v4.fixture-b\n").unwrap();

        let project = Project::discover(temp.path()).unwrap();
        assert_eq!(project.toolchain, "leanprover/lean4:v4.fixture-b");
    }

    #[test]
    fn normalizes_common_toolchain_forms() {
        for (input, expected) in [
            ("4.fixture-b", "leanprover/lean4:v4.fixture-b"),
            ("v4.fixture-b", "leanprover/lean4:v4.fixture-b"),
            ("stable", "leanprover/lean4:stable"),
            ("nightly", "leanprover/lean4-nightly:nightly"),
            (
                "nightly-fixture",
                "leanprover/lean4-nightly:nightly-fixture",
            ),
            (
                "leanprover/lean4:v4.fixture-b",
                "leanprover/lean4:v4.fixture-b",
            ),
            ("99.123.456", "leanprover/lean4:v99.123.456"),
            ("vendor/lean:channel", "vendor/lean:channel"),
            ("local-alias", "local-alias"),
        ] {
            assert_eq!(normalize_toolchain(input).unwrap(), expected, "{input}");
        }
        assert!(normalize_toolchain(" ").is_err());
        assert!(normalize_toolchain("v4.fixture-b bad").is_err());
    }

    #[test]
    fn the_build_directory_is_lake_build_unless_a_lakefile_may_move_it() {
        let temp = tempdir().unwrap();
        let package = Package {
            name: "demo".to_owned(),
            root: temp.path().to_path_buf(),
        };
        assert_eq!(
            package.build_dir().unwrap(),
            temp.path().join(".lake/build")
        );
        fs::write(temp.path().join("lakefile.toml"), "name = \"demo\"\n").unwrap();
        assert_eq!(
            package.build_dir().unwrap(),
            temp.path().join(".lake/build")
        );
        fs::write(
            temp.path().join("lakefile.lean"),
            "package demo where\n  buildDir := \"out\"\n",
        )
        .unwrap();
        let error = package.build_dir().unwrap_err().to_string();
        assert!(error.contains("lakefile.lean may set buildDir"), "{error}");
    }

    #[test]
    fn lists_the_root_and_manifest_packages() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("lean-toolchain"), "test\n").unwrap();
        let project = Project::load(root.clone()).unwrap();
        assert_eq!(
            project.packages().unwrap(),
            [Package {
                name: "project".to_owned(),
                root: root.clone(),
            }]
        );

        fs::write(
            root.join("lake-manifest.json"),
            r#"{
              "name": "demo",
              "packagesDir": ".lake/packages",
              "packages": [
                {"name": "«doc-gen»", "type": "git", "url": "https://example.invalid/doc"},
                {"name": "local", "type": "path", "dir": "../local"},
                {"name": "nested", "type": "git", "subDir": "lean"}
              ]
            }"#,
        )
        .unwrap();
        let names = project
            .packages()
            .unwrap()
            .into_iter()
            .map(|package| (package.name, package.root))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("demo".to_owned(), root.clone()),
                ("«doc-gen»".to_owned(), root.join(".lake/packages/doc-gen")),
                ("local".to_owned(), root.join("../local")),
                ("nested".to_owned(), root.join(".lake/packages/nested/lean")),
            ]
        );
    }
}
