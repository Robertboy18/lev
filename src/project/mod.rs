//! Lean project discovery and canonical metadata.
//!
//! The nearest `lean-toolchain` file defines the project root.

pub(crate) mod audit;
pub(crate) mod config;
pub(crate) mod export;
pub(crate) mod lakefile;
pub(crate) mod local_workspace;
pub(crate) mod lockfile;
pub(crate) mod manifest;
pub(crate) mod workspace;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub toolchain: String,
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
        if toolchain.is_empty() {
            bail!("{} is empty", file.display());
        }
        if toolchain.lines().count() != 1 {
            bail!("{} must contain exactly one toolchain", file.display());
        }

        Ok(Self {
            root,
            toolchain: crate::toolchain::normalize(toolchain)?,
        })
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("lake-manifest.json")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("lev.lock")
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::Project;

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
}
