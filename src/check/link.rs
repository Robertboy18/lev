//! Linker: checks that the per-module results compose.
//!
//! - L1 owners: every referenced constant has an owner, and only theorems
//!   with equal type hashes have more than one.
//! - L2 axiom entries: the owner declares a theorem or opaque (or, in the
//!   exported view, a hidden definition) with the same type hash, and the
//!   owner's slice was accepted by the same kernel.
//! - L3 full entries: the copy has the owner's kind and full hash.
//! - L4 no cycles in the graph "slice copies a constant owned by module".
//! - L5 every slice is accepted by every selected kernel.
//! - L6 the real axioms in use are `propext`, `Classical.choice` and
//!   `Quot.sound`, each with the reference's full hash. Every axiom
//!   declaration in use is checked, and axioms are told apart by their
//!   names' components, never by how the names print.
//! - L7 the pinned constants, the standard axioms and everything their types
//!   and values depend on in the reference, transitively, have the
//!   reference's full hash wherever the development declares them, and are
//!   never axiomized.
//! - L8 every module comes from the same toolchain and exporter, every import
//!   is listed earlier in the manifest, and the reference matches the toolchain.
//!
//! A slice that a kernel could not check (its last run ended in an error, a
//! timeout or a decline) breaks no rule, and neither does relying on its
//! theorems: the development is then neither accepted nor rejected.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::hash::{Hash, Index};
use super::kernel::{KernelKind, Verdict};
use super::read::{Component, Development, EXPORT_FORMAT, Kind, NameId, Owner};
use super::slice::{Role, Slice, View};

/// The axioms Lean's standard library and Mathlib accept.
pub(crate) const STANDARD_AXIOMS: [&str; 3] = ["propext", "Classical.choice", "Quot.sound"];

/// Lake's `primitiveTargets`, the kernel's other built-ins and the quotient
/// package: constants kernels recognize by name, so they must be the
/// toolchain's own and are never axiomized.
pub(crate) const PINNED: [&str; 40] = [
    "Nat.add",
    "Nat.sub",
    "Nat.mul",
    "Nat.pow",
    "Nat.gcd",
    "Nat.div",
    "Nat.mod",
    "Nat.beq",
    "Nat.ble",
    "Nat.land",
    "Nat.lor",
    "Nat.xor",
    "Nat.shiftLeft",
    "Nat.shiftRight",
    "String.ofList",
    "Char.ofNat",
    "List",
    "eagerReduce",
    "Nat",
    "String",
    "String.mk",
    "Char",
    "optParam",
    "autoParam",
    "semiOutParam",
    "outParam",
    "Nat.zero",
    "Nat.succ",
    "Bool",
    "Bool.true",
    "Bool.false",
    "List.nil",
    "List.cons",
    "Eq",
    "Quot",
    "Quot.mk",
    "Quot.lift",
    "Quot.ind",
    "Lean.reduceBool",
    "Lean.reduceNat",
];

/// A broken linker rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Violation {
    pub rule: &'static str,
    pub module: Option<String>,
    pub message: String,
}

impl Violation {
    fn new(rule: &'static str, module: Option<&str>, message: String) -> Self {
        Self {
            rule,
            module: module.map(str::to_owned),
            message,
        }
    }
}

/// The pinned constants and the standard axioms.
fn roots() -> impl Iterator<Item = &'static str> {
    PINNED.into_iter().chain(STANDARD_AXIOMS)
}

/// The reference's pinned constants, its standard axioms and everything
/// their types and values reference, transitively: what the kernel's
/// primitives and the standard axioms mean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pins {
    pub(crate) githash: String,
    constants: Vec<Pin>,
}

/// One constant of the closure, by its name's components, with the full hash
/// of the unit that declares it in the reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pin {
    name: Vec<Component>,
    full: Hash,
}

