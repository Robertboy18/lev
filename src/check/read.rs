//! Reader for `lev-export/1` directories and plain LeanExport 3.1.0 files.
//!
//! Record shapes follow the toolchain's `LeanExport/Basic.lean` (writer) and
//! `LeanExport/Parse.lean` (reader). Names are interned once for the whole
//! development; levels and expressions stay in one arena per module, indexed
//! as in that module's file. Indices must be dense, as LeanExport writes them.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::parallel_map;

/// Format identifier of a per-module export directory.
pub(crate) const EXPORT_FORMAT: &str = "lev-export/1";
/// The only LeanExport format version this reader accepts.
pub(crate) const LEAN_EXPORT_VERSION: &str = "3.1.0";

/// Global name index; 0 is the anonymous name.
pub(crate) type NameId = u32;
pub(crate) const ANONYMOUS: NameId = 0;

/// One component of a hierarchical name. As JSON, a string or a number.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Component {
    Str(Box<str>),
    Num(u64),
}

/// Hierarchical names shared by every module of a development.
///
/// A name is identified by its id, or across developments by its
/// components. Its printed form is for messages only: `«Classical.choice»`
/// (one component) and `Classical.choice` (two) are different names.
#[derive(Debug, Default)]
pub(crate) struct Names {
    /// Prefix and last component of name `id`, stored at `id - 1`.
    entries: Vec<(NameId, Component)>,
    ids: HashMap<(NameId, Component), NameId>,
}

impl Names {
    /// Number of names, the anonymous name included.
    pub(crate) fn count(&self) -> usize {
        self.entries.len() + 1
    }

    /// Prefix and last component of `id`, or `None` for the anonymous name.
    pub(crate) fn get(&self, id: NameId) -> Option<(NameId, &Component)> {
        let index = (id as usize).checked_sub(1)?;
        self.entries.get(index).map(|(pre, last)| (*pre, last))
    }

    /// Names in index order, after the anonymous name.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (NameId, &Component)> {
        self.entries.iter().map(|(pre, last)| (*pre, last))
    }

    /// Find a name whose string components are separated by the dots of
    /// `dotted`, so no component may contain a dot. For the toolchain's
    /// constants, whose components are plain identifiers.
    pub(crate) fn find(&self, dotted: &str) -> Option<NameId> {
        let mut id = ANONYMOUS;
        for part in dotted.split('.') {
            id = *self.ids.get(&(id, Component::Str(part.into())))?;
        }
        Some(id)
    }

    /// The name with these components, outermost first, if it exists.
    pub(crate) fn lookup(&self, components: &[Component]) -> Option<NameId> {
        let mut id = ANONYMOUS;
        for component in components {
            id = *self.ids.get(&(id, component.clone()))?;
        }
        Some(id)
    }

    /// The components of `id`, outermost first.
    pub(crate) fn components(&self, id: NameId) -> Vec<Component> {
        self.parts(id).into_iter().cloned().collect()
    }

    fn parts(&self, id: NameId) -> Vec<&Component> {
        let mut parts = Vec::new();
        let mut current = id;
        while let Some((pre, last)) = self.get(current) {
            parts.push(last);
            current = pre;
        }
        parts.reverse();
        parts
    }

    /// The name as Lean's `Name.toString` prints it: components joined with
    /// dots, and a string component that is not an identifier in `«»`.
    pub(crate) fn display(&self, id: NameId) -> String {
        let parts = self.parts(id);
        if parts.is_empty() {
            return "[anonymous]".to_owned();
        }
        let escape = !prints_raw(&parts);
        let shown: Vec<Cow<'_, str>> = parts
            .iter()
            .map(|part| match part {
                Component::Str(text) if escape => escape_part(text),
                Component::Str(text) => Cow::Borrowed(&**text),
                Component::Num(number) => Cow::Owned(number.to_string()),
            })
            .collect();
        shown.join(".")
    }

    /// Components joined with dots and never escaped, the way nanoda's
    /// `name_to_string` spells the axioms its configuration permits.
    pub(crate) fn dotted(&self, id: NameId) -> String {
        let shown: Vec<String> = self
            .parts(id)
            .iter()
            .map(|part| match part {
                Component::Str(text) => text.to_string(),
                Component::Num(number) => number.to_string(),
            })
            .collect();
        shown.join(".")
    }

    fn intern(&mut self, pre: NameId, last: Component) -> Result<NameId> {
        let key = (pre, last);
        if let Some(id) = self.ids.get(&key) {
            return Ok(*id);
        }
        let id = NameId::try_from(self.entries.len() + 1).context("too many names")?;
        self.entries.push(key.clone());
        self.ids.insert(key, id);
        Ok(id)
    }
}

/// Whether Lean prints a name without escapes: inaccessible names, names
/// with macro scopes, and the delaborator's pseudo-syntax (`_`, `#0`, `?u`).
fn prints_raw(parts: &[&Component]) -> bool {
    let last = parts.iter().rev().find_map(|part| match part {
        Component::Str(text) => Some(&**text),
        Component::Num(_) => None,
    });
    let root = match parts.first() {
        Some(Component::Str(text)) => Some(&**text),
        _ => None,
    };
    last.is_some_and(|text| text.contains('✝') || text == "_inaccessible" || text == "_hyg")
        || (parts.len() == 1 && root == Some("_"))
        || root.is_some_and(|text| text.starts_with(['#', '?']))
}

/// Lean's `Name.escapePart`: an identifier as it is, anything else in `«»`,
/// except a component with `»`, which cannot be escaped.
fn escape_part(text: &str) -> Cow<'_, str> {
    let mut chars = text.chars();
    let identifier = chars.next().is_some_and(is_id_first) && chars.all(is_id_rest);
    if identifier || text.contains('»') {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("«{text}»"))
    }
}

/// Lean's `isIdFirst`.
fn is_id_first(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || is_letter_like(c)
}

/// Lean's `isIdRest`.
fn is_id_rest(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '_' | '\'' | '!' | '?')
        || is_letter_like(c)
        || is_subscript_alnum(c)
}

/// Lean's `isLetterLike`: Greek and Coptic letters except λ, Π and Σ,
/// letter-like symbols, mathematical alphanumerics, and the Latin-1 and
/// Latin Extended-A letters.
fn is_letter_like(c: char) -> bool {
    let c = u32::from(c);
    ((0x3b1..=0x3c9).contains(&c) && c != 0x3bb)
        || ((0x391..=0x3a9).contains(&c) && c != 0x3a0 && c != 0x3a3)
        || (0x3ca..=0x3fb).contains(&c)
        || (0x1f00..=0x1ffe).contains(&c)
        || (0x2100..=0x214f).contains(&c)
        || (0x1d49c..=0x1d59f).contains(&c)
        || ((0xc0..=0xff).contains(&c) && c != 0xd7 && c != 0xf7)
        || (0x100..=0x17f).contains(&c)
}

