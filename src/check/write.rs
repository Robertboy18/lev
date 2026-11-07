//! Materialize a slice as a self-contained LeanExport 3.1.0 file.
//!
//! The file starts with the metadata line and declares the slice's units in
//! dependency order, each after the name, level and expression records it
//! uses. Records are renumbered densely and deduplicated by canonical hash,
//! so no record repeats (nanoda rejects repeats and needs continuous
//! back-references). Each declaration's universe parameters get their
//! `param` level records first, as LeanExport's `dumpUparams` writes them.
//! mdata is dropped and letE is written with nondep false, as LeanExport does
//! by default. Axiomized constants become
//! `{"axiom":{"isUnsafe":false,"levelParams":..,"name":..,"type":..}}`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::hash::{Hash, Index, param_level, zero_level};
use super::read::{
    ANONYMOUS, Component, Decl, Development, Expr, Header, Hints, Level, NameId, QuotKind, Safety,
};
use super::slice::{Role, Slice};

/// Size of a written slice file.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Written {
    pub(crate) bytes: u64,
}

/// Write `slice` to `path`.
pub(crate) fn write(
    dev: &Development,
    index: &Index,
    slice: &Slice,
    path: &Path,
) -> Result<Written> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = Writer {
        dev,
        index,
        out: Counting {
            inner: BufWriter::with_capacity(1 << 20, file),
            bytes: 0,
        },
        names: HashMap::from([(ANONYMOUS, 0)]),
        levels: HashMap::from([(zero_level(), 0)]),
        exprs: HashMap::new(),
        next_name: 1,
        next_level: 1,
        stack: Vec::new(),
    };
    let meta = &dev.modules[slice.module as usize].meta;
    writer.line(&serde_json::to_string(
        &serde_json::json!({ "meta": meta }),
    )?)?;
    for item in &slice.items {
        let module = item.module as usize;
        let unit = &dev.modules[module].units[item.unit as usize];
        writer.unit(module, unit, item.role)?;
    }
    writer.out.flush()?;
    Ok(Written {
        bytes: writer.out.bytes,
    })
}

struct Counting {
    inner: BufWriter<File>,
    bytes: u64,
}

impl Write for Counting {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(data)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct Writer<'a> {
    dev: &'a Development,
    index: &'a Index,
    out: Counting,
    /// Global name to output index.
    names: HashMap<NameId, u64>,
    /// Canonical level hash to output index.
    levels: HashMap<Hash, u64>,
    /// Canonical expression hash to output index.
    exprs: HashMap<Hash, u64>,
    next_name: u64,
    next_level: u64,
    stack: Vec<(u32, bool)>,
}

/// Output index of an already written record.
fn known<K: std::hash::Hash + Eq>(map: &HashMap<K, u64>, key: &K) -> Result<u64> {
    map.get(key)
        .copied()
        .context("a record was used before it was written")
}

impl<'a> Writer<'a> {
    fn line(&mut self, text: &str) -> Result<()> {
        self.out.write_all(text.as_bytes())?;
        self.out.write_all(b"\n")?;
        Ok(())
    }

