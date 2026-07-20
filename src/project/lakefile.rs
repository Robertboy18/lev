//! Structured edits for declarative `lakefile.toml` projects.
//!
//! Executable `lakefile.lean` files are never rewritten.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::core::atomic_file::replace as atomic_replace;

#[derive(Debug)]
pub enum DependencySource {
    Registry { scope: Option<String> },
    Git { url: String },
    Path { path: PathBuf },
}

#[derive(Debug)]
pub struct Dependency {
    pub name: String,
    pub source: DependencySource,
    pub rev: Option<String>,
}

pub fn config_path(project_root: &Path) -> Result<PathBuf> {
    let toml = project_root.join("lakefile.toml");
    if toml.is_file() {
        return Ok(toml);
    }
    let lean = project_root.join("lakefile.lean");
    if lean.is_file() {
        bail!(
            "automatic dependency edits currently require lakefile.toml; {} is a Lean program",
            lean.display()
        );
    }
    bail!(
        "no lakefile.toml found in {}; initialize a Lake project first",
        project_root.display()
    )
}

pub fn add(path: &Path, dependency: &Dependency, replace: bool) -> Result<()> {
    validate_name(&dependency.name)?;
    let mut document = read(path)?;
    let requirements = requirements_mut(&mut document)?;
    let existing = requirements.iter().position(|table| {
        table
            .get("name")
            .and_then(Item::as_str)
            .is_some_and(|name| name == dependency.name)
    });

    let mut table = dependency_table(dependency);
    if let Some(index) = existing {
        if !replace {
            bail!(
                "dependency {} already exists in {}; pass --replace to update it",
                dependency.name,
                path.display()
            );
        }
        std::mem::swap(
            requirements
                .get_mut(index)
                .context("internal error: missing dependency table")?,
            &mut table,
        );
    } else {
        requirements.push(table);
    }
    write(path, &document)
}

pub fn remove(path: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let mut document = read(path)?;
    let requirements = requirements_mut(&mut document)?;
    let index = requirements
        .iter()
        .position(|table| {
            table
                .get("name")
                .and_then(Item::as_str)
                .is_some_and(|candidate| candidate == name)
        })
        .with_context(|| format!("dependency {name} is not declared in {}", path.display()))?;
    requirements.remove(index);
    write(path, &document)
}

pub fn set_revision(path: &Path, name: &str, revision: &str) -> Result<()> {
    set_revisions(
        path,
        &BTreeMap::from([(name.to_owned(), revision.to_owned())]),
    )
}

/// Replace several direct dependency revisions in one parsed transaction.
///
/// Environment activation can change several direct packages together.
/// Parsing and writing once prevents a partially applied overlay if a later
/// package name is absent or malformed.
pub fn set_revisions(path: &Path, revisions: &BTreeMap<String, String>) -> Result<()> {
    if revisions.is_empty() {
        return Ok(());
    }
    for (name, revision) in revisions {
        validate_name(name)?;
        validate_revision(revision)?;
    }
    let mut document = read(path)?;
    let requirements = requirements_mut(&mut document)?;
    for (name, revision) in revisions {
        let requirement = requirements
            .iter_mut()
            .find(|table| {
                table
                    .get("name")
                    .and_then(Item::as_str)
                    .is_some_and(|candidate| candidate == name)
            })
            .with_context(|| format!("dependency {name} is not declared in {}", path.display()))?;
        requirement["rev"] = value(revision);
    }
    write(path, &document)
}

/// Return the unique direct dependency names declared in a TOML Lakefile.
pub fn dependency_names(path: &Path) -> Result<BTreeSet<String>> {
    let document = read(path)?;
    let Some(requirements) = document.get("require") else {
        return Ok(BTreeSet::new());
    };
    let requirements = requirements.as_array_of_tables().with_context(|| {
        format!(
            "lakefile.toml field `require` in {} is not an array of tables",
            path.display()
        )
    })?;
    let mut names = BTreeSet::new();
    for requirement in requirements {
        let name = requirement
            .get("name")
            .and_then(Item::as_str)
            .with_context(|| format!("dependency in {} has no string name", path.display()))?;
        validate_name(name)?;
        if !names.insert(name.to_owned()) {
            bail!(
                "dependency {name} is declared more than once in {}",
                path.display()
            );
        }
    }
    Ok(names)
}

