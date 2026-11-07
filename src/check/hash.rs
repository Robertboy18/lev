//! Canonical SHA-256 Merkle hashes and references of declarations.
//!
//! Every node is hashed over a tagged encoding of its fields and its
//! children's hashes. Hashes ignore what kernels ignore and what exports vary
//! on (lean4export#58): binder names, binder info, mdata, the letE nondep
//! flag, file-local indices, record order and JSON key order. They keep
//! universe-parameter names and order, constant names, literal values,
//! projection indices and every declaration field. A name is hashed
//! component by component, so `«A.b»` and `A.b` hash apart.
//!
//! Each unit gets a full hash (FH) over all of its fields; single constants
//! also get a type hash (TH) over name, universe parameters and type, which
//! is what an axiomized copy shows. Inductive blocks and the quotient package
//! are hashed as units.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use super::parallel_map;
use super::read::{
    Component, Decl, Development, Expr, Header, Hints, Level, Module, NameId, Names, QuotKind,
    Safety,
};

pub(crate) type Hash = [u8; 32];

/// Hashes and references of a whole development.
pub(crate) struct Index {
    /// Hash of every interned name, by `NameId`.
    pub(crate) names: Vec<Hash>,
    pub(crate) modules: Vec<ModuleIndex>,
}

pub(crate) struct ModuleIndex {
    pub(crate) levels: Vec<Hash>,
    pub(crate) exprs: Vec<Hash>,
    pub(crate) units: Vec<UnitIndex>,
}

pub(crate) struct UnitIndex {
    /// FH: the whole unit.
    pub(crate) full: Hash,
    /// TH of a single-constant unit; zero for blocks and the quotient package.
    pub(crate) ty: Hash,
    /// Constants the type mentions (single-constant units only).
    pub(crate) type_refs: Vec<NameId>,
    /// Constants the type and value mention, other than the unit's own.
    pub(crate) refs: Vec<NameId>,
}

/// SHA-256 over a tag and a sequence of fixed-width or length-prefixed fields.
pub(crate) struct Node(Sha256);

impl Node {
    pub(crate) fn new(tag: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update([tag.len() as u8]);
        digest.update(tag.as_bytes());
        Self(digest)
    }

    pub(crate) fn hash(&mut self, hash: &Hash) -> &mut Self {
        self.0.update(hash);
        self
    }

    pub(crate) fn number(&mut self, number: u64) -> &mut Self {
        self.0.update(number.to_le_bytes());
        self
    }

    pub(crate) fn flag(&mut self, flag: bool) -> &mut Self {
        self.0.update([u8::from(flag)]);
        self
    }

    pub(crate) fn text(&mut self, text: &str) -> &mut Self {
        self.number(text.len() as u64);
        self.0.update(text.as_bytes());
        self
    }

    pub(crate) fn finish(self) -> Hash {
        self.0.finalize().into()
    }
}

pub(crate) fn zero_level() -> Hash {
    Node::new("level.zero").finish()
}

pub(crate) fn param_level(name: &Hash) -> Hash {
    let mut node = Node::new("level.param");
    node.hash(name);
    node.finish()
}

/// Hash every name, level, expression and unit of `dev`, modules in parallel.
pub(crate) fn index(dev: &Development, jobs: usize) -> Index {
    let names = name_hashes(&dev.names);
    let extra = Extra::new(dev);
    let modules = parallel_map(&dev.modules, jobs, |_, module| {
        index_module(module, &names, &extra)
    });
    Index { names, modules }
}

fn name_hashes(names: &Names) -> Vec<Hash> {
    let mut out = Vec::with_capacity(names.count());
    out.push(Node::new("name.anonymous").finish());
    for (pre, last) in names.iter() {
        let mut node;
        match last {
            Component::Str(text) => {
                node = Node::new("name.str");
                node.hash(&out[pre as usize]).text(text);
            }
            Component::Num(number) => {
                node = Node::new("name.num");
                node.hash(&out[pre as usize]).number(*number);
            }
        }
        out.push(node.finish());
    }
    out
}

/// Constants that some records depend on without naming them, as LeanExport's
/// `dumpNatDeps`, `dumpStrDeps` and quotient case add them: only those that exist.
struct Extra {
    nat: Option<NameId>,
    strings: Vec<NameId>,
    eq: Option<NameId>,
}