    fn name(&mut self, id: NameId) -> Result<u64> {
        if let Some(index) = self.names.get(&id) {
            return Ok(*index);
        }
        let mut chain = Vec::new();
        let mut current = id;
        while !self.names.contains_key(&current) {
            let (pre, _) = self
                .dev
                .names
                .get(current)
                .context("a name without a prefix")?;
            chain.push(current);
            current = pre;
        }
        let dev: &'a Development = self.dev;
        for name in chain.into_iter().rev() {
            let (pre, last) = dev.names.get(name).context("a name without a prefix")?;
            let pre = known(&self.names, &pre)?;
            let index = self.next_name;
            let text = match last {
                Component::Str(text) => format!(
                    r#"{{"in":{index},"str":{{"pre":{pre},"str":{}}}}}"#,
                    serde_json::to_string(&**text)?
                ),
                Component::Num(number) => {
                    format!(r#"{{"in":{index},"num":{{"i":{number},"pre":{pre}}}}}"#)
                }
            };
            self.line(&text)?;
            self.names.insert(name, index);
            self.next_name += 1;
        }
        known(&self.names, &id)
    }

    fn level(&mut self, module: usize, root: u32) -> Result<u64> {
        let index: &'a Index = self.index;
        let dev: &'a Development = self.dev;
        let hashes = &index.modules[module].levels;
        if let Some(index) = self.levels.get(&hashes[root as usize]) {
            return Ok(*index);
        }
        let levels = &dev.modules[module].levels;
        let mut stack = vec![(root, false)];
        while let Some((level, expanded)) = stack.pop() {
            let hash = hashes[level as usize];
            if self.levels.contains_key(&hash) {
                continue;
            }
            let children: &[u32] = match &levels[level as usize] {
                Level::Zero | Level::Param(_) => &[],
                Level::Succ(l) => std::slice::from_ref(l),
                Level::Max(a, b) | Level::IMax(a, b) => &[*a, *b],
            };
            if !expanded {
                stack.push((level, true));
                stack.extend(children.iter().map(|child| (*child, false)));
                continue;
            }
            let get =
                |levels: &HashMap<Hash, u64>, child: u32| known(levels, &hashes[child as usize]);
            let index = self.next_level;
            let text = match levels[level as usize] {
                Level::Zero => bail!("the zero level has no record"),
                Level::Succ(l) => format!(r#"{{"il":{index},"succ":{}}}"#, get(&self.levels, l)?),
                Level::Max(a, b) => format!(
                    r#"{{"il":{index},"max":[{},{}]}}"#,
                    get(&self.levels, a)?,
                    get(&self.levels, b)?
                ),
                Level::IMax(a, b) => format!(
                    r#"{{"il":{index},"imax":[{},{}]}}"#,
                    get(&self.levels, a)?,
                    get(&self.levels, b)?
                ),
                Level::Param(name) => {
                    let name = self.name(name)?;
                    format!(r#"{{"il":{index},"param":{name}}}"#)
                }
            };
            self.line(&text)?;
            self.levels.insert(hash, index);
            self.next_level += 1;
        }
        known(&self.levels, &hashes[root as usize])
    }

    /// Names of the universe parameters, after their `param` level records.
    fn uparams(&mut self, params: &[NameId]) -> Result<String> {
        let mut indices = Vec::with_capacity(params.len());
        for param in params {
            let name = self.name(*param)?;
            let hash = param_level(&self.index.names[*param as usize]);
            let index = self.next_level;
            if let Entry::Vacant(slot) = self.levels.entry(hash) {
                slot.insert(index);
                self.next_level += 1;
                self.line(&format!(r#"{{"il":{index},"param":{name}}}"#))?;
            }
            indices.push(name);
        }
        Ok(list(&indices))
    }

    fn names(&mut self, names: &[NameId]) -> Result<String> {
        let indices = names
            .iter()
            .map(|name| self.name(*name))
            .collect::<Result<Vec<_>>>()?;
        Ok(list(&indices))
    }

    fn expr(&mut self, module: usize, root: u32) -> Result<u64> {
        let dev: &'a Development = self.dev;
        let index: &'a Index = self.index;
        let hashes = &index.modules[module].exprs;
        if let Some(out) = self.exprs.get(&hashes[root as usize]) {
            return Ok(*out);
        }
        let exprs = &dev.modules[module].exprs;
        let mut stack = std::mem::take(&mut self.stack);
        stack.push((root, false));
        while let Some((expr, expanded)) = stack.pop() {
            let hash = hashes[expr as usize];
            if self.exprs.contains_key(&hash) {
                continue;
            }
            let node = &exprs[expr as usize];
            if !expanded {
                match node {
                    // Same hash as its inner expression, which stands for it.
                    Expr::MData(inner) => stack.push((*inner, false)),
                    _ => {
                        stack.push((expr, true));
                        children(node, |child| stack.push((child, false)));
                    }
                }
                continue;
            }
            let text = match self.expr_record(module, node) {
                Ok(text) => text,
                Err(error) => {
                    self.stack = stack;
                    return Err(error);
                }
            };
            self.line(&text)?;
            let out = self.exprs.len() as u64;
            self.exprs.insert(hash, out);
        }
        self.stack = stack;
        known(&self.exprs, &hashes[root as usize])
    }

    fn expr_record(&mut self, module: usize, node: &Expr) -> Result<String> {
        let index: &'a Index = self.index;
        let hashes = &index.modules[module].exprs;
        let get = |exprs: &HashMap<Hash, u64>, child: u32| known(exprs, &hashes[child as usize]);
        let index = self.exprs.len();
        Ok(match node {
            Expr::BVar(i) => format!(r#"{{"bvar":{i},"ie":{index}}}"#),
            Expr::Sort(level) => {
                let level = self.level(module, *level)?;
                format!(r#"{{"ie":{index},"sort":{level}}}"#)
            }
            Expr::Const(name, us) => {
                let name = self.name(*name)?;
                let us = us
                    .iter()
                    .map(|level| self.level(module, *level))
                    .collect::<Result<Vec<_>>>()?;
                format!(
                    r#"{{"const":{{"name":{name},"us":{}}},"ie":{index}}}"#,
                    list(&us)
                )
            }
            Expr::App(f, a) => format!(
                r#"{{"app":{{"arg":{},"fn":{}}},"ie":{index}}}"#,
                get(&self.exprs, *a)?,
                get(&self.exprs, *f)?
            ),
            Expr::Lam(binder) | Expr::Pi(binder) => {
                let name = self.name(binder.name)?;
                let fields = format!(
                    r#"{{"binderInfo":"{}","body":{},"name":{name},"type":{}}}"#,
                    binder.info.as_str(),
                    get(&self.exprs, binder.body)?,
                    get(&self.exprs, binder.ty)?
                );
                if matches!(node, Expr::Lam(_)) {
                    format!(r#"{{"ie":{index},"lam":{fields}}}"#)
                } else {
                    format!(r#"{{"forallE":{fields},"ie":{index}}}"#)
                }
            }
            Expr::Let {
                name,
                ty,
                value,
                body,
            } => {
                let name = self.name(*name)?;
                format!(
                    r#"{{"ie":{index},"letE":{{"body":{},"name":{name},"nondep":false,"type":{},"value":{}}}}}"#,
                    get(&self.exprs, *body)?,
                    get(&self.exprs, *ty)?,
                    get(&self.exprs, *value)?
                )
            }
            Expr::Proj(type_name, idx, structure) => {
                let type_name = self.name(*type_name)?;
                format!(
                    r#"{{"ie":{index},"proj":{{"idx":{idx},"struct":{},"typeName":{type_name}}}}}"#,
                    get(&self.exprs, *structure)?
                )
            }
            Expr::Nat(digits) => format!(r#"{{"ie":{index},"natVal":"{digits}"}}"#),
            Expr::Str(text) => format!(
                r#"{{"ie":{index},"strVal":{}}}"#,
                serde_json::to_string(&**text)?
            ),
            Expr::MData(_) => bail!("mdata has no record of its own"),
        })
    }

    /// Name, universe parameters and type of a declaration, as JSON fields.
    fn header(&mut self, module: usize, header: &Header) -> Result<(u64, String, u64)> {
        let name = self.name(header.name)?;
        let params = self.uparams(&header.level_params)?;
        let ty = self.expr(module, header.ty)?;
        Ok((name, params, ty))
    }

    fn unit(&mut self, module: usize, unit: &Decl, role: Role) -> Result<()> {
        if role == Role::Axiom {
            let (header, _) = unit
                .single()
                .context("only single constants are axiomized")?;
            let (name, params, ty) = self.header(module, header)?;
            return self.line(&format!(
                r#"{{"axiom":{{"isUnsafe":false,"levelParams":{params},"name":{name},"type":{ty}}}}}"#
            ));
        }
        let text = match unit {
            Decl::Axiom { header, is_unsafe } => {
                let (name, params, ty) = self.header(module, header)?;
                format!(
                    r#"{{"axiom":{{"isUnsafe":{is_unsafe},"levelParams":{params},"name":{name},"type":{ty}}}}}"#
                )
            }
            Decl::Def(def) => {
                let (name, params, ty) = self.header(module, &def.header)?;
                let value = self.expr(module, def.value)?;
                let all = self.names(&def.all)?;
                let hints = match def.hints {
                    Hints::Opaque => r#""opaque""#.to_owned(),
                    Hints::Abbrev => r#""abbrev""#.to_owned(),
                    Hints::Regular(height) => format!(r#"{{"regular":{height}}}"#),
                };
                let safety = match def.safety {
                    Safety::Safe => "safe",
                    Safety::Unsafe => "unsafe",
                    Safety::Partial => "partial",
                };
                format!(
                    r#"{{"def":{{"all":{all},"hints":{hints},"levelParams":{params},"name":{name},"safety":"{safety}","type":{ty},"value":{value}}}}}"#
                )
            }
            Decl::Thm(thm) => {
                let (name, params, ty) = self.header(module, &thm.header)?;
                let value = self.expr(module, thm.value)?;
                let all = self.names(&thm.all)?;
                format!(
                    r#"{{"thm":{{"all":{all},"levelParams":{params},"name":{name},"type":{ty},"value":{value}}}}}"#
                )
            }
            Decl::Opaque(opaque) => {
                let (name, params, ty) = self.header(module, &opaque.header)?;
                let value = self.expr(module, opaque.value)?;
                let all = self.names(&opaque.all)?;
                format!(
                    r#"{{"opaque":{{"all":{all},"isUnsafe":{},"levelParams":{params},"name":{name},"type":{ty},"value":{value}}}}}"#,
                    opaque.is_unsafe
                )
            }
            Decl::Quot(values) => {
                for value in values {
                    let (name, params, ty) = self.header(module, &value.header)?;
                    let kind = match value.kind {
                        QuotKind::Type => "type",
                        QuotKind::Ctor => "ctor",
                        QuotKind::Lift => "lift",
                        QuotKind::Ind => "ind",
                    };
                    self.line(&format!(
                        r#"{{"quot":{{"kind":"{kind}","levelParams":{params},"name":{name},"type":{ty}}}}}"#
                    ))?;
                }
                return Ok(());
            }
            Decl::Inductive(block) => {
                let mut types = Vec::new();
                for t in &block.types {
                    let (name, params, ty) = self.header(module, &t.header)?;
                    let all = self.names(&t.all)?;
                    let ctors = self.names(&t.ctors)?;
                    types.push(format!(
                        r#"{{"all":{all},"ctors":{ctors},"isRec":{},"isReflexive":{},"isUnsafe":{},"levelParams":{params},"name":{name},"numIndices":{},"numNested":{},"numParams":{},"type":{ty}}}"#,
                        t.is_rec, t.is_reflexive, t.is_unsafe, t.num_indices, t.num_nested, t.num_params
                    ));
                }
                let mut ctors = Vec::new();
                for c in &block.ctors {
                    let (name, params, ty) = self.header(module, &c.header)?;
                    let induct = self.name(c.induct)?;
                    ctors.push(format!(
                        r#"{{"cidx":{},"induct":{induct},"isUnsafe":{},"levelParams":{params},"name":{name},"numFields":{},"numParams":{},"type":{ty}}}"#,
                        c.cidx, c.is_unsafe, c.num_fields, c.num_params
                    ));
                }
                let mut recs = Vec::new();
                for r in &block.recs {
                    let (name, params, ty) = self.header(module, &r.header)?;
                    let all = self.names(&r.all)?;
                    let mut rules = Vec::new();
                    for rule in &r.rules {
                        let ctor = self.name(rule.ctor)?;
                        let rhs = self.expr(module, rule.rhs)?;
                        rules.push(format!(
                            r#"{{"ctor":{ctor},"nfields":{},"rhs":{rhs}}}"#,
                            rule.nfields
                        ));
                    }
                    recs.push(format!(
                        r#"{{"all":{all},"isUnsafe":{},"k":{},"levelParams":{params},"name":{name},"numIndices":{},"numMinors":{},"numMotives":{},"numParams":{},"rules":[{}],"type":{ty}}}"#,
                        r.is_unsafe,
                        r.k,
                        r.num_indices,
                        r.num_minors,
                        r.num_motives,
                        r.num_params,
                        rules.join(",")
                    ));
                }
                format!(
                    r#"{{"inductive":{{"ctors":[{}],"recs":[{}],"types":[{}]}}}}"#,
                    ctors.join(","),
                    recs.join(","),
                    types.join(",")
                )
            }
        };
        self.line(&text)
    }
}

fn children(node: &Expr, mut push: impl FnMut(u32)) {
    match node {
        Expr::BVar(_) | Expr::Sort(_) | Expr::Const(..) | Expr::Nat(_) | Expr::Str(_) => {}
        Expr::App(f, a) => {
            push(*a);
            push(*f);
        }
        Expr::Lam(binder) | Expr::Pi(binder) => {
            push(binder.body);
            push(binder.ty);
        }
        Expr::Let {
            ty, value, body, ..
        } => {
            push(*body);
            push(*value);
            push(*ty);
        }
        Expr::Proj(_, _, structure) => push(*structure),
        Expr::MData(inner) => push(*inner),
    }
}

fn list(indices: &[u64]) -> String {
    let parts: Vec<String> = indices.iter().map(u64::to_string).collect();
    format!("[{}]", parts.join(","))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;

    use serde_json::Value;

    use super::super::fixture::{self, File};
    use super::super::hash::{Index, index};
    use super::super::read::{Development, Kind, read_export, read_plain};
    use super::super::slice::{Role, Slice, View, build};
    use super::write;

    fn develop(modules: &[(&str, &[&str], &File)]) -> (tempfile::TempDir, Development, Index) {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture::write_export(&dir.path().join("export"), modules);
        let dev = read_export(&dir.path().join("export"), 2).expect("read");
        let index = index(&dev, 2);
        (dir, dev, index)
    }

    /// Write `slice`, read it back as a plain export and compare every entry.
    fn round_trip(
        dir: &std::path::Path,
        dev: &Development,
        index: &Index,
        slice: &Slice,
    ) -> String {
        let path = dir.join(format!("slice-{}.ndjson", slice.view.as_str()));
        let written = write(dev, index, slice, &path).expect("write");
        let text = fs::read_to_string(&path).expect("read back");
        assert_eq!(written.bytes, text.len() as u64);
        let first: Value = serde_json::from_str(text.lines().next().expect("line")).expect("json");
        assert!(first.get("meta").is_some(), "metadata comes first");

        let parsed = read_plain(&path).expect("the slice parses");
        let parsed_index = super::super::hash::index(&parsed, 1);
        assert_eq!(parsed.owners.len(), slice.entries.len());
        for entry in &slice.entries {
            let name = dev.names.display(entry.name);
            let id = parsed
                .names
                .lookup(&dev.names.components(entry.name))
                .expect("name in slice file");
            let owner = parsed.owners[&id];
            let unit = &parsed_index.modules[0].units[owner.unit as usize];
            if entry.role == Role::Axiom {
                assert_eq!(owner.kind, Kind::Axiom, "{name}");
                assert_eq!(unit.ty, entry.hash, "{name}");
            } else {
                assert_eq!(owner.kind, entry.kind, "{name}");
                assert_eq!(unit.full, entry.hash, "{name}");
            }
        }

        // No record repeats.
        let module = &parsed_index.modules[0];
        assert_eq!(
            module.exprs.iter().collect::<HashSet<_>>().len(),
            module.exprs.len()
        );
        assert_eq!(
            module.levels.iter().collect::<HashSet<_>>().len(),
            module.levels.len()
        );
        let name_records = text
            .lines()
            .filter(|line| line.starts_with(r#"{"in":"#))
            .count();
        assert_eq!(name_records + 1, parsed.names.count());

        // Every universe parameter has its `param` level record first.
        let mut params = HashSet::new();
        for line in text.lines().skip(1) {
            let record: Value = serde_json::from_str(line).expect("json");
            if let Some(name) = record.get("param") {
                params.insert(name.as_u64().expect("index"));
            }
            for kind in ["axiom", "def", "thm", "opaque", "quot", "inductive"] {
                let Some(decl) = record.get(kind) else {
                    continue;
                };
                let members: Vec<&Value> = if kind == "inductive" {
                    ["types", "ctors", "recs"]
                        .iter()
                        .flat_map(|key| decl[*key].as_array().into_iter().flatten())
                        .collect()
                } else {
                    vec![decl]
                };
                for member in members {
                    for param in member["levelParams"].as_array().expect("params") {
                        assert!(params.contains(&param.as_u64().expect("index")));
                    }
                }
            }
        }
        text
    }

    #[test]
    fn materialized_slices_round_trip_in_every_view() {
        let (dir, dev, index) = {
            let (base, top) = fixture::base_and_top();
            develop(&[("Base", &[], &base), ("Top", &["Base"], &top)])
        };
        let pinned = fixture::pinned(&dev);
        for module in 0..2 {
            for view in View::ALL {
                let slice = build(&dev, &index, &pinned, module, view).expect("slice");
                round_trip(dir.path(), &dev, &index, &slice);
            }
        }
    }

    #[test]
    fn mdata_is_dropped_and_shared_nodes_are_written_once() {
        let mut base = File::new();
        let nat = base.cnst("Nat", &[]);
        let x = base.bvar(0);
        let wrapped = base.mdata(x);
        // Alpha-equivalent to the plain identity: written once, under the first name.
        let id = base.lam("x", nat, x);
        let same = base.binder("lam", "renamed", "implicit", nat, wrapped);
        let ty = base.pi("n", nat, nat);
        fixture::nat(&mut base);
        base.def("Base.a", &[], ty, id);
        base.def("Base.b", &[], ty, same);
        let (dir, dev, index) = develop(&[("Base", &[], &base)]);
        let slice = build(&dev, &index, &fixture::pinned(&dev), 0, View::Exported).expect("slice");
        let text = round_trip(dir.path(), &dev, &index, &slice);
        assert!(!text.contains("mdata"));
        assert_eq!(text.matches(r#""lam":"#).count(), 1);
    }

    #[test]
    fn eq_comes_before_the_quotient_package() {
        let mut base = File::new();
        let prop = base.sort(0);
        let one = base.succ(0);
        let ty1 = base.sort(one);
        let eq_ty = base.pi("a", ty1, prop);
        let eq = base.cnst("Eq", &[]);
        let rhs = base.bvar(0);
        base.inductive(
            "Eq",
            eq_ty,
            &[("Eq.refl", eq, 0)],
            ("Eq.rec", &["u"], eq),
            &[("Eq.refl", 0, rhs)],
        );
        let lift_ty = base.pi("f", eq, eq);
        base.quot([
            ("Quot", ty1, "type"),
            ("Quot.mk", ty1, "ctor"),
            ("Quot.lift", lift_ty, "lift"),
            ("Quot.ind", prop, "ind"),
        ]);
        let mut top = File::new();
        let lift = top.cnst("Quot.lift", &[]);
        let prop = top.sort(0);
        top.def("Top.q", &[], prop, lift);
        let (dir, dev, index) = develop(&[("Base", &[], &base), ("Top", &["Base"], &top)]);
        let slice = build(&dev, &index, &fixture::pinned(&dev), 1, View::Exported).expect("slice");
        let roles: Vec<_> = slice
            .entries
            .iter()
            .map(|e| (dev.names.display(e.name), e.role))
            .collect();
        assert!(roles.contains(&("Quot.ind".to_owned(), Role::Full)));
        assert!(roles.contains(&("Eq.refl".to_owned(), Role::Full)));
        let text = round_trip(dir.path(), &dev, &index, &slice);
        let position = |needle: &str| text.lines().position(|line| line.starts_with(needle));
        let eq = position(r#"{"inductive":"#).expect("Eq block");
        let quot = position(r#"{"quot":"#).expect("quotient records");
        assert!(eq < quot);
        assert_eq!(text.matches(r#"{"quot":"#).count(), 4);
    }
}
