//! Deterministic source-level Lean module import graphs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::project::Project;

const MAX_SOURCE_FILES: usize = 250_000;

#[derive(Debug, Clone, Serialize)]
pub struct ImportNode {
    pub module: String,
    pub path: String,
    pub imports: Vec<String>,
    pub project_imports: usize,
    pub external_imports: usize,
    pub transitive_project_imports: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ImportEdge {
    pub from: String,
    pub to: String,
    pub external: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportGraph {
    pub project: String,
    pub nodes: Vec<ImportNode>,
    pub edges: Vec<ImportEdge>,
    pub cycles: Vec<Vec<String>>,
}

impl ImportGraph {
    pub fn inspect(project: &Project, roots: &[String], include_external: bool) -> Result<Self> {
        let mut sources = BTreeMap::new();
        visit_sources(&project.root, &project.root, &mut sources)?;
        let modules = sources.keys().cloned().collect::<BTreeSet<_>>();
        let selected = selected_modules(&project.root, &modules, &sources, roots)?;
        let reachable = if selected.is_empty() {
            modules.clone()
        } else {
            reachable_modules(&selected, &sources, &modules)
        };

        let mut edges = BTreeSet::new();
        let mut nodes = Vec::with_capacity(reachable.len());
        for module in &reachable {
            let source = sources
                .get(module)
                .context("selected import graph module has no source")?;
            let project_imports = source
                .imports
                .iter()
                .filter(|import| modules.contains(*import))
                .count();
            let external_imports = source.imports.len().saturating_sub(project_imports);
            for import in &source.imports {
                let external = !modules.contains(import);
                if !external || include_external {
                    edges.insert(ImportEdge {
                        from: module.clone(),
                        to: import.clone(),
                        external,
                    });
                }
            }
            let imports = source
                .imports
                .iter()
                .filter(|import| include_external || modules.contains(*import))
                .cloned()
                .collect();
            nodes.push(ImportNode {
                module: module.clone(),
                path: source.path.clone(),
                imports,
                project_imports,
                external_imports,
                transitive_project_imports: transitive_count(module, &sources, &modules),
            });
        }

        Ok(Self {
            project: project.root.display().to_string(),
            cycles: import_cycles(&reachable, &sources),
            nodes,
            edges: edges.into_iter().collect(),
        })
    }
}

#[derive(Debug)]
struct SourceModule {
    path: String,
    imports: Vec<String>,
}

fn visit_sources(
    root: &Path,
    directory: &Path,
    modules: &mut BTreeMap<String, SourceModule>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .context("project source traversal escaped its root")?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            if !excluded(relative) {
                visit_sources(root, &path, modules)?;
            }
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("lean")
        {
            continue;
        }
        if modules.len() >= MAX_SOURCE_FILES {
            bail!("project contains more than {MAX_SOURCE_FILES} Lean source files");
        }
        let module = module_from_path(relative)?;
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read Lean source {}", path.display()))?;
        let previous = modules.insert(
            module.clone(),
            SourceModule {
                path: portable_path(relative),
                imports: parse_imports(&source),
            },
        );
        if previous.is_some() {
            bail!("more than one project source maps to module {module}");
        }
    }
    Ok(())
}

fn excluded(relative: &Path) -> bool {
    relative.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(
                    name.to_str(),
                    Some(
                        ".git"
                            | ".lake"
                            | "_build"
                            | "_target"
                            | "build"
                            | "lake-packages"
                            | "lean_packages"
                            | "target"
                    )
                )
        )
    })
}

fn module_from_path(path: &Path) -> Result<String> {
    let without_extension = path.with_extension("");
    let mut components = Vec::new();
    for component in without_extension.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(
                value
                    .to_str()
                    .map(str::to_owned)
                    .context("Lean source path is not UTF-8")?,
            ),
            _ => bail!("Lean source path is not project-relative"),
        }
    }
    if components.is_empty()
        || components
            .iter()
            .any(|component| !valid_module_component(component))
    {
        bail!(
            "Lean source path {} does not map to a valid module name",
            path.display()
        );
    }
    Ok(components.join("."))
}