fn read(path: &Path) -> Result<DocumentMut> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    source
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn requirements_mut(document: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    if !document.contains_key("require") {
        document["require"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    document["require"]
        .as_array_of_tables_mut()
        .context("lakefile.toml field `require` is not an array of tables")
}

fn dependency_table(dependency: &Dependency) -> Table {
    let mut table = Table::new();
    table["name"] = value(&dependency.name);
    match &dependency.source {
        DependencySource::Registry { scope } => {
            if let Some(scope) = scope {
                table["scope"] = value(scope);
            }
        }
        DependencySource::Git { url } => {
            table["git"] = value(url);
        }
        DependencySource::Path { path } => {
            table["path"] = value(path.to_string_lossy().as_ref());
        }
    }
    if let Some(rev) = &dependency.rev {
        table["rev"] = value(rev);
    }
    table
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.chars().any(char::is_whitespace) {
        bail!("invalid Lake package name: {name:?}");
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<()> {
    if revision.trim().is_empty() || revision.contains(char::is_whitespace) {
        bail!("invalid dependency revision: {revision:?}");
    }
    Ok(())
}

fn write(path: &Path, document: &DocumentMut) -> Result<()> {
    atomic_replace(path, document.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        Dependency, DependencySource, add, dependency_names, remove, set_revision, set_revisions,
    };

    #[test]
    fn adds_replaces_and_removes_a_dependency_without_losing_comments() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("lakefile.toml");
        fs::write(&path, "name = \"demo\"\n# keep this\n").unwrap();
        let dependency = Dependency {
            name: "example".to_owned(),
            source: DependencySource::Registry {
                scope: Some("owner".to_owned()),
            },
            rev: Some("v1.2.0".to_owned()),
        };

        add(&path, &dependency, false).unwrap();
        let added = fs::read_to_string(&path).unwrap();
        assert!(added.contains("# keep this"));
        assert!(added.contains("[[require]]"));
        assert!(added.contains("scope = \"owner\""));
        assert!(add(&path, &dependency, false).is_err());

        let replacement = Dependency {
            name: "example".to_owned(),
            source: DependencySource::Git {
                url: "https://example.invalid/example.git".to_owned(),
            },
            rev: Some("main".to_owned()),
        };
        add(&path, &replacement, true).unwrap();
        let replaced = fs::read_to_string(&path).unwrap();
        assert!(replaced.contains("git = \"https://example.invalid/example.git\""));
        assert!(!replaced.contains("scope ="));

        set_revision(&path, "example", "v1.3.0").unwrap();
        let upgraded = fs::read_to_string(&path).unwrap();
        assert!(upgraded.contains("rev = \"v1.3.0\""));
        assert!(upgraded.contains("# keep this"));

        remove(&path, "example").unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("example"));
    }

    #[test]
    fn applies_environment_revisions_atomically() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("lakefile.toml");
        fs::write(
            &path,
            r#"
[[require]]
name = "solver"
rev = "v1.0.0"

[[require]]
name = "linter"
rev = "v1.0.0"
"#,
        )
        .unwrap();
        assert_eq!(
            dependency_names(&path).unwrap(),
            ["linter".to_owned(), "solver".to_owned()].into()
        );

        set_revisions(
            &path,
            &[
                ("linter".to_owned(), "v1.1.0".to_owned()),
                ("solver".to_owned(), "v1.1.0".to_owned()),
            ]
            .into(),
        )
        .unwrap();
        let changed = fs::read_to_string(&path).unwrap();
        assert_eq!(changed.matches("v1.1.0").count(), 2);

        let before = changed;
        let error = set_revisions(
            &path,
            &[
                ("solver".to_owned(), "v1.2.0".to_owned()),
                ("missing".to_owned(), "v1".to_owned()),
            ]
            .into(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("missing"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
}