/// Lean's `isSubScriptAlnum`.
fn is_subscript_alnum(c: char) -> bool {
    let c = u32::from(c);
    (0x2080..=0x2089).contains(&c)
        || (0x2090..=0x209c).contains(&c)
        || (0x1d62..=0x1d6a).contains(&c)
        || c == 0x2c7c
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Zero,
    Succ(u32),
    Max(u32, u32),
    IMax(u32, u32),
    Param(NameId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinderInfo {
    Default,
    Implicit,
    StrictImplicit,
    InstImplicit,
}

impl BinderInfo {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Implicit => "implicit",
            Self::StrictImplicit => "strictImplicit",
            Self::InstImplicit => "instImplicit",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Binder {
    pub(crate) name: NameId,
    pub(crate) info: BinderInfo,
    pub(crate) ty: u32,
    pub(crate) body: u32,
}

/// An expression node; children are indices into the same module arena.
#[derive(Debug, Clone)]
pub(crate) enum Expr {
    BVar(u64),
    Sort(u32),
    Const(NameId, Box<[u32]>),
    App(u32, u32),
    Lam(Binder),
    Pi(Binder),
    /// The nondep flag is dropped: kernels ignore it and LeanExport writes false.
    Let {
        name: NameId,
        ty: u32,
        value: u32,
        body: u32,
    },
    Proj(NameId, u64, u32),
    /// Decimal digits without leading zeros.
    Nat(Box<str>),
    Str(Box<str>),
    MData(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hints {
    Opaque,
    Abbrev,
    Regular(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Safety {
    Safe,
    Unsafe,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum QuotKind {
    Type,
    Ctor,
    Lift,
    Ind,
}

#[derive(Debug, Clone)]
pub(crate) struct Header {
    pub(crate) name: NameId,
    pub(crate) level_params: Vec<NameId>,
    pub(crate) ty: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct Def {
    pub(crate) header: Header,
    pub(crate) value: u32,
    pub(crate) hints: Hints,
    pub(crate) safety: Safety,
    pub(crate) all: Vec<NameId>,
}

#[derive(Debug, Clone)]
pub(crate) struct Thm {
    pub(crate) header: Header,
    pub(crate) value: u32,
    pub(crate) all: Vec<NameId>,
}

#[derive(Debug, Clone)]
pub(crate) struct Opaque {
    pub(crate) header: Header,
    pub(crate) value: u32,
    pub(crate) is_unsafe: bool,
    pub(crate) all: Vec<NameId>,
}

#[derive(Debug, Clone)]
pub(crate) struct QuotVal {
    pub(crate) header: Header,
    pub(crate) kind: QuotKind,
}

#[derive(Debug, Clone)]
pub(crate) struct IndType {
    pub(crate) header: Header,
    pub(crate) num_params: u64,
    pub(crate) num_indices: u64,
    pub(crate) all: Vec<NameId>,
    pub(crate) ctors: Vec<NameId>,
    pub(crate) num_nested: u64,
    pub(crate) is_rec: bool,
    pub(crate) is_reflexive: bool,
    pub(crate) is_unsafe: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Ctor {
    pub(crate) header: Header,
    pub(crate) induct: NameId,
    pub(crate) cidx: u64,
    pub(crate) num_params: u64,
    pub(crate) num_fields: u64,
    pub(crate) is_unsafe: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub(crate) ctor: NameId,
    pub(crate) nfields: u64,
    pub(crate) rhs: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct Rec {
    pub(crate) header: Header,
    pub(crate) all: Vec<NameId>,
    pub(crate) num_params: u64,
    pub(crate) num_indices: u64,
    pub(crate) num_motives: u64,
    pub(crate) num_minors: u64,
    pub(crate) rules: Vec<Rule>,
    pub(crate) k: bool,
    pub(crate) is_unsafe: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Inductive {
    pub(crate) types: Vec<IndType>,
    pub(crate) ctors: Vec<Ctor>,
    pub(crate) recs: Vec<Rec>,
}

/// One declaration unit: a single constant, a whole inductive block, or the
/// quotient package. Units are what slices include and hash.
#[derive(Debug, Clone)]
pub(crate) enum Decl {
    Axiom { header: Header, is_unsafe: bool },
    Def(Def),
    Thm(Thm),
    Opaque(Opaque),
    Quot(Vec<QuotVal>),
    Inductive(Inductive),
}

/// Kind of one constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    Axiom,
    Def,
    Thm,
    Opaque,
    Quot,
    Inductive,
    Ctor,
    Rec,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Axiom => "axiom",
            Self::Def => "def",
            Self::Thm => "thm",
            Self::Opaque => "opaque",
            Self::Quot => "quot",
            Self::Inductive => "inductive",
            Self::Ctor => "ctor",
            Self::Rec => "rec",
        }
    }
}

impl Decl {
    /// Every constant the unit declares, with its kind.
    pub(crate) fn members(&self) -> Vec<(NameId, Kind)> {
        match self {
            Self::Axiom { header, .. } => vec![(header.name, Kind::Axiom)],
            Self::Def(def) => vec![(def.header.name, Kind::Def)],
            Self::Thm(thm) => vec![(thm.header.name, Kind::Thm)],
            Self::Opaque(opaque) => vec![(opaque.header.name, Kind::Opaque)],
            Self::Quot(values) => values
                .iter()
                .map(|value| (value.header.name, Kind::Quot))
                .collect(),
            Self::Inductive(block) => block
                .types
                .iter()
                .map(|ty| (ty.header.name, Kind::Inductive))
                .chain(block.ctors.iter().map(|c| (c.header.name, Kind::Ctor)))
                .chain(block.recs.iter().map(|r| (r.header.name, Kind::Rec)))
                .collect(),
        }
    }

    /// Header and value of a unit that declares exactly one constant.
    pub(crate) fn single(&self) -> Option<(&Header, Option<u32>)> {
        match self {
            Self::Axiom { header, .. } => Some((header, None)),
            Self::Def(def) => Some((&def.header, Some(def.value))),
            Self::Thm(thm) => Some((&thm.header, Some(thm.value))),
            Self::Opaque(opaque) => Some((&opaque.header, Some(opaque.value))),
            Self::Quot(_) | Self::Inductive(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Import {
    pub(crate) module: String,
    pub(crate) all: bool,
}

/// One module's own constants, read from one file.
#[derive(Debug)]
pub(crate) struct Module {
    pub(crate) name: String,
    pub(crate) imports: Vec<Import>,
    pub(crate) is_module: bool,
    /// The LeanExport metadata object of the file (the value of `meta`).
    pub(crate) meta: Value,
    /// `lev.exporter` of the first line; `None` for plain LeanExport files.
    pub(crate) exporter: Option<String>,
    /// Level arena; index 0 is the zero level.
    pub(crate) levels: Vec<Level>,
    pub(crate) exprs: Vec<Expr>,
    pub(crate) units: Vec<Decl>,
    /// Own constants whose exported view has no value.
    pub(crate) hidden: Vec<NameId>,
}

/// Where a constant is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owner {
    pub(crate) module: u32,
    pub(crate) unit: u32,
    pub(crate) kind: Kind,
}

/// Every module of an export, with the global constant table.
#[derive(Debug)]
pub(crate) struct Development {
    pub(crate) lean_version: String,
    pub(crate) githash: String,
    pub(crate) names: Names,
    pub(crate) modules: Vec<Module>,
    /// The earliest owner of every constant, in module order.
    pub(crate) owners: HashMap<NameId, Owner>,
    /// Later owners of constants declared by more than one module.
    pub(crate) duplicates: HashMap<NameId, Vec<Owner>>,
    /// Constants hidden in their owner's exported view.
    pub(crate) hidden: HashSet<NameId>,
    module_ids: HashMap<String, u32>,
}

impl Development {
    fn new(
        lean_version: String,
        githash: String,
        names: Names,
        modules: Vec<Module>,
    ) -> Result<Self> {
        let mut owners: HashMap<NameId, Owner> = HashMap::new();
        let mut duplicates: HashMap<NameId, Vec<Owner>> = HashMap::new();
        let mut hidden = HashSet::new();
        let mut module_ids = HashMap::new();
        for (module_index, module) in modules.iter().enumerate() {
            let module_id = u32::try_from(module_index).context("too many modules")?;
            module_ids.insert(module.name.clone(), module_id);
            let mut own = HashSet::new();
            for (unit_index, unit) in module.units.iter().enumerate() {
                let unit_id = u32::try_from(unit_index).context("too many declarations")?;
                for (name, kind) in unit.members() {
                    if !own.insert(name) {
                        bail!(
                            "module {} declares {} twice",
                            module.name,
                            names.display(name)
                        );
                    }
                    let owner = Owner {
                        module: module_id,
                        unit: unit_id,
                        kind,
                    };
                    match owners.entry(name) {
                        Entry::Occupied(_) => duplicates.entry(name).or_default().push(owner),
                        Entry::Vacant(slot) => {
                            slot.insert(owner);
                        }
                    }
                }
            }
            for name in &module.hidden {
                if !own.contains(name) {
                    bail!(
                        "module {} hides {}, which it does not declare",
                        module.name,
                        names.display(*name)
                    );
                }
                hidden.insert(*name);
            }
        }
        Ok(Self {
            lean_version,
            githash,
            names,
            modules,
            owners,
            duplicates,
            hidden,
            module_ids,
        })
    }

    pub(crate) fn module_index(&self, name: &str) -> Option<u32> {
        self.module_ids.get(name).copied()
    }

    /// The declaration of `name` in `module`, if `module` declares it.
    pub(crate) fn owner_in(&self, module: u32, name: NameId) -> Option<Owner> {
        let owner = self.owners.get(&name)?;
        if owner.module == module {
            return Some(*owner);
        }
        self.duplicates
            .get(&name)?
            .iter()
            .find(|owner| owner.module == module)
            .copied()
    }

    /// Where `module` takes `name` from: its own declaration if it has one,
    /// otherwise the earliest owner.
    pub(crate) fn owner_for(&self, module: u32, name: NameId) -> Option<Owner> {
        self.owner_in(module, name)
            .or_else(|| self.owners.get(&name).copied())
    }
}

#[derive(Debug, Deserialize)]
struct Manifest {
    format: String,
    lean: ManifestLean,
    modules: Vec<ManifestModule>,
}

#[derive(Debug, Deserialize)]
struct ManifestLean {
    version: String,
    githash: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestModule {
    name: String,
    file: String,
    #[serde(default)]
    imports: Vec<ManifestImport>,
    #[serde(default)]
    is_module: bool,
}

#[derive(Debug, Deserialize)]
struct ManifestImport {
    module: String,
    #[serde(default)]
    all: bool,
}

/// Read a `lev-export/1` directory, parsing module files in parallel.
pub(crate) fn read_export(dir: &Path, jobs: usize) -> Result<Development> {
    let manifest_path = dir.join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    if manifest.format != EXPORT_FORMAT {
        bail!(
            "{} has format {:?}, expected {EXPORT_FORMAT:?}",
            manifest_path.display(),
            manifest.format
        );
    }
    let mut seen = HashSet::new();
    for module in &manifest.modules {
        if !seen.insert(module.name.as_str()) {
            bail!("the manifest lists module {} twice", module.name);
        }
    }

    let names = RwLock::new(Names::default());
    let parsed = parallel_map(&manifest.modules, jobs, |_, entry| {
        let path = module_path(dir, &entry.file)?;
        parse_file(&path, Some(&entry.name), &names)
            .with_context(|| format!("failed to read module {}", entry.name))
    });
    let mut modules = Vec::with_capacity(parsed.len());
    for (entry, module) in manifest.modules.iter().zip(parsed) {
        let mut module = module?;
        module.name = entry.name.clone();
        module.imports = entry
            .imports
            .iter()
            .map(|import| Import {
                module: import.module.clone(),
                all: import.all,
            })
            .collect();
        module.is_module = entry.is_module;
        modules.push(module);
    }
    let names = names.into_inner().unwrap_or_else(PoisonError::into_inner);
    Development::new(manifest.lean.version, manifest.lean.githash, names, modules)
}

/// The Lean version a `lev-export/1` manifest names.
pub(crate) fn export_lean_version(dir: &Path) -> Result<String> {
    let path = dir.join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(manifest.lean.version)
}

/// Read one plain LeanExport 3.1.0 file as a single-module development.
pub(crate) fn read_plain(path: &Path) -> Result<Development> {
    let names = RwLock::new(Names::default());
    let mut module = parse_file(path, None, &names)
        .with_context(|| format!("failed to read {}", path.display()))?;
    module.name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_owned());
    let lean = module.meta.get("lean");
    let field = |key: &str| {
        lean.and_then(|lean| lean.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("{} has no lean {key} in its metadata", path.display()))
    };
    let version = field("version")?;
    let githash = field("githash")?;
    let names = names.into_inner().unwrap_or_else(PoisonError::into_inner);
    Development::new(version, githash, names, vec![module])
}

/// Read a reference development: a `lev-export/1` directory or a plain file.
pub(crate) fn read_reference(path: &Path, jobs: usize) -> Result<Development> {
    if path.is_dir() {
        read_export(path, jobs)
    } else {
        read_plain(path)
    }
}

/// SHA-256 over a reference's bytes: the file, or the manifest and every
/// module file of a `lev-export/1` directory.
pub(crate) fn reference_digest(path: &Path) -> Result<String> {
    use crate::cache::file_digest as sha256;
    if !path.is_dir() {
        return sha256(path);
    }
    let manifest_path = path.join("manifest.json");
    let mut node = super::hash::Node::new("lev-reference/1");
    node.text(&sha256(&manifest_path)?);
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?,
    )?;
    for module in &manifest.modules {
        node.text(&module.file)
            .text(&sha256(&module_path(path, &module.file)?)?);
    }
    Ok(crate::core::hex::lowercase(&node.finish()))
}

fn module_path(dir: &Path, file: &str) -> Result<PathBuf> {
    let relative = Path::new(file);
    let plain = relative
        .components()
        .all(|component| matches!(component, PathComponent::Normal(_)));
    if file.is_empty() || !plain {
        bail!("module file {file:?} is not a plain relative path");
    }
    Ok(dir.join(relative))
}

fn parse_file(path: &Path, module: Option<&str>, names: &RwLock<Names>) -> Result<Module> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut buffer = String::new();
    let mut parser = Parser::default();
    let mut line_number = 0_u64;
    let mut header = None;
    let mut trailer = None;
    loop {
        buffer.clear();
        let read = reader
            .read_line(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        line_number += 1;
        let text = buffer.strip_suffix('\n').unwrap_or(&buffer);
        let result = (|| {
            if trailer.is_some() {
                bail!("record after the hidden list");
            }
            if text.is_empty() {
                bail!("empty line");
            }
            let line: Line = serde_json::from_str(text)?;
            if header.is_none() {
                header = Some(parse_header(line, module)?);
                return Ok(());
            }
            if let Some(hidden) = parser.record(line)? {
                if module.is_none() {
                    bail!("plain LeanExport files have no hidden list");
                }
                trailer = Some(hidden);
            }
            Ok(())
        })();
        result.with_context(|| format!("{}:{line_number}", path.display()))?;
    }
    let Some((meta, exporter)) = header else {
        bail!("{} is empty", path.display());
    };
    if !parser.quot.is_empty() {
        bail!("{} has an incomplete quotient package", path.display());
    }
    let hidden = match (module, trailer) {
        (Some(_), None) => bail!("{} has no hidden list as its last line", path.display()),
        (_, hidden) => hidden.unwrap_or_default(),
    };
    parser.finish(meta, exporter, hidden, names)
}

fn parse_header(line: Line<'_>, module: Option<&str>) -> Result<(Value, Option<String>)> {
    line.check_shape()?;
    let Line {
        meta: Some(meta),
        lev,
        ..
    } = line
    else {
        bail!("the first line is not a LeanExport metadata record");
    };
    let version = meta
        .get("format")
        .and_then(|format| format.get("version"))
        .and_then(Value::as_str);
    if version != Some(LEAN_EXPORT_VERSION) {
        bail!("export format version {version:?}, expected {LEAN_EXPORT_VERSION:?}");
    }
    let exporter = match (module, lev) {
        (Some(expected), Some(lev)) => {
            if lev.hidden.is_some() || lev.module.as_deref() != Some(expected) {
                bail!(
                    "the first line names module {:?}, expected {expected:?}",
                    lev.module
                );
            }
            Some(
                lev.exporter
                    .context("the first line has no lev exporter")?
                    .into_owned(),
            )
        }
        (Some(_), None) => bail!("the first line has no lev header"),
        (None, Some(_)) => bail!("plain LeanExport files have no lev header"),
        (None, None) => None,
    };
    Ok((meta, exporter))
}

/// File-local tables while one file is read; names are interned at the end.
#[derive(Default)]
struct Parser {
    /// Local name `i` is `names[i - 1]`, with a local prefix index.
    names: Vec<(u32, Component)>,
    levels: Vec<Level>,
    exprs: Vec<Expr>,
    units: Vec<Decl>,
    quot: Vec<QuotVal>,
}

impl Parser {
    fn name(&self, index: u64) -> Result<u32> {
        if index > self.names.len() as u64 {
            bail!("name {index} is not defined");
        }
        Ok(u32::try_from(index)?)
    }

    fn names(&self, indices: &[u64]) -> Result<Vec<u32>> {
        indices.iter().map(|index| self.name(*index)).collect()
    }

    fn level(&self, index: u64) -> Result<u32> {
        if index > self.levels.len() as u64 {
            bail!("level {index} is not defined");
        }
        Ok(u32::try_from(index)?)
    }

    fn expr(&self, index: u64) -> Result<u32> {
        if index >= self.exprs.len() as u64 {
            bail!("expression {index} is not defined");
        }
        Ok(u32::try_from(index)?)
    }

    fn header(&self, name: u64, level_params: &[u64], ty: u64) -> Result<Header> {
        Ok(Header {
            name: self.name(name)?,
            level_params: self.names(level_params)?,
            ty: self.expr(ty)?,
        })
    }

    /// Add one record; returns the hidden list if this was the lev trailer.
    fn record(&mut self, line: Line<'_>) -> Result<Option<Vec<u32>>> {
        line.check_shape()?;
        if let Some(index) = line.name_index {
            let name = match (line.str, line.num) {
                (Some(record), None) => (self.name(record.pre)?, Component::Str(record.str.into())),
                (None, Some(record)) => (self.name(record.pre)?, Component::Num(record.i)),
                _ => bail!("a name record needs str or num"),
            };
            if index != self.names.len() as u64 + 1 {
                bail!("name index {index} is not the next index");
            }
            self.names.push(name);
            return Ok(None);
        }
        if let Some(index) = line.il {
            let level = if let Some(l) = line.succ {
                Level::Succ(self.level(l)?)
            } else if let Some([a, b]) = line.max {
                Level::Max(self.level(a)?, self.level(b)?)
            } else if let Some([a, b]) = line.imax {
                Level::IMax(self.level(a)?, self.level(b)?)
            } else if let Some(name) = line.param {
                Level::Param(self.name(name)?)
            } else {
                bail!("a level record needs succ, max, imax or param");
            };
            if index != self.levels.len() as u64 + 1 {
                bail!("level index {index} is not the next index");
            }
            self.levels.push(level);
            return Ok(None);
        }
        if let Some(index) = line.ie {
            let expr = self.expr_record(line)?;
            if index != self.exprs.len() as u64 {
                bail!("expression index {index} is not the next index");
            }
            self.exprs.push(expr);
            return Ok(None);
        }
        if line.meta.is_some() {
            bail!("a second metadata record");
        }
        if let Some(lev) = line.lev {
            if lev.exporter.is_some() || lev.module.is_some() {
                bail!("a lev header after the first line");
            }
            let hidden = lev.hidden.context("a lev record without a hidden list")?;
            return self.names(&hidden).map(Some);
        }
        if let Some(record) = line.quot {
            if self.quot.iter().any(|q| q.kind == record.kind.into()) {
                bail!("the quotient package declares {:?} twice", record.kind);
            }
            self.quot.push(QuotVal {
                header: self.header(record.name, &record.level_params, record.ty)?,
                kind: record.kind.into(),
            });
            if self.quot.len() == 4 {
                let mut package = std::mem::take(&mut self.quot);
                package.sort_by_key(|value| value.kind);
                self.units.push(Decl::Quot(package));
            }
            return Ok(None);
        }
        if !self.quot.is_empty() {
            bail!("a declaration inside the quotient package");
        }
        let decl = self.decl_record(line)?;
        self.units.push(decl);
        Ok(None)
    }

    fn expr_record(&self, line: Line<'_>) -> Result<Expr> {
        Ok(if let Some(index) = line.bvar {
            Expr::BVar(index)
        } else if let Some(level) = line.sort {
            Expr::Sort(self.level(level)?)
        } else if let Some(record) = line.constant {
            let levels = record
                .us
                .iter()
                .map(|level| self.level(*level))
                .collect::<Result<Vec<_>>>()?;
            Expr::Const(self.name(record.name)?, levels.into())
        } else if let Some(record) = line.app {
            Expr::App(self.expr(record.r#fn)?, self.expr(record.arg)?)
        } else if let Some(record) = line.lam {
            Expr::Lam(self.binder(&record)?)
        } else if let Some(record) = line.forall {
            Expr::Pi(self.binder(&record)?)
        } else if let Some(record) = line.let_in {
            Expr::Let {
                name: self.name(record.name)?,
                ty: self.expr(record.ty)?,
                value: self.expr(record.value)?,
                body: self.expr(record.body)?,
            }
        } else if let Some(record) = line.proj {
            Expr::Proj(
                self.name(record.type_name)?,
                record.idx,
                self.expr(record.r#struct)?,
            )
        } else if let Some(digits) = line.nat {
            Expr::Nat(canonical_nat(&digits)?.into())
        } else if let Some(text) = line.string {
            Expr::Str(text.into())
        } else if let Some(record) = line.mdata {
            Expr::MData(self.expr(record.expr)?)
        } else {
            bail!("an expression record without an expression");
        })
    }

    fn binder(&self, record: &BinderRecord) -> Result<Binder> {
        Ok(Binder {
            name: self.name(record.name)?,
            info: record.binder_info.into(),
            ty: self.expr(record.ty)?,
            body: self.expr(record.body)?,
        })
    }

    fn decl_record(&self, line: Line<'_>) -> Result<Decl> {
        Ok(if let Some(r) = line.axiom {
            Decl::Axiom {
                header: self.header(r.name, &r.level_params, r.ty)?,
                is_unsafe: r.is_unsafe,
            }
        } else if let Some(r) = line.def {
            Decl::Def(Def {
                header: self.header(r.name, &r.level_params, r.ty)?,
                value: self.expr(r.value)?,
                hints: r.hints.into(),
                safety: r.safety.into(),
                all: self.names(&r.all)?,
            })
        } else if let Some(r) = line.thm {
            Decl::Thm(Thm {
                header: self.header(r.name, &r.level_params, r.ty)?,
                value: self.expr(r.value)?,
                all: self.names(&r.all)?,
            })
        } else if let Some(r) = line.opaque {
            Decl::Opaque(Opaque {
                header: self.header(r.name, &r.level_params, r.ty)?,
                value: self.expr(r.value)?,
                is_unsafe: r.is_unsafe,
                all: self.names(&r.all)?,
            })
        } else if let Some(r) = line.inductive {
            Decl::Inductive(self.inductive(r)?)
        } else {
            bail!("unknown record");
        })
    }

    fn inductive(&self, record: InductiveRecord) -> Result<Inductive> {
        if record.types.is_empty() {
            bail!("an inductive block without types");
        }
        let types = record
            .types
            .iter()
            .map(|t| {
                Ok(IndType {
                    header: self.header(t.name, &t.level_params, t.ty)?,
                    num_params: t.num_params,
                    num_indices: t.num_indices,
                    all: self.names(&t.all)?,
                    ctors: self.names(&t.ctors)?,
                    num_nested: t.num_nested,
                    is_rec: t.is_rec,
                    is_reflexive: t.is_reflexive,
                    is_unsafe: t.is_unsafe,
                })
            })
            .collect::<Result<_>>()?;
        let ctors = record
            .ctors
            .iter()
            .map(|c| {
                Ok(Ctor {
                    header: self.header(c.name, &c.level_params, c.ty)?,
                    induct: self.name(c.induct)?,
                    cidx: c.cidx,
                    num_params: c.num_params,
                    num_fields: c.num_fields,
                    is_unsafe: c.is_unsafe,
                })
            })
            .collect::<Result<_>>()?;
        let recs = record
            .recs
            .iter()
            .map(|r| {
                Ok(Rec {
                    header: self.header(r.name, &r.level_params, r.ty)?,
                    all: self.names(&r.all)?,
                    num_params: r.num_params,
                    num_indices: r.num_indices,
                    num_motives: r.num_motives,
                    num_minors: r.num_minors,
                    rules: r
                        .rules
                        .iter()
                        .map(|rule| {
                            Ok(Rule {
                                ctor: self.name(rule.ctor)?,
                                nfields: rule.nfields,
                                rhs: self.expr(rule.rhs)?,
                            })
                        })
                        .collect::<Result<_>>()?,
                    k: r.k,
                    is_unsafe: r.is_unsafe,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Inductive { types, ctors, recs })
    }

    /// Intern the file's names and rewrite every local name index as global.
    fn finish(
        self,
        meta: Value,
        exporter: Option<String>,
        hidden: Vec<u32>,
        names: &RwLock<Names>,
    ) -> Result<Module> {
        let map = intern(names, self.names)?;
        let name = |local: u32| map[local as usize];
        let header = |h: Header| Header {
            name: name(h.name),
            level_params: h.level_params.into_iter().map(name).collect(),
            ty: h.ty,
        };
        let list = |list: Vec<u32>| list.into_iter().map(name).collect::<Vec<_>>();
        let levels = std::iter::once(Level::Zero)
            .chain(self.levels.into_iter().map(|level| match level {
                Level::Param(n) => Level::Param(name(n)),
                other => other,
            }))
            .collect();
        let exprs = self
            .exprs
            .into_iter()
            .map(|expr| match expr {
                Expr::Const(n, us) => Expr::Const(name(n), us),
                Expr::Lam(b) => Expr::Lam(Binder {
                    name: name(b.name),
                    ..b
                }),
                Expr::Pi(b) => Expr::Pi(Binder {
                    name: name(b.name),
                    ..b
                }),
                Expr::Let {
                    name: n,
                    ty,
                    value,
                    body,
                } => Expr::Let {
                    name: name(n),
                    ty,
                    value,
                    body,
                },
                Expr::Proj(n, idx, s) => Expr::Proj(name(n), idx, s),
                other => other,
            })
            .collect();
        let units = self
            .units
            .into_iter()
            .map(|unit| match unit {
                Decl::Axiom {
                    header: h,
                    is_unsafe,
                } => Decl::Axiom {
                    header: header(h),
                    is_unsafe,
                },
                Decl::Def(d) => Decl::Def(Def {
                    header: header(d.header),
                    all: list(d.all),
                    ..d
                }),
                Decl::Thm(t) => Decl::Thm(Thm {
                    header: header(t.header),
                    all: list(t.all),
                    ..t
                }),
                Decl::Opaque(o) => Decl::Opaque(Opaque {
                    header: header(o.header),
                    all: list(o.all),
                    ..o
                }),
                Decl::Quot(values) => Decl::Quot(
                    values
                        .into_iter()
                        .map(|v| QuotVal {
                            header: header(v.header),
                            kind: v.kind,
                        })
                        .collect(),
                ),
                Decl::Inductive(block) => Decl::Inductive(Inductive {
                    types: block
                        .types
                        .into_iter()
                        .map(|t| IndType {
                            header: header(t.header),
                            all: list(t.all),
                            ctors: list(t.ctors),
                            ..t
                        })
                        .collect(),
                    ctors: block
                        .ctors
                        .into_iter()
                        .map(|c| Ctor {
                            header: header(c.header),
                            induct: name(c.induct),
                            ..c
                        })
                        .collect(),
                    recs: block
                        .recs
                        .into_iter()
                        .map(|r| Rec {
                            header: header(r.header),
                            all: list(r.all),
                            rules: r
                                .rules
                                .into_iter()
                                .map(|rule| Rule {
                                    ctor: name(rule.ctor),
                                    ..rule
                                })
                                .collect(),
                            ..r
                        })
                        .collect(),
                }),
            })
            .collect::<Vec<_>>();
        for unit in &units {
            match unit {
                Decl::Def(def) if def.safety != Safety::Safe && exporter.is_some() => {
                    bail!("{} is not safe", names_display(names, def.header.name));
                }
                Decl::Axiom {
                    header,
                    is_unsafe: true,
                }
                | Decl::Opaque(Opaque {
                    header,
                    is_unsafe: true,
                    ..
                }) if exporter.is_some() => {
                    bail!("{} is unsafe", names_display(names, header.name));
                }
                Decl::Inductive(block)
                    if exporter.is_some()
                        && (block.types.iter().any(|t| t.is_unsafe)
                            || block.ctors.iter().any(|c| c.is_unsafe)
                            || block.recs.iter().any(|r| r.is_unsafe)) =>
                {
                    bail!("an unsafe inductive block");
                }
                _ => {}
            }
        }
        Ok(Module {
            name: String::new(),
            imports: Vec::new(),
            is_module: false,
            meta,
            exporter,
            levels,
            exprs,
            units,
            hidden: hidden.into_iter().map(name).collect(),
        })
    }
}

fn names_display(names: &RwLock<Names>, id: NameId) -> String {
    names
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .display(id)
}

/// Intern local names, returning the local-to-global map (index 0 is anonymous).
///
/// Most names already exist, so they are looked up under a shared lock first.
fn intern(names: &RwLock<Names>, mut local: Vec<(u32, Component)>) -> Result<Vec<NameId>> {
    const UNKNOWN: NameId = NameId::MAX;
    let mut map = vec![UNKNOWN; local.len() + 1];
    map[0] = ANONYMOUS;
    {
        let names = names.read().unwrap_or_else(PoisonError::into_inner);
        for (index, (pre, last)) in local.iter_mut().enumerate() {
            let pre = map[*pre as usize];
            if pre == UNKNOWN {
                continue;
            }
            let key = (pre, std::mem::replace(last, Component::Num(0)));
            if let Some(id) = names.ids.get(&key) {
                map[index + 1] = *id;
            }
            *last = key.1;
        }
    }
    if map.contains(&UNKNOWN) {
        let mut names = names.write().unwrap_or_else(PoisonError::into_inner);
        for (index, (pre, last)) in local.into_iter().enumerate() {
            if map[index + 1] == UNKNOWN {
                map[index + 1] = names.intern(map[pre as usize], last)?;
            }
        }
    }
    Ok(map)
}

fn canonical_nat(digits: &str) -> Result<&str> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("natVal {digits:?} is not a decimal natural number");
    }
    let trimmed = digits.trim_start_matches('0');
    Ok(if trimmed.is_empty() { "0" } else { trimmed })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Line<'a> {
    #[serde(rename = "in")]
    name_index: Option<u64>,
    il: Option<u64>,
    ie: Option<u64>,
    #[serde(borrow)]
    str: Option<NameStr<'a>>,
    num: Option<NameNum>,
    succ: Option<u64>,
    max: Option<[u64; 2]>,
    imax: Option<[u64; 2]>,
    param: Option<u64>,
    bvar: Option<u64>,
    sort: Option<u64>,
    #[serde(rename = "const")]
    constant: Option<ConstRecord>,
    app: Option<AppRecord>,
    lam: Option<BinderRecord>,
    #[serde(rename = "forallE")]
    forall: Option<BinderRecord>,
    #[serde(rename = "letE")]
    let_in: Option<LetRecord>,
    proj: Option<ProjRecord>,
    #[serde(rename = "natVal", borrow)]
    nat: Option<Cow<'a, str>>,
    #[serde(rename = "strVal", borrow)]
    string: Option<Cow<'a, str>>,
    mdata: Option<MDataRecord>,
    axiom: Option<AxiomRecord>,
    def: Option<DefRecord>,
    thm: Option<ThmRecord>,
    opaque: Option<OpaqueRecord>,
    quot: Option<QuotRecord>,
    inductive: Option<InductiveRecord>,
    meta: Option<Value>,
    #[serde(borrow)]
    lev: Option<LevRecord<'a>>,
}

impl Line<'_> {
    /// One payload, and at most one `in`/`il`/`ie` index.
    fn check_shape(&self) -> Result<()> {
        let payloads = self.payloads();
        if payloads != 1 {
            bail!("expected exactly one record payload, found {payloads}");
        }
        let indices = [self.name_index, self.il, self.ie]
            .into_iter()
            .filter(Option::is_some)
            .count();
        if indices > 1 {
            bail!("a record with {indices} index keys");
        }
        Ok(())
    }

    /// Number of payload keys: everything except the `in`/`il`/`ie` index.
    /// The first line's `meta` and `lev` count as one.
    fn payloads(&self) -> usize {
        [
            self.str.is_some(),
            self.num.is_some(),
            self.succ.is_some(),
            self.max.is_some(),
            self.imax.is_some(),
            self.param.is_some(),
            self.bvar.is_some(),
            self.sort.is_some(),
            self.constant.is_some(),
            self.app.is_some(),
            self.lam.is_some(),
            self.forall.is_some(),
            self.let_in.is_some(),
            self.proj.is_some(),
            self.nat.is_some(),
            self.string.is_some(),
            self.mdata.is_some(),
            self.axiom.is_some(),
            self.def.is_some(),
            self.thm.is_some(),
            self.opaque.is_some(),
            self.quot.is_some(),
            self.inductive.is_some(),
            self.meta.is_some(),
            self.lev.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
            - usize::from(self.meta.is_some() && self.lev.is_some())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameStr<'a> {
    pre: u64,
    #[serde(borrow)]
    str: Cow<'a, str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameNum {
    pre: u64,
    i: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConstRecord {
    name: u64,
    us: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppRecord {
    r#fn: u64,
    arg: u64,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
enum BinderInfoRecord {
    Default,
    Implicit,
    StrictImplicit,
    InstImplicit,
}

impl From<BinderInfoRecord> for BinderInfo {
    fn from(info: BinderInfoRecord) -> Self {
        match info {
            BinderInfoRecord::Default => Self::Default,
            BinderInfoRecord::Implicit => Self::Implicit,
            BinderInfoRecord::StrictImplicit => Self::StrictImplicit,
            BinderInfoRecord::InstImplicit => Self::InstImplicit,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BinderRecord {
    name: u64,
    #[serde(rename = "type")]
    ty: u64,
    body: u64,
    binder_info: BinderInfoRecord,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LetRecord {
    name: u64,
    #[serde(rename = "type")]
    ty: u64,
    value: u64,
    body: u64,
    #[allow(dead_code)]
    nondep: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProjRecord {
    type_name: u64,
    idx: u64,
    r#struct: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MDataRecord {
    expr: u64,
    #[allow(dead_code)]
    data: serde_json::Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AxiomRecord {
    name: u64,
    level_params: Vec<u64>,
    #[serde(rename = "type")]
    ty: u64,
    is_unsafe: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum HintsRecord {
    Opaque,
    Abbrev,
    Regular(u32),
}

impl From<HintsRecord> for Hints {
    fn from(hints: HintsRecord) -> Self {
        match hints {
            HintsRecord::Opaque => Self::Opaque,
            HintsRecord::Abbrev => Self::Abbrev,
            HintsRecord::Regular(height) => Self::Regular(height),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum SafetyRecord {
    Safe,
    Unsafe,
    Partial,
}

impl From<SafetyRecord> for Safety {
    fn from(safety: SafetyRecord) -> Self {
        match safety {
            SafetyRecord::Safe => Self::Safe,
            SafetyRecord::Unsafe => Self::Unsafe,
            SafetyRecord::Partial => Self::Partial,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DefRecord {
    name: u64,
    level_params: Vec<u64>,
    #[serde(rename = "type")]
    ty: u64,
    value: u64,
    hints: HintsRecord,
    safety: SafetyRecord,
    all: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ThmRecord {
    name: u64,
    level_params: Vec<u64>,
    #[serde(rename = "type")]
    ty: u64,
    value: u64,
    all: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OpaqueRecord {
    name: u64,
    level_params: Vec<u64>,
    #[serde(rename = "type")]
    ty: u64,
    value: u64,
    /// Optional, as in `Parse.lean`.
    #[serde(default)]
    is_unsafe: bool,
    all: Vec<u64>,
}

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "camelCase")]
enum QuotKindRecord {
    Type,
    Ctor,
    Lift,
    Ind,
}

impl From<QuotKindRecord> for QuotKind {
    fn from(kind: QuotKindRecord) -> Self {
        match kind {
            QuotKindRecord::Type => Self::Type,
            QuotKindRecord::Ctor => Self::Ctor,
            QuotKindRecord::Lift => Self::Lift,
            QuotKindRecord::Ind => Self::Ind,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct QuotRecord {
    kind: QuotKindRecord,
    name: u64,
    level_params: Vec<u64>,
    #[serde(rename = "type")]
    ty: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InductiveRecord {
    types: Vec<TypeRecord>,
    ctors: Vec<CtorRecord>,
    recs: Vec<RecRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TypeRecord {
    all: Vec<u64>,
    ctors: Vec<u64>,
    is_rec: bool,
    is_reflexive: bool,
    is_unsafe: bool,
    level_params: Vec<u64>,
    name: u64,
    num_indices: u64,
    num_nested: u64,
    num_params: u64,
    #[serde(rename = "type")]
    ty: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CtorRecord {
    cidx: u64,
    induct: u64,
    is_unsafe: bool,
    level_params: Vec<u64>,
    name: u64,
    num_fields: u64,
    num_params: u64,
    #[serde(rename = "type")]
    ty: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RecRecord {
    all: Vec<u64>,
    is_unsafe: bool,
    k: bool,
    level_params: Vec<u64>,
    name: u64,
    num_indices: u64,
    num_minors: u64,
    num_motives: u64,
    num_params: u64,
    rules: Vec<RuleRecord>,
    #[serde(rename = "type")]
    ty: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleRecord {
    ctor: u64,
    nfields: u64,
    rhs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LevRecord<'a> {
    #[serde(borrow)]
    exporter: Option<Cow<'a, str>>,
    #[serde(borrow)]
    module: Option<Cow<'a, str>>,
    hidden: Option<Vec<u64>>,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::fixture::{self, File};
    use super::{
        Component, Decl, Expr, Hints, Kind, Level, QuotKind, escape_part, read_export, read_plain,
    };

    fn plain(text: &str) -> anyhow::Result<super::Development> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("plain.ndjson");
        fs::write(&path, text)?;
        read_plain(&path)
    }

    #[test]
    fn parses_every_record_kind() {
        let mut f = File::new();
        let seven = f.num_name("Foo", 7);
        let u = f.param("u");
        let v = f.param("v");
        let su = f.succ(u);
        let m = f.max(su, v);
        let im = f.imax(m, 0);
        let sort = f.sort(im);
        let b = f.bvar(3);
        let c = f.cnst("Foo.c", &[u, v]);
        let app = f.app(c, b);
        let lam = f.binder("lam", "x", "implicit", sort, app);
        let pi = f.binder("forallE", "y", "instImplicit", sort, lam);
        let nat = f.nat("0042");
        let text = f.string("h\u{e9} \"quoted\"\n");
        let let_in = f.let_in("z", sort, nat, b, true);
        let proj = f.proj("Foo.S", 1, let_in);
        let mdata = f.mdata(proj);
        f.axiom("Foo.ax", &["u"], sort);
        f.def("Foo.d", &["u", "v"], pi, mdata);
        f.thm("Foo.t", &[], sort, text);
        f.opaque("Foo.o", &[], sort, b);
        let (n_abbrev, n_opaque, n_bare) = (f.name("Foo.ab"), f.name("Foo.op"), f.name("Foo.bare"));
        f.raw(format!(r#"{{"def":{{"all":[],"hints":"abbrev","levelParams":[],"name":{n_abbrev},"safety":"safe","type":{sort},"value":{b}}}}}"#));
        f.raw(format!(r#"{{"def":{{"all":[],"hints":"opaque","levelParams":[],"name":{n_opaque},"safety":"partial","type":{sort},"value":{b}}}}}"#));
        // Parse.lean defaults a missing opaque isUnsafe to false.
        f.raw(format!(r#"{{"opaque":{{"all":[],"levelParams":[],"name":{n_bare},"type":{sort},"value":{b}}}}}"#));
        f.quot([
            ("Quot", sort, "type"),
            ("Quot.mk", sort, "ctor"),
            ("Quot.lift", sort, "lift"),
            ("Quot.ind", sort, "ind"),
        ]);
        fixture::nat(&mut f);

        let dev = plain(&f.plain()).expect("parse");
        let module = &dev.modules[0];
        // One file into a fresh interner: global name ids equal the file's.
        assert_eq!(dev.names.display(seven as u32), "Foo.7");
        let [sort, app, let_in, proj] = [sort, app, let_in, proj].map(|index| index as u32);
        assert_eq!(module.levels[0], Level::Zero);
        assert!(matches!(module.levels[im as usize], Level::IMax(_, 0)));
        assert!(matches!(module.levels[u as usize], Level::Param(_)));
        assert!(matches!(&module.exprs[nat as usize], Expr::Nat(d) if &**d == "42"));
        assert!(
            matches!(&module.exprs[text as usize], Expr::Str(s) if &**s == "h\u{e9} \"quoted\"\n")
        );
        assert!(matches!(&module.exprs[pi as usize], Expr::Pi(binder) if binder.ty == sort));
        assert!(matches!(&module.exprs[lam as usize], Expr::Lam(binder) if binder.body == app));
        assert!(matches!(&module.exprs[c as usize], Expr::Const(_, us) if us.len() == 2));
        assert!(matches!(&module.exprs[proj as usize], Expr::Proj(_, 1, s) if *s == let_in));
        assert!(matches!(&module.exprs[mdata as usize], Expr::MData(e) if *e == proj));

        let kinds: Vec<Kind> = module
            .units
            .iter()
            .flat_map(|unit| unit.members().into_iter().map(|(_, kind)| kind))
            .collect();
        assert_eq!(
            kinds,
            [
                Kind::Axiom,
                Kind::Def,
                Kind::Thm,
                Kind::Opaque,
                Kind::Def,
                Kind::Def,
                Kind::Opaque,
                Kind::Quot,
                Kind::Quot,
                Kind::Quot,
                Kind::Quot,
                Kind::Inductive,
                Kind::Ctor,
                Kind::Ctor,
                Kind::Rec,
            ]
        );
        assert!(matches!(&module.units[4], Decl::Def(d) if d.hints == Hints::Abbrev));
        assert!(
            matches!(&module.units[1], Decl::Def(d) if d.hints == Hints::Regular(1) && d.header.level_params.len() == 2)
        );
        assert!(matches!(&module.units[6], Decl::Opaque(o) if !o.is_unsafe));
        let Decl::Quot(package) = &module.units[7] else {
            panic!("quotient package")
        };
        assert_eq!(
            package.iter().map(|q| q.kind).collect::<Vec<_>>(),
            [
                QuotKind::Type,
                QuotKind::Ctor,
                QuotKind::Lift,
                QuotKind::Ind
            ]
        );
        let Decl::Inductive(block) = &module.units[8] else {
            panic!("inductive block")
        };
        assert_eq!(block.recs[0].rules.len(), 2);
        assert_eq!(dev.owners.len(), 15);
    }

    #[test]
    fn names_are_structured_and_print_the_way_lean_prints_them() {
        let mut f = File::new();
        let cases = [
            ("Classical.choice", "Classical.choice"),
            ("«Classical.choice»", "«Classical.choice»"),
            ("Foo.«7»", "Foo.«7»"),
            ("«a b».c", "«a b».c"),
            ("α.x₁'", "α.x₁'"),
            ("«»", "«»"),
            // Inaccessible names and pseudo-syntax are printed as they are.
            ("«a b».x✝", "a b.x✝"),
            ("«#0».«a b»", "#0.a b"),
        ];
        let ids: Vec<u64> = cases.iter().map(|(name, _)| f.name(name)).collect();
        let seven = f.num_name("Foo", 7);
        let dev = plain(&f.plain()).expect("parse");
        for ((name, printed), id) in cases.iter().zip(&ids) {
            assert_eq!(dev.names.display(*id as u32), *printed, "{name}");
        }
        assert_eq!(dev.names.display(seven as u32), "Foo.7");
        assert_eq!(dev.names.display(0), "[anonymous]");
        assert_eq!(escape_part("a»b"), "a»b");

        // The two spellings of Classical.choice are different names that
        // nanoda's configuration spells alike.
        let (dotted, escaped) = (ids[0] as u32, ids[1] as u32);
        assert_ne!(dotted, escaped);
        assert_eq!(dev.names.find("Classical.choice"), Some(dotted));
        assert_eq!(
            dev.names
                .lookup(&[Component::Str("Classical.choice".into())]),
            Some(escaped)
        );
        assert_eq!(dev.names.dotted(escaped), dev.names.dotted(dotted));
        for id in ids.iter().map(|id| *id as u32).chain([seven as u32]) {
            assert_eq!(dev.names.lookup(&dev.names.components(id)), Some(id));
        }
        let parts = serde_json::to_string(&dev.names.components(seven as u32)).expect("json");
        assert_eq!(parts, r#"["Foo",7]"#);
        let back: Vec<Component> = serde_json::from_str(&parts).expect("parts");
        assert_eq!(dev.names.lookup(&back), Some(seven as u32));
    }

    #[test]
    fn rejects_malformed_files() {
        let meta = serde_json::json!({ "meta": fixture::meta() }).to_string();
        let cases = [
            ("a skipped index", r#"{"in":2,"str":{"pre":0,"str":"A"}}"#),
            (
                "an undefined prefix",
                r#"{"in":1,"str":{"pre":5,"str":"A"}}"#,
            ),
            (
                "an unknown key",
                r#"{"in":1,"str":{"pre":0,"str":"A"},"extra":1}"#,
            ),
            (
                "an undefined expression",
                r#"{"app":{"arg":0,"fn":0},"ie":0}"#,
            ),
            ("two payloads", r#"{"bvar":0,"ie":0,"sort":0}"#),
            ("a bad natural", r#"{"ie":0,"natVal":"12a"}"#),
            (
                "a bad binder info",
                "{\"ie\":0,\"sort\":0}\n{\"ie\":1,\"lam\":{\"binderInfo\":\"weird\",\"body\":0,\"name\":0,\"type\":0}}",
            ),
            (
                "a blank line",
                "{\"ie\":0,\"sort\":0}\n\n{\"ie\":1,\"sort\":0}",
            ),
        ];
        for (what, line) in cases {
            let result = plain(&format!("{meta}\n{line}\n"));
            assert!(result.is_err(), "accepted {what}");
        }
        let mut f = File::new();
        let sort = f.sort(0);
        f.axiom("A", &[], sort);
        f.axiom("A", &[], sort);
        assert!(
            plain(&f.plain()).is_err(),
            "accepted a duplicate declaration"
        );
        let old = r#"{"meta":{"exporter":{"name":"lean4export","version":"3.0.0"},"format":{"version":"3.0.0"},"lean":{"githash":"x","version":"y"}}}"#;
        assert!(plain(&format!("{old}\n")).is_err(), "accepted format 3.0.0");
        let mut incomplete = File::new();
        let sort = incomplete.sort(0);
        let n = incomplete.name("Quot");
        incomplete.raw(format!(
            r#"{{"quot":{{"kind":"type","levelParams":[],"name":{n},"type":{sort}}}}}"#
        ));
        assert!(
            plain(&incomplete.plain()).is_err(),
            "accepted a partial quotient package"
        );
    }

    #[test]
    fn reads_lev_export_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (base, top) = fixture::base_and_top();
        fixture::write_export(
            dir.path(),
            &[("Base", &[], &base), ("Top", &["Base"], &top)],
        );
        let dev = read_export(dir.path(), 2).expect("read");
        assert_eq!(dev.modules.len(), 2);
        assert_eq!(dev.modules[1].imports[0].module, "Base");
        assert_eq!(
            dev.modules[1].exporter.as_deref(),
            Some(super::EXPORT_FORMAT)
        );
        let hid = dev.names.find("Base.hid").expect("name");
        assert!(dev.hidden.contains(&hid));
        let lemma = dev.names.find("Base.lemma").expect("name");
        assert_eq!(dev.owners[&lemma].kind, Kind::Thm);
        assert_eq!(dev.owners[&lemma].module, 0);
        let f = dev.names.find("Top.f").expect("name");
        assert_eq!(dev.owners[&f].module, 1);
        assert!(dev.duplicates.is_empty());

        // A module file without its hidden list is rejected.
        let text = fs::read_to_string(dir.path().join("Top.ndjson")).expect("module");
        let truncated: Vec<&str> = text.lines().collect();
        fs::write(
            dir.path().join("Top.ndjson"),
            truncated[..truncated.len() - 1].join("\n") + "\n",
        )
        .expect("write");
        assert!(read_export(dir.path(), 1).is_err());
    }
}
