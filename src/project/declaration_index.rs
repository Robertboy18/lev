//! Declaration discovery from Lean server `.ilean` metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::project::manifest::{LakeManifest, package_directory_name};
use crate::project::{Project, import_graph};

const MAX_INDEX_FILES: usize = 250_000;
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DeclarationCandidate {
    pub name: String,
    pub module: String,
    pub source: Option<PathBuf>,
    pub range: Option<DeclarationRange>,
    pub package: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeclarationRange {
    pub start_line: u64,
    pub start_column: u64,
    pub end_line: u64,
    pub end_column: u64,
}

#[derive(Debug, Default)]
pub struct DeclarationSearch {
    pub candidates: Vec<DeclarationCandidate>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct DeclarationInventory {
    pub declarations: Vec<DeclarationCandidate>,
    pub warnings: Vec<String>,
}

pub fn find(project: &Project, name: &str) -> Result<DeclarationSearch> {
    let canonical_name = import_graph::canonical_lean_name(name)?;
    let roots = search_roots(project)?;
    let mut search = DeclarationSearch::default();
    let mut budget = IndexBudget::default();

    for root in roots {
        if !root.index.is_dir() {
            continue;
        }
        visit_index_files(
            &root,
            &root.index,
            &mut budget,
            &mut search.warnings,
            &mut |root, path, contents, warnings| {
                // Reference keys are themselves JSON strings, so their quotes
                // are escaped. This byte check is only a cheap prefilter.
                if !contains_subslice(contents, canonical_name.as_bytes()) {
                    return;
                }
                let Some(index) = parse_index(path, contents, warnings) else {
                    return;
                };
                collect_candidates(root, path, &canonical_name, index, &mut search.candidates);
            },
        )?;
        if budget.stopped {
            break;
        }
    }
    search.candidates.sort_by(|left, right| {
        (
            &left.module,
            &left.package,
            left.source.as_deref().unwrap_or_else(|| Path::new("")),
        )
            .cmp(&(
                &right.module,
                &right.package,
                right.source.as_deref().unwrap_or_else(|| Path::new("")),
            ))
    });
    search.candidates.dedup_by(|left, right| {
        left.name == right.name && left.module == right.module && left.source == right.source
    });
    Ok(search)
}

pub fn project_declarations(project: &Project) -> Result<DeclarationInventory> {
    let root = search_roots(project)?
        .into_iter()
        .next()
        .context("declaration index has no project root")?;
    let mut inventory = DeclarationInventory::default();
    if !root.index.is_dir() {
        return Ok(inventory);
    }

    let mut budget = IndexBudget::default();
    visit_index_files(
        &root,
        &root.index,
        &mut budget,
        &mut inventory.warnings,
        &mut |root, path, contents, warnings| {
            let Some(index) = parse_index(path, contents, warnings) else {
                return;
            };
            let source = source_path(root, path);
            for (name, range) in local_declarations(&index) {
                inventory.declarations.push(DeclarationCandidate {
                    name,
                    module: index.module.clone(),
                    source: source.clone(),
                    range,
                    package: root.package.clone(),
                });
            }
        },
    )?;
    inventory.declarations.sort_by(|left, right| {
        (
            &left.name,
            &left.module,
            left.source.as_deref().unwrap_or_else(|| Path::new("")),
        )
            .cmp(&(
                &right.name,
                &right.module,
                right.source.as_deref().unwrap_or_else(|| Path::new("")),
            ))
    });
    inventory.declarations.dedup_by(|left, right| {
        left.name == right.name && left.module == right.module && left.source == right.source
    });
    Ok(inventory)
}

struct SearchRoot {
    package: String,
    source: PathBuf,
    index: PathBuf,
}

fn search_roots(project: &Project) -> Result<Vec<SearchRoot>> {
    let manifest = LakeManifest::read(&project.manifest_path())?;
    let mut roots = vec![SearchRoot {
        package: manifest
            .name
            .clone()
            .unwrap_or_else(|| "project".to_owned()),
        source: project.root.clone(),
        index: declaration_index_path(&project.root),
    }];
    let packages_path = manifest.packages_path(&project.root)?;

    for package in manifest.packages {
        let mut source = if let Some(directory) = package.dir {
            if directory.is_absolute() {
                directory
            } else {
                project.root.join(directory)
            }
        } else {
            packages_path.join(package_directory_name(&package.name)?)
        };
        if let Some(subdirectory) = package.sub_dir {
            source.push(subdirectory);
        }
        roots.push(SearchRoot {
            package: package.name,
            index: declaration_index_path(&source),
            source,
        });
    }

    let mut seen = BTreeSet::new();
    roots.retain(|root| seen.insert(root.index.clone()));
    Ok(roots)
}

fn declaration_index_path(source: &Path) -> PathBuf {
    let current = source.join(".lake/build/lib/lean");
    if current.is_dir() {
        current
    } else {
        source.join(".lake/build/lib")
    }
}

#[derive(Debug, Default)]
struct IndexBudget {
    files: usize,
    total_bytes: u64,
    stopped: bool,
}

fn visit_index_files<F>(
    root: &SearchRoot,
    directory: &Path,
    budget: &mut IndexBudget,
    warnings: &mut Vec<String>,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(&SearchRoot, &Path, &[u8], &mut Vec<String>),
{
    if budget.stopped {
        return Ok(());
    }
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read declaration index {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read declaration index {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            visit_index_files(root, &entry.path(), budget, warnings, visit)?;
            if budget.stopped {
                return Ok(());
            }
            continue;
        }
        let path = entry.path();
        if !file_type.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("ilean")
        {
            continue;
        }
        if budget.files >= MAX_INDEX_FILES {
            warnings.push(format!(
                "stopped after searching {MAX_INDEX_FILES} declaration indexes"
            ));
            budget.stopped = true;
            return Ok(());
        }
        budget.files += 1;
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.len() > MAX_INDEX_BYTES {
            warnings.push(format!("skipped oversized index {}", path.display()));
            continue;
        }
        budget.total_bytes = budget.total_bytes.saturating_add(metadata.len());
        if budget.total_bytes > MAX_TOTAL_BYTES {
            warnings.push("stopped after reading 8 GiB of declaration indexes".to_owned());
            budget.stopped = true;
            return Ok(());
        }
        let contents =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        visit(root, &path, &contents, warnings);
    }
    Ok(())
}

fn parse_index(path: &Path, contents: &[u8], warnings: &mut Vec<String>) -> Option<IleanFile> {
    match serde_json::from_slice(contents) {
        Ok(index) => Some(index),
        Err(error) => {
            warnings.push(format!("could not parse {}: {error}", path.display()));
            None
        }
    }
}

#[derive(Debug, Deserialize)]
struct IleanFile {
    module: String,
    #[serde(default)]
    decls: BTreeMap<String, Vec<serde_json::Value>>,
    #[serde(default)]
    references: BTreeMap<String, IleanReference>,
}

#[derive(Debug, Deserialize)]
struct IleanReference {
    #[serde(default)]
    definition: Option<Vec<serde_json::Value>>,
}

fn collect_candidates(
    root: &SearchRoot,
    index_path: &Path,
    name: &str,
    index: IleanFile,
    output: &mut Vec<DeclarationCandidate>,
) {
    for (key, reference) in index.references {
        let Some(range) = reference.definition.as_deref().and_then(parse_range) else {
            continue;
        };
        let Ok(key): Result<serde_json::Value, _> = serde_json::from_str(&key) else {
            continue;
        };
        let Some((module, declaration)) = constant_name(&key) else {
            continue;
        };
        if declaration == name {
            output.push(DeclarationCandidate {
                name: declaration,
                module,
                source: source_path(root, index_path),
                range: Some(range),
                package: root.package.clone(),
            });
        }
    }

    if index.decls.contains_key(name)
        && !output
            .iter()
            .any(|candidate| candidate.name == name && candidate.module == index.module)
    {
        output.push(DeclarationCandidate {
            name: name.to_owned(),
            module: index.module,
            source: source_path(root, index_path),
            range: index.decls.get(name).and_then(|range| parse_range(range)),
            package: root.package.clone(),
        });
    }
}

fn local_declarations(index: &IleanFile) -> BTreeMap<String, Option<DeclarationRange>> {
    let mut declarations = index
        .decls
        .iter()
        .map(|(name, values)| (name.clone(), parse_range(values)))
        .collect::<BTreeMap<_, _>>();
    for (key, reference) in &index.references {
        let Some(range) = reference.definition.as_deref().and_then(parse_range) else {
            continue;
        };
        let Ok(key): Result<serde_json::Value, _> = serde_json::from_str(key) else {
            continue;
        };
        let Some((module, declaration)) = constant_name(&key) else {
            continue;
        };
        if module == index.module {
            declarations.entry(declaration).or_insert(Some(range));
        }
    }
    declarations
}

fn constant_name(value: &serde_json::Value) -> Option<(String, String)> {
    let constant = value.get("c")?;
    Some((
        constant.get("m")?.as_str()?.to_owned(),
        constant.get("n")?.as_str()?.to_owned(),
    ))
}

fn parse_range(values: &[serde_json::Value]) -> Option<DeclarationRange> {
    let values = values
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect::<Vec<_>>();
    let values = values.get(values.len().checked_sub(4)?..)?;
    Some(DeclarationRange {
        start_line: values[0] + 1,
        start_column: values[1] + 1,
        end_line: values[2] + 1,
        end_column: values[3] + 1,
    })
}

fn source_path(root: &SearchRoot, index_path: &Path) -> Option<PathBuf> {
    let relative = index_path.strip_prefix(&root.index).ok()?;
    let candidate = root.source.join(relative).with_extension("lean");
    candidate.is_file().then_some(candidate)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{constant_name, parse_range};

    #[test]
    fn reads_constant_reference_keys_and_one_based_ranges() {
        let key = json!({"c": {"m": "Mathlib.Data.Nat.Basic", "n": "Nat.add_comm"}});
        assert_eq!(
            constant_name(&key),
            Some((
                "Mathlib.Data.Nat.Basic".to_owned(),
                "Nat.add_comm".to_owned()
            ))
        );
        let range = parse_range(&[json!(2), json!(4), json!(2), json!(16)]).unwrap();
        assert_eq!(range.start_line, 3);
        assert_eq!(range.start_column, 5);
        assert_eq!(range.end_line, 3);
        assert_eq!(range.end_column, 17);
    }

    #[test]
    fn reads_ranges_from_old_and_current_mixed_ilean_arrays() {
        let old = [
            json!(0),
            json!(6),
            json!(0),
            json!(15),
            json!("trustBase"),
            json!(0),
            json!(0),
            json!(0),
            json!(21),
            json!(0),
            json!(6),
            json!(0),
            json!(15),
        ];
        let current = [json!(0), json!(6), json!(0), json!(15), json!("trustBase")];

        for values in [&old[..], &current[..]] {
            let range = parse_range(values).unwrap();
            assert_eq!(range.start_line, 1);
            assert_eq!(range.start_column, 7);
            assert_eq!(range.end_line, 1);
            assert_eq!(range.end_column, 16);
        }
    }
}
