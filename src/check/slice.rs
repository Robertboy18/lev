//! Slices: one module's own declarations plus the closure of what they
//! reference, under a view that decides which imports appear as axioms.
//!
//! The closure follows only the type of an axiomized constant and the type
//! and value of everything else. Inductive blocks (types, constructors and
//! recursors) and the quotient package are included as units. Pinned
//! constants and what they depend on are never axiomized. The slice key is a
//! SHA-256 over the sorted `(name, role, kind, hash)` entries, where the
//! name is its hash, component by component, and the hash is the full hash
//! for own and full entries and the type hash for axiom entries.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::hash::{Hash, Index, Node};
use super::read::{Development, EXPORT_FORMAT, Kind, LEAN_EXPORT_VERSION, NameId};

/// Which imported constants a slice shows as axioms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    /// Imported theorems and opaques, and imported definitions hidden in
    /// their module's exported view: what the module saw when it was built.
    Exported,
    /// Imported theorems and opaques only.
    Theorems,
    /// Nothing: imports appear in full.
    Closure,
}

impl View {
    /// Views in fallback order.
    pub const ALL: [View; 3] = [View::Exported, View::Theorems, View::Closure];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exported => "exported",
            Self::Theorems => "theorems",
            Self::Closure => "closure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Role {
    Own,
    Full,
    Axiom,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Full => "full",
            Self::Axiom => "axiom",
        }
    }
}

/// One constant of a slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: NameId,
    pub(crate) role: Role,
    /// The constant's kind as its owner declares it.
    pub(crate) kind: Kind,
    /// FH for own and full entries, TH for axiom entries.
    pub(crate) hash: Hash,
    /// Module whose declaration the entry copies.
    pub(crate) owner: u32,
}

/// One unit of a slice, in the order the slice file declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) module: u32,
    pub(crate) unit: u32,
    pub(crate) role: Role,
}

#[derive(Debug, Clone)]
pub(crate) struct Slice {
    pub(crate) module: u32,
    pub(crate) view: View,
    /// Units in dependency order.
    pub(crate) items: Vec<Item>,
    /// Entries sorted by name hash.
    pub(crate) entries: Vec<Entry>,
    /// Referenced constants that no module declares.
    pub(crate) missing: Vec<NameId>,
    pub(crate) key: Hash,
}

impl Slice {
    /// Axioms a kernel must admit to check the slice: the axiomized imports
    /// and the module's own axioms, spelled the way nanoda's configuration
    /// matches them. Which real axioms are used, and whether they are
    /// standard, is the linker's rule L6.
    pub(crate) fn assumptions(&self, dev: &Development) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.role == Role::Axiom || (entry.role == Role::Own && entry.kind == Kind::Axiom)
            })
            .map(|entry| dev.names.dotted(entry.name))
            .collect()
    }
}

