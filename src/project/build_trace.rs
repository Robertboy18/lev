//! Lake's `.trace` files and what changed in them across one build.
//!
//! A trace (schema 2025-09-10) records the hash of each labeled input of one
//! build job, the combined `depHash`, and the artifacts the job produced. Lake
//! names the module, its source file, its options, the Lean commit, and each
//! direct import in those labels, so a changed label is an observed reason for
//! a rebuild. Traces that came from a cache download carry no inputs; for those
//! modules the reason is inferred from imports whose public `.olean` changed.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Package;
use super::import_graph::{downstream, module_from_path};

const MAX_TRACE_BYTES: u64 = 64 * 1024 * 1024;

/// One parsed trace file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trace {
    pub dep_hash: Option<String>,
    /// Labeled input hashes. Nested labels are joined with ` / `.
    pub inputs: BTreeMap<String, String>,
    /// Every artifact named in `outputs`, such as `0123456789abcdef.olean`.
    pub outputs: BTreeSet<String>,
    /// The module's public `.olean`: the first entry of `outputs.o`.
    pub olean: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTrace {
    #[serde(rename = "depHash", default)]
    dep_hash: Value,
    #[serde(default)]
    inputs: Value,
    #[serde(default)]
    outputs: Value,
}

impl Trace {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let raw: RawTrace = serde_json::from_slice(bytes).context("invalid trace JSON")?;
        let mut inputs = BTreeMap::new();
        flatten_inputs(&raw.inputs, &mut Vec::new(), &mut inputs);
        let mut outputs = BTreeSet::new();
        collect_outputs(&raw.outputs, &mut outputs);
        let olean = match raw.outputs.get("o") {
            Some(Value::Array(oleans)) => oleans.first().and_then(Value::as_str),
            Some(Value::String(olean)) => Some(olean.as_str()),
            _ => None,
        }
        .map(str::to_owned);
        let dep_hash = match raw.dep_hash {
            Value::String(hash) => Some(hash),
            Value::Number(hash) => Some(hash.to_string()),
            _ => None,
        };
        Ok(Self {
            dep_hash,
            inputs,
            outputs,
            olean,
        })
    }

    /// The direct imports named by this trace's `imports` inputs.
    pub fn imports(&self) -> BTreeSet<String> {
        self.inputs
            .keys()
            .filter_map(|label| import_of(label))
            .collect()
    }
}

/// Call `visit` for every `.trace` file below `dir`, in path order.
///
/// Symbolic links are not followed. A trace larger than 64 MiB is reported as
/// an error instead of being read.
pub fn visit_traces(dir: &Path, visit: &mut dyn FnMut(&Path, Result<Trace>)) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            visit_traces(&path, visit)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("trace")
        {
            // Lake may replace a trace between the listing and this read.
            let size = match entry.metadata() {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect {}", path.display()));
                }
            };
            let trace = if size > MAX_TRACE_BYTES {
                Err(anyhow::anyhow!("trace is larger than 64 MiB"))
            } else {
                match fs::read(&path) {
                    Ok(bytes) => Trace::parse(&bytes),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        Err(error).with_context(|| format!("failed to read {}", path.display()))
                    }
                }
            };
            visit(&path, trace);
        }
    }
    Ok(())
}

/// One module's trace and the package that owns it.
#[derive(Debug, Clone)]
pub struct ModuleTrace {
    pub package: String,
    pub trace: Trace,
}

/// The module traces of a workspace, keyed by module name.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub modules: BTreeMap<String, ModuleTrace>,
    pub warnings: Vec<String>,
}