impl Extra {
    fn new(dev: &Development) -> Self {
        let owned = |name: &str| {
            dev.names
                .find(name)
                .filter(|id| dev.owners.contains_key(id))
        };
        Self {
            nat: owned("Nat"),
            strings: ["Char.ofNat", "String.ofList"]
                .into_iter()
                .filter_map(owned)
                .collect(),
            eq: owned("Eq"),
        }
    }
}

fn index_module(module: &Module, names: &[Hash], extra: &Extra) -> ModuleIndex {
    let levels = level_hashes(module, names);
    let exprs = expr_hashes(module, names, &levels);
    let mut refs = Refs::new(module, extra);
    let units = module
        .units
        .iter()
        .map(|unit| {
            let own: HashSet<NameId> = unit.members().into_iter().map(|(n, _)| n).collect();
            let full = full_hash(unit, names, &exprs);
            match unit.single() {
                Some((header, value)) => {
                    let mut roots = vec![header.ty];
                    let type_refs = refs.collect(&roots, &own, false);
                    roots.extend(value);
                    UnitIndex {
                        full,
                        ty: type_hash(header, names, &exprs),
                        type_refs,
                        refs: refs.collect(&roots, &own, false),
                    }
                }
                None => {
                    let (roots, quot): (Vec<u32>, bool) = match unit {
                        Decl::Quot(values) => (values.iter().map(|v| v.header.ty).collect(), true),
                        Decl::Inductive(block) => {
                            let mut roots: Vec<u32> =
                                block.types.iter().map(|t| t.header.ty).collect();
                            roots.extend(block.ctors.iter().map(|c| c.header.ty));
                            for rec in &block.recs {
                                roots.push(rec.header.ty);
                                roots.extend(rec.rules.iter().map(|rule| rule.rhs));
                            }
                            (roots, false)
                        }
                        _ => (Vec::new(), false),
                    };
                    UnitIndex {
                        full,
                        ty: [0; 32],
                        type_refs: Vec::new(),
                        refs: refs.collect(&roots, &own, quot),
                    }
                }
            }
        })
        .collect();
    ModuleIndex {
        levels,
        exprs,
        units,
    }
}

fn level_hashes(module: &Module, names: &[Hash]) -> Vec<Hash> {
    let mut out: Vec<Hash> = Vec::with_capacity(module.levels.len());
    for level in &module.levels {
        let hash = match *level {
            Level::Zero => zero_level(),
            Level::Succ(l) => {
                let mut node = Node::new("level.succ");
                node.hash(&out[l as usize]);
                node.finish()
            }
            Level::Max(a, b) => {
                let mut node = Node::new("level.max");
                node.hash(&out[a as usize]).hash(&out[b as usize]);
                node.finish()
            }
            Level::IMax(a, b) => {
                let mut node = Node::new("level.imax");
                node.hash(&out[a as usize]).hash(&out[b as usize]);
                node.finish()
            }
            Level::Param(name) => param_level(&names[name as usize]),
        };
        out.push(hash);
    }
    out
}

/// Children always precede parents in a module's arena, so one pass suffices.
fn expr_hashes(module: &Module, names: &[Hash], levels: &[Hash]) -> Vec<Hash> {
    let mut out: Vec<Hash> = Vec::with_capacity(module.exprs.len());
    for expr in &module.exprs {
        let child = |index: u32| &out[index as usize];
        let hash = match expr {
            Expr::BVar(index) => {
                let mut node = Node::new("expr.bvar");
                node.number(*index);
                node.finish()
            }
            Expr::Sort(level) => {
                let mut node = Node::new("expr.sort");
                node.hash(&levels[*level as usize]);
                node.finish()
            }
            Expr::Const(name, us) => {
                let mut node = Node::new("expr.const");
                node.hash(&names[*name as usize]).number(us.len() as u64);
                for level in us.iter() {
                    node.hash(&levels[*level as usize]);
                }
                node.finish()
            }
            Expr::App(f, a) => {
                let mut node = Node::new("expr.app");
                node.hash(child(*f)).hash(child(*a));
                node.finish()
            }
            Expr::Lam(binder) => {
                let mut node = Node::new("expr.lam");
                node.hash(child(binder.ty)).hash(child(binder.body));
                node.finish()
            }
            Expr::Pi(binder) => {
                let mut node = Node::new("expr.forallE");
                node.hash(child(binder.ty)).hash(child(binder.body));
                node.finish()
            }
            Expr::Let {
                ty, value, body, ..
            } => {
                let mut node = Node::new("expr.letE");
                node.hash(child(*ty)).hash(child(*value)).hash(child(*body));
                node.finish()
            }
            Expr::Proj(name, idx, structure) => {
                let mut node = Node::new("expr.proj");
                node.hash(&names[*name as usize])
                    .number(*idx)
                    .hash(child(*structure));
                node.finish()
            }
            Expr::Nat(digits) => {
                let mut node = Node::new("expr.natVal");
                node.text(digits);
                node.finish()
            }
            Expr::Str(text) => {
                let mut node = Node::new("expr.strVal");
                node.text(text);
                node.finish()
            }
            Expr::MData(inner) => *child(*inner),
        };
        out.push(hash);
    }
    out
}