fn valid_module_component(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_alphabetic())
        && characters
            .all(|character| character == '_' || character == '\'' || character.is_alphanumeric())
}

fn parse_imports(source: &str) -> Vec<String> {
    let stripped = strip_comments(source);
    let mut imports = BTreeSet::new();
    for line in stripped.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else {
            continue;
        };
        let import = if first == "import" {
            true
        } else if matches!(first, "public" | "private") {
            words.next() == Some("import")
        } else {
            false
        };
        if !import {
            continue;
        }
        for word in words {
            let module = word.trim_matches(|character: char| matches!(character, ',' | ';'));
            if valid_module_name(module) {
                imports.insert(module.to_owned());
            }
        }
    }
    imports.into_iter().collect()
}

fn valid_module_name(value: &str) -> bool {
    !value.is_empty() && value.split('.').all(valid_module_component)
}

fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(source.len());
    let mut index = 0;
    let mut block_depth = 0_u32;
    let mut line_comment = false;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        let pair = bytes.get(index..index.saturating_add(2));
        if line_comment {
            if byte == b'\n' {
                line_comment = false;
                output.push(b'\n');
            } else {
                output.push(b' ');
            }
            index += 1;
            continue;
        }
        if block_depth > 0 {
            if pair == Some(b"/-") {
                block_depth += 1;
                output.extend_from_slice(b"  ");
                index += 2;
            } else if pair == Some(b"-/") {
                block_depth -= 1;
                output.extend_from_slice(b"  ");
                index += 2;
            } else {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                index += 1;
            }
            continue;
        }
        if in_string {
            output.push(if byte == b'\n' { b'\n' } else { b' ' });
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if pair == Some(b"--") {
            line_comment = true;
            output.extend_from_slice(b"  ");
            index += 2;
        } else if pair == Some(b"/-") {
            block_depth = 1;
            output.extend_from_slice(b"  ");
            index += 2;
        } else if byte == b'"' {
            in_string = true;
            output.push(b' ');
            index += 1;
        } else {
            output.push(byte);
            index += 1;
        }
    }
    String::from_utf8(output).expect("comment replacement preserves UTF-8")
}

fn selected_modules(
    project_root: &Path,
    modules: &BTreeSet<String>,
    sources: &BTreeMap<String, SourceModule>,
    roots: &[String],
) -> Result<BTreeSet<String>> {
    let mut selected = BTreeSet::new();
    for root in roots {
        let normalized = if root.ends_with(".lean") || root.contains('/') || root.contains('\\') {
            let path = Path::new(root);
            let relative = if path.is_absolute() {
                path.strip_prefix(project_root).with_context(|| {
                    format!(
                        "Lean source {} is outside project {}",
                        path.display(),
                        project_root.display()
                    )
                })?
            } else {
                path
            };
            module_from_path(relative)?
        } else {
            root.clone()
        };
        if !modules.contains(&normalized) || !sources.contains_key(&normalized) {
            bail!("module {root:?} is not a Lean source in this project");
        }
        selected.insert(normalized);
    }
    Ok(selected)
}

fn reachable_modules(
    roots: &BTreeSet<String>,
    sources: &BTreeMap<String, SourceModule>,
    modules: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut pending = roots.iter().cloned().collect::<Vec<_>>();
    while let Some(module) = pending.pop() {
        if !reachable.insert(module.clone()) {
            continue;
        }
        if let Some(source) = sources.get(&module) {
            pending.extend(
                source
                    .imports
                    .iter()
                    .filter(|import| modules.contains(*import))
                    .cloned(),
            );
        }
    }
    reachable
}

fn transitive_count(
    root: &str,
    sources: &BTreeMap<String, SourceModule>,
    modules: &BTreeSet<String>,
) -> usize {
    let roots = BTreeSet::from([root.to_owned()]);
    reachable_modules(&roots, sources, modules)
        .len()
        .saturating_sub(1)
}