impl Snapshot {
    /// Read the traces below `.lake/build/lib/lean` of every package.
    pub fn capture(packages: &[Package]) -> Result<Self> {
        let mut snapshot = Self::default();
        for package in packages {
            let library = package.root.join(".lake/build/lib/lean");
            if !is_directory(&library)? {
                continue;
            }
            let Snapshot { modules, warnings } = &mut snapshot;
            visit_traces(&library, &mut |path, trace| {
                let module = path.strip_prefix(&library).ok().and_then(module_from_path);
                match (module, trace) {
                    (Some(module), Ok(trace)) => match modules.entry(module) {
                        Entry::Occupied(entry) => warnings.push(format!(
                            "module {} has traces in more than one package; kept the first",
                            entry.key()
                        )),
                        Entry::Vacant(entry) => {
                            entry.insert(ModuleTrace {
                                package: package.name.clone(),
                                trace,
                            });
                        }
                    },
                    (None, _) => {
                        warnings.push(format!("{} does not name a Lean module", path.display()))
                    }
                    (Some(_), Err(error)) => {
                        warnings.push(format!("could not read {}: {error:#}", path.display()))
                    }
                }
            })?;
        }
        Ok(snapshot)
    }
}

/// Collect the `depHash` of every trace below `dir` and every artifact the
/// traces name. A missing reference would make a cache entry look unused, so
/// a trace that cannot be read or parsed, or that has no `depHash`, is an
/// error, returned after the readable traces are collected. So is an
/// unreadable directory.
pub fn collect_references(
    dir: &Path,
    hashes: &mut BTreeSet<String>,
    artifacts: &mut BTreeSet<String>,
) -> Result<()> {
    if !is_directory(dir)? {
        return Ok(());
    }
    let mut unreadable = Vec::new();
    visit_traces(dir, &mut |path, trace| match trace {
        Ok(trace) => {
            match trace.dep_hash {
                Some(hash) => {
                    hashes.insert(hash);
                }
                None => unreadable.push(format!("{} has no depHash", path.display())),
            }
            artifacts.extend(trace.outputs);
        }
        Err(error) => unreadable.push(format!("{}: {error:#}", path.display())),
    })?;
    match unreadable.as_slice() {
        [] => Ok(()),
        [only] => bail!("cannot read the trace {only}"),
        [first, ..] => bail!("cannot read {} traces, the first {first}", unreadable.len()),
    }
}

/// Whether `path` is a directory. A path that does not exist is not one; a
/// path that cannot be inspected is an error rather than an empty build.
fn is_directory(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

/// The kind of a labeled trace input, read from Lake's caption for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Source,
    Import,
    Dependency,
    Libraries,
    Toolchain,
    Options,
    Configuration,
    Other,
}

/// One reason a module rebuilt: an input whose hash changed, or for an import,
/// the imported module.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Cause {
    pub kind: InputKind,
    pub input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    /// The module had no trace before the build.
    New,
    /// The module's trace changed.
    Changed,
}

/// How one module's trace changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleChange {
    pub module: String,
    pub package: String,
    pub status: ChangeStatus,
    pub causes: Vec<Cause>,
    /// One of the two traces recorded no inputs, so the causes were inferred
    /// from changed imports instead of read from the trace.
    pub inferred: bool,
}

/// What a build changed in a workspace's module traces.
#[derive(Debug, Default)]
pub struct Comparison {
    pub changes: Vec<ModuleChange>,
    pub removed: Vec<String>,
}