impl Pins {
    pub(crate) fn new(reference: &Development, index: &Index) -> Self {
        let mut stack: Vec<Owner> = roots()
            .filter_map(|name| reference.names.find(name))
            .filter_map(|id| reference.owners.get(&id).copied())
            .collect();
        let mut seen = HashSet::new();
        let mut constants = Vec::new();
        while let Some(owner) = stack.pop() {
            if !seen.insert((owner.module, owner.unit)) {
                continue;
            }
            let unit = &index.modules[owner.module as usize].units[owner.unit as usize];
            let decl = &reference.modules[owner.module as usize].units[owner.unit as usize];
            for (name, _) in decl.members() {
                constants.push(Pin {
                    name: reference.names.components(name),
                    full: unit.full,
                });
            }
            stack.extend(
                unit.refs
                    .iter()
                    .filter_map(|name| reference.owners.get(name).copied()),
            );
        }
        constants.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            githash: reference.githash.clone(),
            constants,
        }
    }
}

/// The pins as constants of one development.
pub(crate) struct Pinned {
    /// What no slice axiomizes: the pinned constants, the standard axioms
    /// and every constant of the reference's closure that the development
    /// names.
    pub(crate) names: HashSet<NameId>,
    /// The reference's full hash of every closure constant the development names.
    hashes: HashMap<NameId, Hash>,
}

impl Pinned {
    pub(crate) fn new(dev: &Development, pins: &Pins) -> Self {
        let hashes: HashMap<NameId, Hash> = pins
            .constants
            .iter()
            .filter_map(|pin| Some((dev.names.lookup(&pin.name)?, pin.full)))
            .collect();
        let mut names: HashSet<NameId> = hashes.keys().copied().collect();
        names.extend(roots().filter_map(|name| dev.names.find(name)));
        Self { names, hashes }
    }
}

/// What one checked slice contributes to the link.
#[derive(Debug, Clone)]
pub(crate) struct SliceLink {
    /// Owners of the slice's axiom entries, whose slices must be accepted.
    pub(crate) axiom_owners: BTreeSet<u32>,
    /// Owners of every axiom and full entry: the slice graph's edges.
    pub(crate) owners: BTreeSet<u32>,
    pub(crate) missing: Vec<NameId>,
    pub(crate) violations: Vec<Violation>,
}

/// Rules L2, L3 and the axiom half of L7 for one slice.
pub(crate) fn check_slice(
    dev: &Development,
    index: &Index,
    pinned: &HashSet<NameId>,
    slice: &Slice,
) -> SliceLink {
    let module_name = dev.modules[slice.module as usize].name.as_str();
    let mut link = SliceLink {
        axiom_owners: BTreeSet::new(),
        owners: BTreeSet::new(),
        missing: slice.missing.clone(),
        violations: Vec::new(),
    };
    let mut flag = |rule, message| {
        link.violations
            .push(Violation::new(rule, Some(module_name), message))
    };
    for entry in &slice.entries {
        // Names are only spelled out for messages.
        let name = || dev.names.display(entry.name);
        let owner_name = &dev.modules[entry.owner as usize].name;
        let Some(owner) = dev.owner_in(entry.owner, entry.name) else {
            let rule = if entry.role == Role::Full { "L3" } else { "L2" };
            flag(
                rule,
                format!(
                    "{} is copied from {owner_name}, which does not declare it",
                    name()
                ),
            );
            continue;
        };
        let canonical = dev.owner_for(slice.module, entry.name).map(|o| o.module);
        if canonical != Some(entry.owner) {
            flag(
                "L1",
                format!(
                    "{} is copied from {owner_name}, which is not its owner here",
                    name()
                ),
            );
        }
        let unit = &index.modules[owner.module as usize].units[owner.unit as usize];
        match entry.role {
            Role::Own => {
                if entry.owner != slice.module || entry.hash != unit.full {
                    flag(
                        "L3",
                        format!("own entry {} differs from the module's declaration", name()),
                    );
                }
            }
            Role::Axiom => {
                if pinned.contains(&entry.name) {
                    flag("L7", format!("pinned constant {} is axiomized", name()));
                }
                let allowed = match owner.kind {
                    Kind::Thm | Kind::Opaque => slice.view != View::Closure,
                    Kind::Def => slice.view == View::Exported && dev.hidden.contains(&entry.name),
                    _ => false,
                };
                if !allowed {
                    flag(
                        "L2",
                        format!(
                            "{} is a {} of {owner_name}, which the {} view does not axiomize",
                            name(),
                            owner.kind.as_str(),
                            slice.view.as_str()
                        ),
                    );
                }
                if entry.kind != owner.kind || entry.hash != unit.ty {
                    flag(
                        "L2",
                        format!(
                            "the axiomized type of {} differs from {owner_name}'s",
                            name()
                        ),
                    );
                }
                link.axiom_owners.insert(entry.owner);
                link.owners.insert(entry.owner);
            }
            Role::Full => {
                if entry.kind != owner.kind || entry.hash != unit.full {
                    flag(
                        "L3",
                        format!("the copy of {} differs from {owner_name}'s", name()),
                    );
                }
                link.owners.insert(entry.owner);
            }
        }
    }
    link.owners.remove(&slice.module);
    link.axiom_owners.remove(&slice.module);
    link
}