/// How a constant of `owner_kind` appears when imported under `view`.
fn role_of(
    dev: &Development,
    pinned: &HashSet<NameId>,
    name: NameId,
    owner_kind: Kind,
    view: View,
) -> Role {
    if pinned.contains(&name) {
        return Role::Full;
    }
    match (owner_kind, view) {
        (Kind::Thm | Kind::Opaque, View::Exported | View::Theorems) => Role::Axiom,
        (Kind::Def, View::Exported) if dev.hidden.contains(&name) => Role::Axiom,
        _ => Role::Full,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    Open,
    Done,
}

/// Build the slice of `module` under `view`.
pub(crate) fn build(
    dev: &Development,
    index: &Index,
    pinned: &HashSet<NameId>,
    module: u32,
    view: View,
) -> Result<Slice> {
    let own_units = dev.modules[module as usize].units.len();
    let mut state: HashMap<(u32, u32), Visit> = HashMap::new();
    let mut items = Vec::new();
    let mut missing = BTreeSet::new();
    // (item, expanded): a unit is emitted once all of its dependencies are.
    let mut stack: Vec<(Item, bool)> = Vec::new();
    for unit in 0..own_units as u32 {
        let root = Item {
            module,
            unit,
            role: Role::Own,
        };
        stack.push((root, false));
        while let Some((item, expanded)) = stack.pop() {
            let key = (item.module, item.unit);
            if expanded {
                state.insert(key, Visit::Done);
                items.push(item);
                continue;
            }
            match state.get(&key) {
                Some(Visit::Done) => continue,
                Some(Visit::Open) => bail!(
                    "declarations of {} depend on each other",
                    describe(dev, item)
                ),
                None => {}
            }
            state.insert(key, Visit::Open);
            stack.push((item, true));
            let unit_index = &index.modules[item.module as usize].units[item.unit as usize];
            let refs = if item.role == Role::Axiom {
                &unit_index.type_refs
            } else {
                &unit_index.refs
            };
            for name in refs.iter().rev() {
                let Some(owner) = dev.owner_for(module, *name) else {
                    missing.insert(*name);
                    continue;
                };
                let role = if owner.module == module {
                    Role::Own
                } else {
                    role_of(dev, pinned, *name, owner.kind, view)
                };
                let next = Item {
                    module: owner.module,
                    unit: owner.unit,
                    role,
                };
                match state.get(&(next.module, next.unit)) {
                    Some(Visit::Done) => {}
                    Some(Visit::Open) => bail!(
                        "declarations of {} and {} depend on each other",
                        describe(dev, item),
                        dev.names.display(*name)
                    ),
                    None => stack.push((next, false)),
                }
            }
        }
    }

    let mut entries = Vec::new();
    for item in &items {
        let unit_index = &index.modules[item.module as usize].units[item.unit as usize];
        let unit = &dev.modules[item.module as usize].units[item.unit as usize];
        for (name, kind) in unit.members() {
            entries.push(Entry {
                name,
                role: item.role,
                kind,
                hash: if item.role == Role::Axiom {
                    unit_index.ty
                } else {
                    unit_index.full
                },
                owner: item.module,
            });
        }
    }
    sort_entries(&mut entries, index);
    let key = key(&dev.githash, view, &entries, index);
    Ok(Slice {
        module,
        view,
        items,
        entries,
        missing: missing.into_iter().collect(),
        key,
    })
}

fn sort_entries(entries: &mut [Entry], index: &Index) {
    entries.sort_by(|a, b| {
        let name = |entry: &Entry| index.names[entry.name as usize];
        (name(a), a.role, a.kind, a.hash).cmp(&(name(b), b.role, b.kind, b.hash))
    });
}

/// K = SHA-256 of the formats, the Lean githash, the view and the sorted entries.
fn key(githash: &str, view: View, entries: &[Entry], index: &Index) -> Hash {
    let mut node = Node::new("lev-slice/1");
    node.text(EXPORT_FORMAT)
        .text(LEAN_EXPORT_VERSION)
        .text(githash)
        .text(view.as_str())
        .number(entries.len() as u64);
    for entry in entries {
        node.hash(&index.names[entry.name as usize])
            .text(entry.role.as_str())
            .text(entry.kind.as_str())
            .hash(&entry.hash);
    }
    node.finish()
}

fn describe(dev: &Development, item: Item) -> String {
    let unit = &dev.modules[item.module as usize].units[item.unit as usize];
    unit.members()
        .first()
        .map(|(name, _)| dev.names.display(*name))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::fixture::{self, File};
    use super::super::hash::{Index, index};
    use super::super::read::{Development, read_export};
    use super::{Role, Slice, View, build};

    pub(crate) fn develop(
        modules: &[(&str, &[&str], &File)],
    ) -> (tempfile::TempDir, Development, Index) {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture::write_export(dir.path(), modules);
        let dev = read_export(dir.path(), 2).expect("read");
        let index = index(&dev, 2);
        (dir, dev, index)
    }

    fn slice(dev: &Development, index: &Index, module: &str, view: View) -> Slice {
        let module = dev.module_index(module).expect("module");
        build(dev, index, &fixture::pinned(dev), module, view).expect("slice")
    }

    fn roles(dev: &Development, slice: &Slice) -> HashMap<String, Role> {
        slice
            .entries
            .iter()
            .map(|entry| (dev.names.display(entry.name), entry.role))
            .collect()
    }

    #[test]
    fn roles_follow_the_view() {
        let (base, top) = fixture::base_and_top();
        let (_dir, dev, index) = develop(&[("Base", &[], &base), ("Top", &["Base"], &top)]);

        let exported = roles(&dev, &slice(&dev, &index, "Top", View::Exported));
        for own in ["Top.f", "Top.g", "Top.t", "Top.o"] {
            assert_eq!(exported[own], Role::Own, "{own}");
        }
        for full in ["Base.id", "Nat", "Nat.zero", "Nat.succ", "Nat.rec"] {
            assert_eq!(exported[full], Role::Full, "{full}");
        }
        for axiom in ["Base.hid", "Base.lemma", "Base.opq"] {
            assert_eq!(exported[axiom], Role::Axiom, "{axiom}");
        }
        assert!(!exported.contains_key("Base.helper"));

        let theorems = roles(&dev, &slice(&dev, &index, "Top", View::Theorems));
        assert_eq!(theorems["Base.hid"], Role::Full);
        assert_eq!(theorems["Base.lemma"], Role::Axiom);
        assert_eq!(theorems["Base.opq"], Role::Axiom);
        assert!(!theorems.contains_key("Base.helper"));

        // The closure follows the theorem's proof to the helper it mentions.
        let closure = roles(&dev, &slice(&dev, &index, "Top", View::Closure));
        assert!(closure.values().all(|role| *role != Role::Axiom));
        assert_eq!(closure["Base.helper"], Role::Full);
        assert_eq!(closure["Base.lemma"], Role::Full);

        // A module without imports has the same entries in every view; the
        // view is still part of the key.
        let slices: Vec<_> = View::ALL
            .into_iter()
            .map(|view| slice(&dev, &index, "Base", view))
            .collect();
        assert!(
            slices
                .windows(2)
                .all(|pair| pair[0].entries == pair[1].entries)
        );
        assert!(slices.windows(2).all(|pair| pair[0].key != pair[1].key));
        assert!(
            slices[0]
                .entries
                .iter()
                .all(|entry| entry.role == Role::Own)
        );
    }

    #[test]
    fn pinned_constants_are_never_axiomized() {
        let mut base = File::new();
        fixture::nat(&mut base);
        let nat = base.cnst("Nat", &[]);
        let ty = base.pi("n", nat, nat);
        let x = base.bvar(0);
        let value = base.lam("n", nat, x);
        base.def("Nat.add", &[], ty, value);
        base.hide("Nat.add");
        let mut top = File::new();
        let nat = top.cnst("Nat", &[]);
        let ty = top.pi("n", nat, nat);
        let add = top.cnst("Nat.add", &[]);
        top.def("Top.add", &[], ty, add);
        let (_dir, dev, index) = develop(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let exported = roles(&dev, &slice(&dev, &index, "Top", View::Exported));
        assert_eq!(exported["Nat.add"], Role::Full);
    }

    #[test]
    fn keys_change_with_statements_but_not_with_proofs() {
        let key = |proof: &str, statement: bool| {
            let (mut base, top) = fixture::base_and_top();
            let ty = if statement {
                let prop = base.sort(0);
                let p = base.bvar(0);
                base.pi("p", prop, p)
            } else {
                fixture::prop_id(&mut base)
            };
            let value = base.cnst(proof, &[]);
            base.thm("Base.extra", &[], ty, value);
            let mut top = top;
            let extra = top.cnst("Base.extra", &[]);
            let ty = fixture::prop_id(&mut top);
            top.thm("Top.uses", &[], ty, extra);
            let (_dir, dev, index) = develop(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
            (
                slice(&dev, &index, "Base", View::Exported).key,
                slice(&dev, &index, "Top", View::Exported).key,
                slice(&dev, &index, "Top", View::Closure).key,
            )
        };
        let (base, top, top_closure) = key("Proof.one", false);
        let (proof_base, proof_top, proof_closure) = key("Proof.two", false);
        let (statement_base, statement_top, _) = key("Proof.one", true);
        assert_ne!(base, proof_base);
        assert_eq!(top, proof_top);
        assert_ne!(top_closure, proof_closure);
        assert_ne!(base, statement_base);
        assert_ne!(top, statement_top);
    }

    #[test]
    fn items_come_after_their_dependencies() {
        let (base, top) = fixture::base_and_top();
        let (_dir, dev, index) = develop(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        for view in View::ALL {
            let slice = slice(&dev, &index, "Top", view);
            let mut seen = std::collections::HashSet::new();
            for item in &slice.items {
                let unit = &index.modules[item.module as usize].units[item.unit as usize];
                let refs = if item.role == Role::Axiom {
                    &unit.type_refs
                } else {
                    &unit.refs
                };
                for name in refs {
                    let owner = dev.owner_for(slice.module, *name).expect("owner");
                    assert!(seen.contains(&(owner.module, owner.unit)), "{view:?}");
                }
                seen.insert((item.module, item.unit));
            }
            assert!(slice.missing.is_empty());
        }
    }
}