/// Compare the module traces before and after a build.
pub fn compare(before: &Snapshot, after: &Snapshot) -> Comparison {
    let mut comparison = Comparison {
        removed: before
            .modules
            .keys()
            .filter(|module| !after.modules.contains_key(*module))
            .cloned()
            .collect(),
        ..Comparison::default()
    };

    let mut imports = BTreeMap::new();
    let mut changed_oleans = BTreeSet::new();
    for (module, current) in &after.modules {
        let previous = before.modules.get(module).map(|previous| &previous.trace);
        let current = &current.trace;
        let source = if current.inputs.is_empty() {
            previous.unwrap_or(current)
        } else {
            current
        };
        imports.insert(module.clone(), source.imports());
        if previous.and_then(|trace| trace.olean.as_ref()) != current.olean.as_ref() {
            changed_oleans.insert(module.clone());
        }
    }
    let affected = downstream(&imports, &changed_oleans);

    for (module, current) in &after.modules {
        let change = match before.modules.get(module) {
            None => ModuleChange {
                module: module.clone(),
                package: current.package.clone(),
                status: ChangeStatus::New,
                causes: Vec::new(),
                inferred: false,
            },
            Some(previous)
                if previous.trace.dep_hash != current.trace.dep_hash
                    || previous.trace.inputs != current.trace.inputs =>
            {
                let recorded =
                    !previous.trace.inputs.is_empty() && !current.trace.inputs.is_empty();
                let causes = if recorded {
                    label_causes(&previous.trace.inputs, &current.trace.inputs)
                } else {
                    imports[module]
                        .iter()
                        .filter(|import| *import != module && affected.contains(*import))
                        .map(|import| Cause {
                            kind: InputKind::Import,
                            input: import.clone(),
                        })
                        .collect()
                };
                ModuleChange {
                    module: module.clone(),
                    package: current.package.clone(),
                    status: ChangeStatus::Changed,
                    causes,
                    inferred: !recorded,
                }
            }
            Some(_) => continue,
        };
        comparison.changes.push(change);
    }
    comparison
}