/// The final verdict of one module under one kernel.
pub(crate) struct Outcome<'a> {
    pub(crate) module: u32,
    pub(crate) kernel: KernelKind,
    pub(crate) verdict: Verdict,
    /// The slice that produced the verdict, if one was built.
    pub(crate) slice: Option<&'a SliceLink>,
}

pub(crate) struct Linked {
    pub(crate) violations: Vec<Violation>,
    /// Real axioms used by the checked modules.
    pub(crate) axioms: Vec<String>,
}

/// All rules over the modules in `scope`.
pub(crate) fn link(
    dev: &Development,
    index: &Index,
    pins: &Pins,
    pinned: &Pinned,
    scope: &[u32],
    outcomes: &[Outcome<'_>],
) -> Linked {
    let mut violations = Vec::new();
    let name_of = |module: u32| dev.modules[module as usize].name.as_str();
    consistency(dev, pins, &mut violations);
    owners(dev, index, &mut violations);

    let verdicts: HashMap<(u32, KernelKind), Verdict> = outcomes
        .iter()
        .map(|outcome| ((outcome.module, outcome.kernel), outcome.verdict))
        .collect();
    let mut edges: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); dev.modules.len()];
    for outcome in outcomes {
        let module = name_of(outcome.module);
        let kernel = outcome.kernel.as_str();
        // An error, a timeout or a decline leaves the module unchecked.
        if outcome.verdict == Verdict::Reject {
            violations.push(Violation::new(
                "L5",
                Some(module),
                format!("rejected by {kernel}"),
            ));
        }
        let Some(slice) = outcome.slice else {
            continue;
        };
        for name in &slice.missing {
            violations.push(Violation::new(
                "L1",
                Some(module),
                format!(
                    "references {}, which no module declares",
                    dev.names.display(*name)
                ),
            ));
        }
        for owner in &slice.axiom_owners {
            match verdicts.get(&(*owner, outcome.kernel)) {
                // An owner the kernel could not check leaves this module
                // unknown too, which the owner's own result already says.
                Some(Verdict::Accept | Verdict::Error | Verdict::Decline) => {}
                Some(Verdict::Reject) | None => violations.push(Violation::new(
                    "L2",
                    Some(module),
                    format!(
                        "relies on theorems of {}, whose slice {kernel} did not accept",
                        name_of(*owner)
                    ),
                )),
            }
        }
        violations.extend(slice.violations.iter().cloned());
        edges[outcome.module as usize].extend(slice.owners.iter().copied());
    }
    for cycle in cycles(scope, &edges) {
        let mut names: Vec<&str> = cycle.iter().map(|module| name_of(*module)).collect();
        names.sort_unstable();
        violations.push(Violation::new(
            "L4",
            None,
            format!("slices depend on each other: {}", names.join(", ")),
        ));
    }

    let axioms = axioms(dev, index, pinned, scope, &mut violations);
    pins_rule(dev, index, pinned, &mut violations);
    violations.sort();
    violations.dedup();
    Linked { violations, axioms }
}