fn header(node: &mut Node, header: &Header, names: &[Hash], exprs: &[Hash]) {
    node.hash(&names[header.name as usize])
        .number(header.level_params.len() as u64);
    for param in &header.level_params {
        node.hash(&names[*param as usize]);
    }
    node.hash(&exprs[header.ty as usize]);
}

fn name_list(node: &mut Node, list: &[NameId], names: &[Hash]) {
    node.number(list.len() as u64);
    for name in list {
        node.hash(&names[*name as usize]);
    }
}

/// TH: what an axiomized copy of the constant shows.
pub(crate) fn type_hash(h: &Header, names: &[Hash], exprs: &[Hash]) -> Hash {
    let mut node = Node::new("type");
    header(&mut node, h, names, exprs);
    node.finish()
}

fn full_hash(unit: &Decl, names: &[Hash], exprs: &[Hash]) -> Hash {
    let expr = |index: u32| &exprs[index as usize];
    match unit {
        Decl::Axiom {
            header: h,
            is_unsafe,
        } => {
            let mut node = Node::new("axiom");
            header(&mut node, h, names, exprs);
            node.flag(*is_unsafe);
            node.finish()
        }
        Decl::Def(def) => {
            let mut node = Node::new("def");
            header(&mut node, &def.header, names, exprs);
            node.hash(expr(def.value));
            match def.hints {
                Hints::Opaque => node.number(0),
                Hints::Abbrev => node.number(1),
                Hints::Regular(height) => node.number(2).number(u64::from(height)),
            };
            node.number(match def.safety {
                Safety::Safe => 0,
                Safety::Unsafe => 1,
                Safety::Partial => 2,
            });
            name_list(&mut node, &def.all, names);
            node.finish()
        }
        Decl::Thm(thm) => {
            let mut node = Node::new("thm");
            header(&mut node, &thm.header, names, exprs);
            node.hash(expr(thm.value));
            name_list(&mut node, &thm.all, names);
            node.finish()
        }
        Decl::Opaque(opaque) => {
            let mut node = Node::new("opaque");
            header(&mut node, &opaque.header, names, exprs);
            node.hash(expr(opaque.value)).flag(opaque.is_unsafe);
            name_list(&mut node, &opaque.all, names);
            node.finish()
        }
        Decl::Quot(values) => {
            let mut node = Node::new("quot");
            node.number(values.len() as u64);
            for value in values {
                header(&mut node, &value.header, names, exprs);
                node.number(match value.kind {
                    QuotKind::Type => 0,
                    QuotKind::Ctor => 1,
                    QuotKind::Lift => 2,
                    QuotKind::Ind => 3,
                });
            }
            node.finish()
        }
        Decl::Inductive(block) => {
            let mut node = Node::new("inductive");
            node.number(block.types.len() as u64);
            for ty in &block.types {
                header(&mut node, &ty.header, names, exprs);
                node.number(ty.num_params).number(ty.num_indices);
                name_list(&mut node, &ty.all, names);
                name_list(&mut node, &ty.ctors, names);
                node.number(ty.num_nested)
                    .flag(ty.is_rec)
                    .flag(ty.is_reflexive)
                    .flag(ty.is_unsafe);
            }
            node.number(block.ctors.len() as u64);
            for ctor in &block.ctors {
                header(&mut node, &ctor.header, names, exprs);
                node.hash(&names[ctor.induct as usize])
                    .number(ctor.cidx)
                    .number(ctor.num_params)
                    .number(ctor.num_fields)
                    .flag(ctor.is_unsafe);
            }
            // LeanExport lists recursors in NameSet order; sort them by name hash.
            let mut recs: Vec<_> = block.recs.iter().collect();
            recs.sort_by(|a, b| names[a.header.name as usize].cmp(&names[b.header.name as usize]));
            node.number(recs.len() as u64);
            for rec in recs {
                header(&mut node, &rec.header, names, exprs);
                name_list(&mut node, &rec.all, names);
                node.number(rec.num_params)
                    .number(rec.num_indices)
                    .number(rec.num_motives)
                    .number(rec.num_minors)
                    .number(rec.rules.len() as u64);
                for rule in &rec.rules {
                    node.hash(&names[rule.ctor as usize])
                        .number(rule.nfields)
                        .hash(expr(rule.rhs));
                }
                node.flag(rec.k).flag(rec.is_unsafe);
            }
            node.finish()
        }
    }
}