/// The causes read from the inputs that differ between two traces.
fn label_causes(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<Cause> {
    let mut causes = BTreeSet::new();
    for (label, hash) in after {
        if before.get(label) != Some(hash) {
            causes.insert(cause_of(label));
        }
    }
    // A vanished input matters unless a new input of its kind replaced it, as
    // a new `Lean <version>, commit` label replaces the old one.
    for label in before.keys().filter(|label| !after.contains_key(*label)) {
        let cause = cause_of(label);
        if cause.kind == InputKind::Import || !causes.iter().any(|known| known.kind == cause.kind) {
            causes.insert(cause);
        }
    }
    causes.into_iter().collect()
}

fn cause_of(label: &str) -> Cause {
    match import_of(label) {
        Some(module) => Cause {
            kind: InputKind::Import,
            input: module,
        },
        None => Cause {
            kind: classify(label),
            input: label.to_owned(),
        },
    }
}

/// Classify an input by the first segment of its label, which Lake writes
/// from a fixed set of captions.
fn classify(label: &str) -> InputKind {
    let first = label.split(" / ").next().unwrap_or(label);
    if first == "deps" || first.ends_with(":deps") {
        InputKind::Dependency
    } else if first == "libs" || first.ends_with(":libs") {
        InputKind::Libraries
    } else if first.starts_with("Lean ") && first.contains(", commit ") {
        InputKind::Toolchain
    } else if first == "options" || first.starts_with("Module.leanArgs:") {
        InputKind::Options
    } else if ["isModule:", "Module.name:", "Package.id?:"]
        .iter()
        .any(|prefix| first.starts_with(prefix))
    {
        InputKind::Configuration
    } else if first.ends_with(".lean") {
        InputKind::Source
    } else {
        InputKind::Other
    }
}

/// The module named by an `imports / X:importArts` or
/// `imports / X transitive imports (public)` input.
fn import_of(label: &str) -> Option<String> {
    let mut segments = label.split(" / ");
    segments.by_ref().find(|segment| *segment == "imports")?;
    let entry = segments.next()?;
    let module = if let Some((module, _)) = entry.rsplit_once(" transitive imports (") {
        module
    } else if let Some((module, _)) = entry.rsplit_once(':') {
        module
    } else {
        entry
    };
    (!module.is_empty()).then(|| module.to_owned())
}

fn flatten_inputs(value: &Value, prefix: &mut Vec<String>, inputs: &mut BTreeMap<String, String>) {
    let Some(entries) = value.as_array() else {
        return;
    };
    for entry in entries {
        let Some([label, child]) = entry.as_array().map(Vec::as_slice) else {
            continue;
        };
        let Some(label) = label.as_str() else {
            continue;
        };
        prefix.push(label.to_owned());
        if let Some(hash) = child.as_str() {
            inputs.insert(prefix.join(" / "), hash.to_owned());
        } else {
            flatten_inputs(child, prefix, inputs);
        }
        prefix.pop();
    }
}

fn collect_outputs(value: &Value, outputs: &mut BTreeSet<String>) {
    match value {
        Value::String(name) => {
            outputs.insert(name.clone());
        }
        // Lake also accepts a bare 64-bit hash for an `.art` artifact.
        Value::Number(hash) => {
            if let Some(hash) = hash.as_u64() {
                outputs.insert(format!("{hash:016x}.art"));
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_outputs(value, outputs)),
        Value::Object(values) => values
            .values()
            .for_each(|value| collect_outputs(value, outputs)),
        Value::Null | Value::Bool(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        Cause, ChangeStatus, InputKind, ModuleTrace, Snapshot, Trace, classify, collect_references,
        compare,
    };
    use crate::project::Package;

    fn trace(value: serde_json::Value) -> Trace {
        Trace::parse(&serde_json::to_vec(&value).unwrap()).unwrap()
    }

    fn module_trace(depends: &str, source: &str, imports: &[(&str, &str)], olean: &str) -> Trace {
        let imports = imports
            .iter()
            .flat_map(|(module, hash)| {
                [
                    json!([format!("{module}:importArts"), hash]),
                    json!([format!("{module} transitive imports (public)"), hash]),
                ]
            })
            .collect::<Vec<_>>();
        trace(json!({
            "depHash": depends,
            "inputs": [
                ["deps", [["imports", imports]]],
                ["Lean 4.x, commit abc", "toolchain"],
                ["/project/Root.lean", source],
                ["options", "none"],
                ["Module.name: Root", "name"]
            ],
            "outputs": {"o": [format!("{olean}.olean"), "1111111111111111.olean.server"], "i": "2222222222222222.ilean", "m": true}
        }))
    }

    fn cached(depends: &str, olean: &str) -> Trace {
        trace(json!({
            "schemaVersion": "2025-09-10",
            "depHash": depends,
            "outputs": {"o": [format!("{olean}.olean")], "m": true}
        }))
    }

    fn snapshot(modules: &[(&str, Trace)]) -> Snapshot {
        Snapshot {
            modules: modules
                .iter()
                .map(|(module, trace)| {
                    (
                        (*module).to_owned(),
                        ModuleTrace {
                            package: "root".to_owned(),
                            trace: trace.clone(),
                        },
                    )
                })
                .collect(),
            warnings: Vec::new(),
        }
    }

    fn causes(kind: InputKind, input: &str) -> Vec<Cause> {
        vec![Cause {
            kind,
            input: input.to_owned(),
        }]
    }

    #[test]
    fn parses_inputs_outputs_and_the_public_olean() {
        let parsed = module_trace("aa", "src", &[("Dep", "d1")], "0123456789abcdef");
        assert_eq!(parsed.dep_hash.as_deref(), Some("aa"));
        assert_eq!(parsed.inputs["deps / imports / Dep:importArts"], "d1");
        assert_eq!(parsed.olean.as_deref(), Some("0123456789abcdef.olean"));
        assert!(parsed.outputs.contains("2222222222222222.ilean"));
        assert!(parsed.outputs.contains("1111111111111111.olean.server"));
        assert_eq!(parsed.imports().into_iter().collect::<Vec<_>>(), ["Dep"]);

        let numeric = trace(json!({"depHash": 42, "outputs": 42}));
        assert_eq!(numeric.dep_hash.as_deref(), Some("42"));
        assert!(numeric.outputs.contains("000000000000002a.art"));
    }

    #[test]
    fn classifies_inputs_by_lake_captions() {
        for (label, kind) in [
            ("/home/me/Mathlib/Data/Option/Basic.lean", InputKind::Source),
            ("Module.name: Mathlib.Lean.Expr", InputKind::Configuration),
            ("Package.id?: (some mathlib)", InputKind::Configuration),
            ("isModule: true", InputKind::Configuration),
            ("Lean 4.x, commit abc", InputKind::Toolchain),
            ("options", InputKind::Options),
            ("Module.leanArgs: #[]", InputKind::Options),
            (
                "deps / mathlib/cache:extraDep / @mathlib:extraDep",
                InputKind::Dependency,
            ),
            ("libs / module dynlibs", InputKind::Libraries),
            ("opaque key", InputKind::Other),
        ] {
            assert_eq!(classify(label), kind, "{label}");
        }
    }

    #[test]
    fn reads_causes_from_changed_labels() {
        let before = snapshot(&[
            (
                "Dep",
                module_trace("d-before", "dep-src-1", &[], "aaaaaaaaaaaaaaaa"),
            ),
            (
                "Root",
                module_trace(
                    "r-before",
                    "root-src",
                    &[("Dep", "dep-1")],
                    "bbbbbbbbbbbbbbbb",
                ),
            ),
            (
                "Gone",
                module_trace("g", "gone-src", &[], "cccccccccccccccc"),
            ),
        ]);
        let after = snapshot(&[
            (
                "Dep",
                module_trace("d-after", "dep-src-2", &[], "dddddddddddddddd"),
            ),
            (
                "Root",
                module_trace(
                    "r-after",
                    "root-src",
                    &[("Dep", "dep-2")],
                    "bbbbbbbbbbbbbbbb",
                ),
            ),
            (
                "Fresh",
                module_trace("f", "fresh-src", &[], "eeeeeeeeeeeeeeee"),
            ),
        ]);

        let comparison = compare(&before, &after);
        assert_eq!(comparison.removed, ["Gone"]);
        let by_module = comparison
            .changes
            .iter()
            .map(|change| (change.module.as_str(), change))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            by_module["Dep"].causes,
            causes(InputKind::Source, "/project/Root.lean")
        );
        assert_eq!(by_module["Root"].causes, causes(InputKind::Import, "Dep"));
        assert!(!by_module["Root"].inferred);
        assert_eq!(by_module["Fresh"].status, ChangeStatus::New);
        assert!(by_module["Fresh"].causes.is_empty());
    }

    #[test]
    fn does_not_list_every_label_when_the_previous_trace_came_from_a_cache() {
        let before = snapshot(&[
            ("Base", cached("base-1", "aaaaaaaaaaaaaaaa")),
            ("Middle", cached("middle-1", "bbbbbbbbbbbbbbbb")),
            ("Top", cached("top-1", "cccccccccccccccc")),
            ("Other", cached("other-1", "eeeeeeeeeeeeeeee")),
        ]);
        let after = snapshot(&[
            (
                "Base",
                module_trace("base-2", "base-src", &[], "dddddddddddddddd"),
            ),
            (
                "Middle",
                module_trace(
                    "middle-2",
                    "middle-src",
                    &[("Base", "b2")],
                    "bbbbbbbbbbbbbbbb",
                ),
            ),
            (
                "Top",
                module_trace("top-2", "top-src", &[("Middle", "m2")], "cccccccccccccccc"),
            ),
            (
                "Other",
                module_trace("other-2", "other-src", &[], "eeeeeeeeeeeeeeee"),
            ),
        ]);

        let comparison = compare(&before, &after);
        let by_module = comparison
            .changes
            .iter()
            .map(|change| (change.module.as_str(), change))
            .collect::<BTreeMap<_, _>>();
        assert!(by_module.values().all(|change| change.inferred));
        assert!(by_module["Base"].causes.is_empty());
        assert_eq!(
            by_module["Middle"].causes,
            causes(InputKind::Import, "Base")
        );
        // Middle's public olean did not change, but Base's did, two hops down.
        assert_eq!(by_module["Top"].causes, causes(InputKind::Import, "Middle"));
        assert!(by_module["Other"].causes.is_empty());
    }

    #[test]
    fn a_new_toolchain_label_replaces_the_old_one() {
        let before = snapshot(&[(
            "Root",
            trace(
                json!({"depHash": "1", "inputs": [["Lean 4.x, commit old", "a"], ["/p/Root.lean", "s"]]}),
            ),
        )]);
        let after = snapshot(&[(
            "Root",
            trace(
                json!({"depHash": "2", "inputs": [["Lean 4.y, commit new", "b"], ["/p/Root.lean", "s"]]}),
            ),
        )]);
        let comparison = compare(&before, &after);
        assert_eq!(
            comparison.changes[0].causes,
            causes(InputKind::Toolchain, "Lean 4.y, commit new")
        );
    }

    #[test]
    fn references_fail_on_any_unreadable_trace_after_reading_the_rest() {
        let temp = tempdir().unwrap();
        let library = temp.path().join("lib/lean/Root");
        fs::create_dir_all(&library).unwrap();
        fs::write(
            library.join("Basic.trace"),
            br#"{"depHash":"aaaaaaaaaaaaaaaa","outputs":{"o":["0123456789abcdef.olean"]}}"#,
        )
        .unwrap();
        let (mut hashes, mut artifacts) = Default::default();
        collect_references(temp.path(), &mut hashes, &mut artifacts).unwrap();
        assert_eq!(hashes.len(), 1);

        fs::write(library.join("Broken.trace"), b"not json").unwrap();
        fs::write(
            library.join("Bare.trace"),
            br#"{"outputs":"1111111111111111.olean"}"#,
        )
        .unwrap();
        let (mut hashes, mut artifacts) = Default::default();
        let error = collect_references(temp.path(), &mut hashes, &mut artifacts)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot read 2 traces"), "{error}");
        assert!(error.contains("Bare.trace has no depHash"), "{error}");
        // What could be read is still collected.
        assert!(hashes.contains("aaaaaaaaaaaaaaaa"));
        assert!(artifacts.contains("1111111111111111.olean"));
        // A build directory that does not exist has no references.
        collect_references(&temp.path().join("absent"), &mut hashes, &mut artifacts).unwrap();
    }

    #[test]
    fn captures_module_traces_of_every_package() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let dependency = temp.path().join("root/.lake/packages/dep");
        let library = root.join(".lake/build/lib/lean/Root");
        fs::create_dir_all(&library).unwrap();
        fs::write(library.join("Basic.trace"), br#"{"depHash":"1"}"#).unwrap();
        fs::write(library.join("Broken.trace"), b"not json").unwrap();
        let dependency_library = dependency.join(".lake/build/lib/lean");
        fs::create_dir_all(&dependency_library).unwrap();
        fs::write(dependency_library.join("Dep.trace"), br#"{"depHash":"2"}"#).unwrap();
        fs::write(dependency_library.join("Dep.olean"), b"not a trace").unwrap();

        let snapshot = Snapshot::capture(&[
            Package {
                name: "root".to_owned(),
                root,
            },
            Package {
                name: "dep".to_owned(),
                root: dependency,
            },
            Package {
                name: "missing".to_owned(),
                root: temp.path().join("missing"),
            },
        ])
        .unwrap();
        assert_eq!(
            snapshot.modules.keys().collect::<Vec<_>>(),
            ["Dep", "Root.Basic"]
        );
        assert_eq!(snapshot.modules["Dep"].package, "dep");
        assert_eq!(snapshot.warnings.len(), 1);
        assert!(
            snapshot.warnings[0].contains("Broken.trace"),
            "{:?}",
            snapshot.warnings
        );
    }
}
