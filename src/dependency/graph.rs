//! Deterministic dependency graphs built from Lake manifests.
//!
//! Child manifests add transitive edges; output is normalized and sorted.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::project::Project;
use crate::project::manifest::{LakeManifest, ManifestPackage, package_directory_name};

#[derive(Debug, Serialize)]
pub struct DependencyGraph {
    pub root: String,
    pub dependencies: Vec<String>,
    pub packages: BTreeMap<String, GraphPackage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphPackage {
    pub name: String,
    pub kind: String,
    pub direct: bool,
    pub source: Option<String>,
    pub revision: Option<String>,
    pub requested: Option<String>,
    pub dependencies: Vec<String>,
}

impl DependencyGraph {
    pub fn load(project: &Project) -> Result<Self> {
        let manifest = LakeManifest::read(&project.manifest_path())?;
        let packages_dir = manifest.packages_path(&project.root)?;
        let root = manifest.name.clone().unwrap_or_else(|| {
            project
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| project.root.display().to_string())
        });
        let dependencies = direct_names(&manifest);
        let mut definitions = manifest
            .packages
            .iter()
            .map(|package| (package.name.clone(), package.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut packages = BTreeMap::new();
        let mut queue = definitions.keys().cloned().collect::<VecDeque<_>>();
        let mut visited = HashSet::new();

        // Child manifests can introduce packages absent from the root
        // manifest. Walking a queue lets those definitions extend the graph
        // without recursion and still terminates on dependency cycles.
        while let Some(name) = queue.pop_front() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let definition = definitions
                .get(&name)
                .with_context(|| format!("missing manifest definition for package {name}"))?
                .clone();
            let package_root = package_root(&project.root, &packages_dir, &definition)?;
            let child_manifest_path = package_root.join(
                definition
                    .manifest_file
                    .as_deref()
                    .unwrap_or_else(|| Path::new("lake-manifest.json")),
            );
            let child_dependencies = if child_manifest_path.is_file() {
                let child = LakeManifest::read(&child_manifest_path)?;
                let child_dependencies = direct_names(&child);
                for package in child.packages {
                    definitions
                        .entry(package.name.clone())
                        .or_insert_with(|| package.clone());
                    if !visited.contains(&package.name) {
                        queue.push_back(package.name.clone());
                    }
                }
                child_dependencies
            } else {
                Vec::new()
            };
            let source = package_source(&definition);
            packages.insert(
                name,
                GraphPackage {
                    name: definition.name,
                    kind: definition.kind,
                    direct: !definition.inherited,
                    source,
                    revision: definition.rev,
                    requested: definition.input_rev,
                    dependencies: child_dependencies,
                },
            );
        }

        for package in packages.values_mut() {
            package.dependencies.sort();
            package.dependencies.dedup();
        }

        Ok(Self {
            root,
            dependencies,
            packages,
        })
    }