fn consistency(dev: &Development, pins: &Pins, violations: &mut Vec<Violation>) {
    if pins.githash != dev.githash {
        violations.push(Violation::new(
            "L8",
            None,
            format!(
                "the reference was exported by Lean {}, the development by {}",
                pins.githash, dev.githash
            ),
        ));
    }
    let first = dev.modules.first().map(|module| &module.meta);
    for (position, module) in dev.modules.iter().enumerate() {
        let name = Some(module.name.as_str());
        if module.exporter.as_deref() != Some(EXPORT_FORMAT) {
            violations.push(Violation::new(
                "L8",
                name,
                format!(
                    "exported by {:?}, expected {EXPORT_FORMAT}",
                    module.exporter
                ),
            ));
        }
        let githash = module
            .meta
            .get("lean")
            .and_then(|lean| lean.get("githash"))
            .and_then(|githash| githash.as_str());
        if githash != Some(dev.githash.as_str()) {
            violations.push(Violation::new(
                "L8",
                name,
                format!(
                    "exported by Lean {githash:?}, the manifest says {}",
                    dev.githash
                ),
            ));
        } else if Some(&module.meta) != first {
            violations.push(Violation::new(
                "L8",
                name,
                "export metadata differs from the first module's".to_owned(),
            ));
        }
        for import in &module.imports {
            match dev.module_index(&import.module) {
                None => violations.push(Violation::new(
                    "L8",
                    name,
                    format!(
                        "imports {}, which the manifest does not list",
                        import.module
                    ),
                )),
                Some(index) if index as usize >= position => violations.push(Violation::new(
                    "L8",
                    name,
                    format!("is listed before its import {}", import.module),
                )),
                Some(_) => {}
            }
        }
    }
}

fn owners(dev: &Development, index: &Index, violations: &mut Vec<Violation>) {
    let mut duplicates: Vec<_> = dev.duplicates.iter().collect();
    duplicates.sort_by_key(|(name, _)| index.names[**name as usize]);
    for (name, later) in duplicates {
        let Some(first) = dev.owners.get(name) else {
            continue;
        };
        let all: Vec<_> = std::iter::once(first).chain(later.iter()).collect();
        let modules: Vec<&str> = all
            .iter()
            .map(|owner| dev.modules[owner.module as usize].name.as_str())
            .collect();
        let type_hash =
            |owner: &&Owner| index.modules[owner.module as usize].units[owner.unit as usize].ty;
        let theorems = all.iter().all(|owner| owner.kind == Kind::Thm);
        let same = all
            .iter()
            .all(|owner| type_hash(owner) == type_hash(&first));
        if !theorems || !same {
            violations.push(Violation::new(
                "L1",
                None,
                format!(
                    "{} is declared by {}{}",
                    dev.names.display(*name),
                    modules.join(" and "),
                    if theorems {
                        " with different statements"
                    } else {
                        ""
                    }
                ),
            ));
        }
    }
}

/// L6: the real axioms the checked modules' declarations use. Each axiom
/// declaration in use is checked, and a standard axiom is recognized by its
/// name's components, so `«Classical.choice»` is not `Classical.choice`.
fn axioms(
    dev: &Development,
    index: &Index,
    pinned: &Pinned,
    scope: &[u32],
    violations: &mut Vec<Violation>,
) -> Vec<String> {
    let standard: HashSet<NameId> = STANDARD_AXIOMS
        .iter()
        .filter_map(|name| dev.names.find(name))
        .collect();
    // Every axiom declaration in use, by its unit, with the first user.
    let mut used: BTreeMap<(u32, u32), (NameId, u32)> = BTreeMap::new();
    for &module in scope {
        for unit in &index.modules[module as usize].units {
            for name in &unit.refs {
                let Some(owner) = dev.owner_for(module, *name) else {
                    continue;
                };
                if owner.kind == Kind::Axiom {
                    used.entry((owner.module, owner.unit))
                        .or_insert((*name, module));
                }
            }
        }
    }
    let mut shown = BTreeSet::new();
    for ((owner, unit), (name, user)) in used {
        let printed = dev.names.display(name);
        let user = Some(dev.modules[user as usize].name.as_str());
        let full = index.modules[owner as usize].units[unit as usize].full;
        if !standard.contains(&name) {
            violations.push(Violation::new(
                "L6",
                user,
                format!("uses the axiom {printed}"),
            ));
        } else if pinned.hashes.get(&name) != Some(&full) {
            violations.push(Violation::new(
                "L6",
                user,
                format!("uses an axiom {printed} that differs from the toolchain's"),
            ));
        }
        shown.insert(printed);
    }
    shown.into_iter().collect()
}