fn import_cycles(
    modules: &BTreeSet<String>,
    sources: &BTreeMap<String, SourceModule>,
) -> Vec<Vec<String>> {
    fn visit(
        module: &str,
        modules: &BTreeSet<String>,
        sources: &BTreeMap<String, SourceModule>,
        temporary: &mut BTreeSet<String>,
        permanent: &mut BTreeSet<String>,
        stack: &mut Vec<String>,
        cycles: &mut BTreeSet<Vec<String>>,
    ) {
        if permanent.contains(module) {
            return;
        }
        if temporary.contains(module) {
            if let Some(start) = stack.iter().position(|entry| entry == module) {
                let mut cycle = stack[start..].to_vec();
                cycle.push(module.to_owned());
                canonicalize_cycle(&mut cycle);
                cycles.insert(cycle);
            }
            return;
        }
        temporary.insert(module.to_owned());
        stack.push(module.to_owned());
        if let Some(source) = sources.get(module) {
            for import in &source.imports {
                if modules.contains(import) {
                    visit(
                        import, modules, sources, temporary, permanent, stack, cycles,
                    );
                }
            }
        }
        stack.pop();
        temporary.remove(module);
        permanent.insert(module.to_owned());
    }

    let mut temporary = BTreeSet::new();
    let mut permanent = BTreeSet::new();
    let mut stack = Vec::new();
    let mut cycles = BTreeSet::new();
    for module in modules {
        visit(
            module,
            modules,
            sources,
            &mut temporary,
            &mut permanent,
            &mut stack,
            &mut cycles,
        );
    }
    cycles.into_iter().collect()
}

fn canonicalize_cycle(cycle: &mut Vec<String>) {
    if cycle.len() <= 2 {
        return;
    }
    cycle.pop();
    let start = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, value)| *value)
        .map(|(index, _)| index)
        .unwrap_or(0);
    cycle.rotate_left(start);
    cycle.push(cycle[0].clone());
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::project::Project;

    use super::{ImportGraph, parse_imports};

    #[test]
    fn parses_modifiers_and_ignores_comments_and_strings() {
        let imports = parse_imports(
            r#"-- import Fake
public import Mathlib.Data.Nat.Prime
/- import Also.Fake -/
private import Project.Helper
def text := "import String.Fake"
"#,
        );
        assert_eq!(imports, ["Mathlib.Data.Nat.Prime", "Project.Helper"]);
    }

    #[test]
    fn selects_reachable_project_modules_and_classifies_external_edges() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "test\n").unwrap();
        fs::write(
            temp.path().join("Main.lean"),
            "import Project.A\nimport Mathlib\n",
        )
        .unwrap();
        fs::create_dir(temp.path().join("Project")).unwrap();
        fs::write(temp.path().join("Project/A.lean"), "import Project.B\n").unwrap();
        fs::write(temp.path().join("Project/B.lean"), "def b := 1\n").unwrap();
        fs::write(temp.path().join("Unused.lean"), "def unused := 1\n").unwrap();
        let project = Project::load(temp.path().to_owned()).unwrap();

        let graph = ImportGraph::inspect(&project, &["Main".to_owned()], true).unwrap();
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|node| node.module.as_str())
                .collect::<Vec<_>>(),
            ["Main", "Project.A", "Project.B"]
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.to == "Mathlib" && edge.external)
        );
        assert_eq!(graph.nodes[0].transitive_project_imports, 2);

        let absolute = temp.path().join("Main.lean").display().to_string();
        let graph = ImportGraph::inspect(&project, &[absolute], false).unwrap();
        assert_eq!(graph.nodes.len(), 3);

        let graph = ImportGraph::inspect(&project, &["./Main.lean".to_owned()], false).unwrap();
        assert_eq!(graph.nodes.len(), 3);
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| !node.imports.iter().any(|import| import == "Mathlib"))
        );
        assert!(graph.edges.iter().all(|edge| !edge.external));
    }
}