    pub fn why(&self, target: &str) -> Option<Vec<String>> {
        if target == self.root {
            return Some(vec![self.root.clone()]);
        }
        // Breadth-first paths make `lev why` return the shortest explanation,
        // which is generally the most useful one in a shared dependency DAG.
        let mut queue = VecDeque::new();
        let mut seen = HashSet::new();
        for dependency in &self.dependencies {
            queue.push_back(vec![self.root.clone(), dependency.clone()]);
        }
        while let Some(path) = queue.pop_front() {
            let name = path.last()?;
            if name == target {
                return Some(path);
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(package) = self.packages.get(name) {
                for dependency in &package.dependencies {
                    let mut next = path.clone();
                    next.push(dependency.clone());
                    queue.push_back(next);
                }
            }
        }
        None
    }

    pub fn render_tree(&self) -> String {
        let mut lines = vec![self.root.clone()];
        let mut expanded = HashSet::new();
        for (index, dependency) in self.dependencies.iter().enumerate() {
            self.render_package(
                dependency,
                "",
                index + 1 == self.dependencies.len(),
                &mut expanded,
                &mut lines,
            );
        }
        lines.join("\n")
    }

    fn render_package(
        &self,
        name: &str,
        prefix: &str,
        last: bool,
        expanded: &mut HashSet<String>,
        lines: &mut Vec<String>,
    ) {
        let connector = if last { "`- " } else { "|- " };
        let label = self
            .packages
            .get(name)
            .map(package_label)
            .unwrap_or_else(|| name.to_owned());
        if !expanded.insert(name.to_owned()) {
            // A package may be reached through several parents. Mark the
            // repeated edge instead of expanding the same subtree forever.
            lines.push(format!("{prefix}{connector}{label} (*)"));
            return;
        }
        lines.push(format!("{prefix}{connector}{label}"));
        let Some(package) = self.packages.get(name) else {
            return;
        };
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "|  " });
        for (index, dependency) in package.dependencies.iter().enumerate() {
            self.render_package(
                dependency,
                &child_prefix,
                index + 1 == package.dependencies.len(),
                expanded,
                lines,
            );
        }
    }
}

fn direct_names(manifest: &LakeManifest) -> Vec<String> {
    let mut names = manifest
        .packages
        .iter()
        .filter(|package| !package.inherited)
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn package_root(root: &Path, packages_dir: &Path, package: &ManifestPackage) -> Result<PathBuf> {
    let mut path = if package.kind == "path" {
        let directory = package
            .dir
            .as_deref()
            .with_context(|| format!("path package {} has no directory", package.name))?;
        root.join(directory)
    } else {
        packages_dir.join(package_directory_name(&package.name)?)
    };
    if let Some(subdir) = &package.sub_dir {
        validate_relative_path(subdir, "package subdirectory")?;
        path.push(subdir);
    }
    if let Some(manifest) = &package.manifest_file {
        validate_relative_path(manifest, "package manifest path")?;
    }
    Ok(path)
}

fn package_source(package: &ManifestPackage) -> Option<String> {
    if package.kind == "path" {
        package
            .dir
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
    } else {
        package.url.clone()
    }
}

fn package_label(package: &GraphPackage) -> String {
    let revision = package
        .revision
        .as_deref()
        .map(|revision| &revision[..revision.len().min(12)]);
    match revision {
        Some(revision) => format!("{} {revision}", package.name),
        None => package.name.clone(),
    }
}

fn validate_relative_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!("unsafe {label}: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::DependencyGraph;
    use crate::project::Project;

    #[test]
    fn loads_tree_and_finds_shortest_dependency_path() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "lean\n").unwrap();
        fs::write(temp.path().join("lakefile.toml"), "name = \"root\"\n").unwrap();
        fs::write(
            temp.path().join("lake-manifest.json"),
            r#"{
              "name": "root",
              "packagesDir": ".lake/packages",
              "packages": [
                {"name":"a","type":"git","url":"https://a","rev":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","inherited":false},
                {"name":"b","type":"git","url":"https://b","rev":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","inherited":true}
              ]
            }"#,
        )
        .unwrap();
        fs::create_dir_all(temp.path().join(".lake/packages/a")).unwrap();
        fs::write(
            temp.path().join(".lake/packages/a/lake-manifest.json"),
            r#"{
              "name": "a",
              "packages": [
                {"name":"b","type":"git","url":"https://b","rev":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","inherited":false}
              ]
            }"#,
        )
        .unwrap();
        let project = Project::discover(temp.path()).unwrap();

        let graph = DependencyGraph::load(&project).unwrap();
        assert_eq!(graph.dependencies, ["a"]);
        assert_eq!(graph.packages["a"].dependencies, ["b"]);
        assert_eq!(graph.why("b").unwrap(), ["root", "a", "b"]);
        assert!(graph.render_tree().contains("`- b bbbbbbbbbbbb"));
    }
}