/// L7: every constant of the reference's closure has the reference's full
/// hash in every module that declares it, and every pinned constant the
/// development declares is in the reference.
fn pins_rule(dev: &Development, index: &Index, pinned: &Pinned, violations: &mut Vec<Violation>) {
    let module_of = |owner: &Owner| Some(dev.modules[owner.module as usize].name.as_str());
    for (name, reference) in &pinned.hashes {
        let owners = dev
            .owners
            .get(name)
            .into_iter()
            .chain(dev.duplicates.get(name).into_iter().flatten());
        for owner in owners {
            let full = index.modules[owner.module as usize].units[owner.unit as usize].full;
            if full != *reference {
                violations.push(Violation::new(
                    "L7",
                    module_of(owner),
                    format!("{} differs from the toolchain's", dev.names.display(*name)),
                ));
            }
        }
    }
    for name in PINNED {
        let Some(id) = dev.names.find(name) else {
            continue;
        };
        if pinned.hashes.contains_key(&id) {
            continue;
        }
        if let Some(owner) = dev.owners.get(&id) {
            violations.push(Violation::new(
                "L7",
                module_of(owner),
                format!("{name} is not in the reference"),
            ));
        }
    }
}

/// Strongly connected components with more than one module (Tarjan).
fn cycles(scope: &[u32], edges: &[BTreeSet<u32>]) -> Vec<Vec<u32>> {
    let count = edges.len();
    let mut order = vec![usize::MAX; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut next = 0;
    let mut out = Vec::new();
    let successors: Vec<Vec<u32>> = edges
        .iter()
        .map(|set| set.iter().copied().collect())
        .collect();
    for &root in scope {
        if order[root as usize] != usize::MAX {
            continue;
        }
        let mut work = vec![(root, 0_usize)];
        order[root as usize] = next;
        low[root as usize] = next;
        next += 1;
        stack.push(root);
        on_stack[root as usize] = true;
        while let Some(top) = work.last_mut() {
            let (node, position) = *top;
            let node_index = node as usize;
            if let Some(&target) = successors[node_index].get(position) {
                top.1 += 1;
                let target_index = target as usize;
                if order[target_index] == usize::MAX {
                    order[target_index] = next;
                    low[target_index] = next;
                    next += 1;
                    stack.push(target);
                    on_stack[target_index] = true;
                    work.push((target, 0));
                } else if on_stack[target_index] {
                    low[node_index] = low[node_index].min(order[target_index]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent as usize] = low[parent as usize].min(low[node_index]);
            }
            if low[node_index] == order[node_index] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member as usize] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                if component.len() > 1 {
                    out.push(component);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{self, File};
    use super::super::hash::{Index, index};
    use super::super::kernel::{KernelKind, Verdict};
    use super::super::read::{Development, read_export, read_plain};
    use super::super::slice::{Role, Slice, View, build};
    use super::{Outcome, Pinned, Pins, SliceLink, Violation, check_slice, link};

    struct Setup {
        _dir: tempfile::TempDir,
        dev: Development,
        index: Index,
        pins: Pins,
        pinned: Pinned,
    }

    /// The modules, checked against the fixtures' reference (`Nat` only).
    fn setup(modules: &[(&str, &[&str], &File)]) -> Setup {
        let mut reference = File::new();
        fixture::nat(&mut reference);
        setup_with(modules, &reference)
    }

    fn setup_with(modules: &[(&str, &[&str], &File)], reference: &File) -> Setup {
        let dir = tempfile::tempdir().expect("tempdir");
        let export = dir.path().join("export");
        fixture::write_export(&export, modules);
        let dev = read_export(&export, 2).expect("read");
        let index = index(&dev, 2);
        let reference =
            read_plain(&fixture::write_reference(dir.path(), reference)).expect("reference");
        let pins = Pins::new(&reference, &super::super::hash::index(&reference, 1));
        let pinned = Pinned::new(&dev, &pins);
        Setup {
            _dir: dir,
            dev,
            index,
            pins,
            pinned,
        }
    }

    /// Link every module's slice; `verdict` and `tamper` act per module name.
    fn link_all(
        setup: &Setup,
        view: View,
        verdict: impl Fn(&str) -> Verdict,
        tamper: impl Fn(&str, &mut Slice),
    ) -> Vec<Violation> {
        let pinned = &setup.pinned.names;
        let scope: Vec<u32> = (0..setup.dev.modules.len() as u32).collect();
        let links: Vec<SliceLink> = scope
            .iter()
            .map(|module| {
                let mut slice =
                    build(&setup.dev, &setup.index, pinned, *module, view).expect("slice");
                tamper(&setup.dev.modules[*module as usize].name, &mut slice);
                check_slice(&setup.dev, &setup.index, pinned, &slice)
            })
            .collect();
        let outcomes: Vec<Outcome<'_>> = scope
            .iter()
            .zip(&links)
            .map(|(module, slice)| Outcome {
                module: *module,
                kernel: KernelKind::Official,
                verdict: verdict(&setup.dev.modules[*module as usize].name),
                slice: Some(slice),
            })
            .collect();
        link(
            &setup.dev,
            &setup.index,
            &setup.pins,
            &setup.pinned,
            &scope,
            &outcomes,
        )
        .violations
    }

    fn accept(_: &str) -> Verdict {
        Verdict::Accept
    }

    fn untouched(_: &str, _: &mut Slice) {}

    fn rules(violations: &[Violation]) -> Vec<&'static str> {
        violations.iter().map(|violation| violation.rule).collect()
    }

    fn tamper_entry(setup: &Setup, slice: &mut Slice, name: &str, role: Role) {
        let id = setup.dev.names.find(name).expect("name");
        let entry = slice
            .entries
            .iter_mut()
            .find(|entry| entry.name == id)
            .expect("entry");
        assert_eq!(entry.role, role);
        entry.hash[0] ^= 1;
    }

    #[test]
    fn a_consistent_development_links_in_every_view() {
        let (base, top) = fixture::base_and_top();
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        for view in View::ALL {
            let violations = link_all(&setup, view, accept, untouched);
            assert!(violations.is_empty(), "{view:?}: {violations:?}");
        }
    }

    #[test]
    fn l1_rejects_a_definition_with_two_owners() {
        let (mut base, mut top) = fixture::base_and_top();
        for file in [&mut base, &mut top] {
            let prop = file.sort(0);
            let value = file.bvar(0);
            file.def("Shared.def", &[], prop, value);
            // Equal statements may have several owners; definitions may not.
            let ty = fixture::prop_id(file);
            file.thm("Shared.thm", &[], ty, value);
        }
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert_eq!(rules(&violations), ["L1"], "{violations:?}");
        assert!(violations[0].message.contains("Shared.def"));
    }

    #[test]
    fn l1_rejects_references_that_no_module_declares() {
        let (base, mut top) = fixture::base_and_top();
        let prop = top.sort(0);
        let missing = top.cnst("Nowhere.c", &[]);
        top.def("Top.dangling", &[], prop, missing);
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert_eq!(rules(&violations), ["L1"], "{violations:?}");
    }

    #[test]
    fn l2_rejects_an_axiom_whose_type_differs_from_its_owner() {
        let (base, top) = fixture::base_and_top();
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let violations = link_all(&setup, View::Exported, accept, |module, slice| {
            if module == "Top" {
                tamper_entry(&setup, slice, "Base.lemma", Role::Axiom);
            }
        });
        assert_eq!(rules(&violations), ["L2"], "{violations:?}");
    }

    #[test]
    fn l2_rejects_axioms_whose_owner_was_not_accepted() {
        let (base, top) = fixture::base_and_top();
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let reject = |module: &str| {
            if module == "Base" {
                Verdict::Reject
            } else {
                Verdict::Accept
            }
        };
        let violations = link_all(&setup, View::Exported, reject, untouched);
        assert_eq!(rules(&violations), ["L2", "L5"], "{violations:?}");
    }

    #[test]
    fn l3_rejects_a_full_copy_that_differs_from_its_owner() {
        let (base, top) = fixture::base_and_top();
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let violations = link_all(&setup, View::Exported, accept, |module, slice| {
            if module == "Top" {
                tamper_entry(&setup, slice, "Base.id", Role::Full);
            }
        });
        assert_eq!(rules(&violations), ["L3"], "{violations:?}");
    }

    #[test]
    fn l4_rejects_a_cycle_between_slices() {
        let mut a = File::new();
        fixture::nat(&mut a);
        let nat = a.cnst("Nat", &[]);
        let from_b = a.cnst("B.y", &[]);
        a.def("A.x", &[], nat, from_b);
        let zero = a.cnst("Nat.zero", &[]);
        a.def("A.z", &[], nat, zero);
        let mut b = File::new();
        let nat = b.cnst("Nat", &[]);
        let from_a = b.cnst("A.z", &[]);
        b.def("B.y", &[], nat, from_a);
        let setup = setup(&[("A", &[], &a), ("B", &["A"], &b)]);
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert_eq!(rules(&violations), ["L4"], "{violations:?}");
        assert!(violations[0].message.contains("A, B"));
    }

    #[test]
    fn l5_rejects_a_slice_a_kernel_rejected() {
        let (base, top) = fixture::base_and_top();
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let verdict = |module: &str| {
            if module == "Top" {
                Verdict::Reject
            } else {
                Verdict::Accept
            }
        };
        let violations = link_all(&setup, View::Exported, verdict, untouched);
        assert_eq!(rules(&violations), ["L5"], "{violations:?}");
        assert_eq!(violations[0].message, "rejected by official");
    }

    #[test]
    fn slices_no_kernel_could_check_break_no_rule() {
        let (base, top) = fixture::base_and_top();
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        // Top relies on Base's theorems, which the kernel could not check.
        for unchecked in [Verdict::Error, Verdict::Decline] {
            let verdict = |module: &str| {
                if module == "Base" {
                    unchecked
                } else {
                    Verdict::Accept
                }
            };
            let violations = link_all(&setup, View::Exported, verdict, untouched);
            assert!(violations.is_empty(), "{unchecked:?}: {violations:?}");
        }
    }

    #[test]
    fn l6_rejects_non_standard_axioms() {
        let (mut base, mut top) = fixture::base_and_top();
        let ty = fixture::prop_id(&mut base);
        base.axiom("Base.bad", &[], ty);
        base.axiom("propext", &[], ty);
        let ty = fixture::prop_id(&mut top);
        let bad = top.cnst("Base.bad", &[]);
        top.thm("Top.cheat", &[], ty, bad);
        let ext = top.cnst("propext", &[]);
        top.thm("Top.ext", &[], ty, ext);
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert_eq!(rules(&violations), ["L6", "L6"], "{violations:?}");
        let messages: Vec<&str> = violations.iter().map(|v| v.message.as_str()).collect();
        assert!(
            messages.contains(&"uses the axiom Base.bad"),
            "{messages:?}"
        );
        // The reference has no propext, so this one is not the toolchain's.
        assert!(
            messages.iter().any(|m| m.contains("propext")),
            "{messages:?}"
        );
    }

    #[test]
    fn l6_tells_an_escaped_name_from_a_standard_axiom() {
        // `«Classical.choice»` has one component and `Classical.choice` two,
        // but both used to print as Classical.choice.
        let mut reference = File::new();
        fixture::nat(&mut reference);
        let ty = fixture::prop_id(&mut reference);
        reference.axiom("Classical.choice", &[], ty);
        let (mut base, mut top) = fixture::base_and_top();
        let ty = fixture::prop_id(&mut base);
        base.axiom("Classical.choice", &[], ty);
        let choice = base.cnst("Classical.choice", &[]);
        base.thm("Base.classical", &[], ty, choice);
        let falsity = top.sort(0);
        top.axiom("«Classical.choice»", &[], falsity);
        let fake = top.cnst("«Classical.choice»", &[]);
        top.thm("Top.bad", &[], falsity, fake);
        let setup = setup_with(
            &[("Base", &[], &base), ("Top", &["Base"], &top)],
            &reference,
        );
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert_eq!(rules(&violations), ["L6"], "{violations:?}");
        assert_eq!(violations[0].module.as_deref(), Some("Top"));
        assert_eq!(violations[0].message, "uses the axiom «Classical.choice»");
    }

    #[test]
    fn l7_pins_what_a_pinned_constant_depends_on() {
        // Nat.land is pinned; Nat.bitwise, which defines it, is not.
        let bitwise = |file: &mut File, value: &str| {
            fixture::nat(file);
            let nat = file.cnst("Nat", &[]);
            let ty = file.pi("n", nat, nat);
            let body = match value {
                "zero" => file.cnst("Nat.zero", &[]),
                _ => file.bvar(0),
            };
            let value = file.lam("n", nat, body);
            file.def("Nat.bitwise", &[], ty, value);
            let bitwise = file.cnst("Nat.bitwise", &[]);
            file.def("Nat.land", &[], ty, bitwise);
        };
        let mut reference = File::new();
        bitwise(&mut reference, "identity");
        let mut base = File::new();
        bitwise(&mut base, "zero");
        base.hide("Nat.bitwise");
        let mut top = File::new();
        let nat = top.cnst("Nat", &[]);
        let ty = top.pi("n", nat, nat);
        let land = top.cnst("Nat.land", &[]);
        top.def("Top.and", &[], ty, land);
        let setup = setup_with(
            &[("Base", &[], &base), ("Top", &["Base"], &top)],
            &reference,
        );
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert_eq!(rules(&violations), ["L7"], "{violations:?}");
        assert_eq!(violations[0].module.as_deref(), Some("Base"));
        assert_eq!(
            violations[0].message,
            "Nat.bitwise differs from the toolchain's"
        );

        // Hidden in Base's exported view, Nat.bitwise is still copied in full.
        let module = setup.dev.module_index("Top").expect("Top");
        let slice = build(
            &setup.dev,
            &setup.index,
            &setup.pinned.names,
            module,
            View::Exported,
        )
        .expect("slice");
        let id = setup.dev.names.find("Nat.bitwise").expect("name");
        let entry = slice
            .entries
            .iter()
            .find(|entry| entry.name == id)
            .expect("entry");
        assert_eq!(entry.role, Role::Full);
    }

    #[test]
    fn l7_rejects_a_pinned_constant_with_another_definition() {
        let mut base = File::new();
        let one = base.succ(0);
        let ty = base.sort(one);
        let nat = base.cnst("Nat", &[]);
        let rhs = base.bvar(0);
        base.inductive(
            "Nat",
            ty,
            &[("Nat.zero", nat, 0)],
            ("Nat.rec", &["u"], nat),
            &[("Nat.zero", 0, rhs)],
        );
        let setup = setup(&[("Base", &[], &base)]);
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert!(!violations.is_empty());
        assert!(violations.iter().all(|v| v.rule == "L7"), "{violations:?}");
    }

    #[test]
    fn l8_rejects_a_module_missing_from_the_manifest() {
        let (base, top) = fixture::base_and_top();
        let setup = setup(&[("Base", &[], &base), ("Top", &["Base", "Gone"], &top)]);
        let violations = link_all(&setup, View::Exported, accept, untouched);
        assert_eq!(rules(&violations), ["L8"], "{violations:?}");
        assert!(violations[0].message.contains("Gone"));
    }
}