/// Collects the constants an expression DAG mentions, in first-occurrence order.
struct Refs<'m> {
    module: &'m Module,
    extra: &'m Extra,
    stamps: Vec<u32>,
    stamp: u32,
    stack: Vec<u32>,
    seen: HashSet<NameId>,
}

impl<'m> Refs<'m> {
    fn new(module: &'m Module, extra: &'m Extra) -> Self {
        Self {
            module,
            extra,
            stamps: vec![0; module.exprs.len()],
            stamp: 0,
            stack: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// `const` names and projection structure names under `roots`, plus the
    /// constants literals need (and `Eq` for the quotient package).
    fn collect(&mut self, roots: &[u32], own: &HashSet<NameId>, quot: bool) -> Vec<NameId> {
        self.stamp += 1;
        self.seen.clear();
        let mut out = Vec::new();
        let (mut nat, mut string) = (false, false);
        self.stack.extend(roots.iter().rev());
        while let Some(index) = self.stack.pop() {
            let slot = &mut self.stamps[index as usize];
            if *slot == self.stamp {
                continue;
            }
            *slot = self.stamp;
            let mut add = |name: NameId| {
                if !own.contains(&name) && self.seen.insert(name) {
                    out.push(name);
                }
            };
            match &self.module.exprs[index as usize] {
                Expr::BVar(_) | Expr::Sort(_) => {}
                Expr::Const(name, _) => add(*name),
                Expr::Proj(name, _, structure) => {
                    add(*name);
                    self.stack.push(*structure);
                }
                Expr::App(f, a) => self.stack.extend([*a, *f]),
                Expr::Lam(binder) | Expr::Pi(binder) => {
                    self.stack.extend([binder.body, binder.ty]);
                }
                Expr::Let {
                    ty, value, body, ..
                } => self.stack.extend([*body, *value, *ty]),
                Expr::MData(inner) => self.stack.push(*inner),
                Expr::Nat(_) => nat = true,
                Expr::Str(_) => string = true,
            }
        }
        let mut extra = Vec::new();
        if nat {
            extra.extend(self.extra.nat);
        }
        if string {
            extra.extend(&self.extra.strings);
        }
        if quot {
            extra.extend(self.extra.eq);
        }
        for name in extra {
            if !own.contains(&name) && self.seen.insert(name) {
                out.push(name);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::fixture::{self, File};
    use super::super::read::{Development, read_plain};
    use super::{Index, UnitIndex, index};

    fn indexed(file: &File) -> (Development, Index) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plain.ndjson");
        fs::write(&path, file.plain()).expect("write");
        let dev = read_plain(&path).expect("read");
        let index = index(&dev, 1);
        (dev, index)
    }

    fn unit<'a>(dev: &Development, index: &'a Index, name: &str) -> &'a UnitIndex {
        let owner = dev.owners[&dev.names.find(name).expect("name")];
        &index.modules[owner.module as usize].units[owner.unit as usize]
    }

    fn names(dev: &Development, refs: &[u32]) -> Vec<String> {
        refs.iter().map(|name| dev.names.display(*name)).collect()
    }

    /// `def D : Nat → Nat := fun (binder : Nat) => binder` with the given binder info.
    fn identity(binder: &str, info: &str, params: &[&str]) -> File {
        let mut f = File::new();
        let us: Vec<u64> = params.iter().map(|p| f.param(p)).collect();
        let nat = f.cnst("Nat", &us);
        let ty = f.binder("forallE", binder, info, nat, nat);
        let x = f.bvar(0);
        let value = f.binder("lam", binder, info, nat, x);
        f.def("D", params, ty, value);
        f
    }

    #[test]
    fn binder_names_and_info_do_not_change_hashes() {
        let (a, ai) = indexed(&identity("x", "default", &[]));
        let (b, bi) = indexed(&identity("y", "instImplicit", &[]));
        assert_eq!(unit(&a, &ai, "D").full, unit(&b, &bi, "D").full);
        assert_eq!(unit(&a, &ai, "D").ty, unit(&b, &bi, "D").ty);
    }

    #[test]
    fn universe_parameter_names_change_hashes() {
        let (a, ai) = indexed(&identity("x", "default", &["u"]));
        let (b, bi) = indexed(&identity("x", "default", &["v"]));
        assert_ne!(unit(&a, &ai, "D").full, unit(&b, &bi, "D").full);
        assert_ne!(unit(&a, &ai, "D").ty, unit(&b, &bi, "D").ty);
    }

    #[test]
    fn mdata_nondep_indices_and_record_order_do_not_change_hashes() {
        let build = |wrapped: bool, nondep: bool, unrelated_first: bool| {
            let mut f = File::new();
            let prop = f.sort(0);
            if unrelated_first {
                let one = f.succ(0);
                let ty = f.sort(one);
                f.axiom("Unrelated", &[], ty);
            }
            let nat = f.cnst("Nat", &[]);
            let x = f.bvar(0);
            let body = f.let_in("y", nat, x, x, nondep);
            let body = if wrapped { f.mdata(body) } else { body };
            let value = f.lam("x", nat, body);
            let ty = f.pi("x", nat, nat);
            f.def("D", &[], ty, value);
            if !unrelated_first {
                f.axiom("Unrelated", &[], prop);
            }
            f
        };
        let (a, ai) = indexed(&build(false, false, false));
        let (b, bi) = indexed(&build(true, true, true));
        assert_eq!(unit(&a, &ai, "D").full, unit(&b, &bi, "D").full);
    }

    #[test]
    fn names_hash_component_by_component() {
        let build = |name: &str| {
            let mut f = File::new();
            let ty = fixture::prop_id(&mut f);
            f.axiom(name, &[], ty);
            f
        };
        let (a, ai) = indexed(&build("Classical.choice"));
        let (b, bi) = indexed(&build("«Classical.choice»"));
        let escaped = b
            .names
            .lookup(&[super::super::read::Component::Str(
                "Classical.choice".into(),
            )])
            .expect("name");
        let dotted = a.names.find("Classical.choice").expect("name");
        assert_ne!(ai.names[dotted as usize], bi.names[escaped as usize]);
        let full = |dev: &Development, index: &Index, id: u32| {
            let owner = dev.owners[&id];
            index.modules[owner.module as usize].units[owner.unit as usize].full
        };
        assert_ne!(full(&a, &ai, dotted), full(&b, &bi, escaped));
    }

    #[test]
    fn statements_and_proofs_have_separate_hashes() {
        let build = |proof: &str| {
            let mut f = File::new();
            let ty = fixture::prop_id(&mut f);
            let value = f.cnst(proof, &[]);
            f.thm("T", &[], ty, value);
            f
        };
        let (a, ai) = indexed(&build("ProofA"));
        let (b, bi) = indexed(&build("ProofB"));
        assert_eq!(unit(&a, &ai, "T").ty, unit(&b, &bi, "T").ty);
        assert_ne!(unit(&a, &ai, "T").full, unit(&b, &bi, "T").full);
    }

    #[test]
    fn references_cover_types_values_projections_and_literals() {
        let mut f = File::new();
        fixture::nat(&mut f);
        let prop = f.sort(0);
        f.axiom("Char.ofNat", &[], prop);
        f.axiom("String.ofList", &[], prop);
        let a = f.cnst("Foo.a", &[]);
        let five = f.nat("5");
        let ty = f.app(a, five);
        let b = f.cnst("Foo.b", &[]);
        let text = f.string("s");
        let proj = f.proj("Foo.S", 0, text);
        let value = f.app(b, proj);
        f.thm("Foo.t", &[], ty, value);
        let (dev, index) = indexed(&f);
        let t = unit(&dev, &index, "Foo.t");
        assert_eq!(names(&dev, &t.type_refs), ["Foo.a", "Nat"]);
        let mut refs = names(&dev, &t.refs);
        refs.sort();
        assert_eq!(
            refs,
            [
                "Char.ofNat",
                "Foo.S",
                "Foo.a",
                "Foo.b",
                "Nat",
                "String.ofList"
            ]
        );
        // A block does not reference its own members.
        let nat = unit(&dev, &index, "Nat");
        assert!(names(&dev, &nat.refs).is_empty());
    }
}
