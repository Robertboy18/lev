//! Hand-written LeanExport fixtures for unit tests.
//!
//! [`File`] writes records the way LeanExport does: dense indices, sorted
//! keys, `param` levels before a declaration's universe parameters. The
//! declarations need not type-check; unit tests never run a kernel.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::link::{Pinned, Pins};
use super::read::{Development, EXPORT_FORMAT, LEAN_EXPORT_VERSION, NameId, read_plain};

pub(crate) const GITHASH: &str = "0000000000000000000000000000000000000000";

pub(crate) fn meta() -> serde_json::Value {
    serde_json::json!({
        "exporter": {"name": "lean4export", "version": LEAN_EXPORT_VERSION},
        "format": {"version": LEAN_EXPORT_VERSION},
        "lean": {"githash": GITHASH, "version": "0.0.0"},
    })
}

fn json(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn list(items: &[u64]) -> String {
    let parts: Vec<String> = items.iter().map(u64::to_string).collect();
    format!("[{}]", parts.join(","))
}

/// The components of a name written with dots and `«»`, each with the
/// length of the prefix that ends with it.
fn components(name: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start <= name.len() {
        let rest = &name[start..];
        let (part, length) = match rest.strip_prefix('«') {
            Some(quoted) => {
                let close = quoted.find('»').expect("a closing »");
                (&quoted[..close], '«'.len_utf8() + close + '»'.len_utf8())
            }
            None => {
                let end = rest.find('.').unwrap_or(rest.len());
                (&rest[..end], end)
            }
        };
        out.push((part, start + length));
        start += length + 1;
    }
    out
}

/// One export file under construction.
#[derive(Default)]
pub(crate) struct File {
    lines: Vec<String>,
    names: HashMap<String, u64>,
    levels: HashMap<String, u64>,
    exprs: HashMap<String, u64>,
    hidden: Vec<u64>,
}

impl File {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The index of `name`, written as Lean writes names: string components
    /// separated by dots, a component that is not an identifier in `«»`.
    pub(crate) fn name(&mut self, name: &str) -> u64 {
        let mut pre = 0;
        for (part, end) in components(name) {
            let prefix = &name[..end];
            if let Some(index) = self.names.get(prefix) {
                pre = *index;
                continue;
            }
            let index = self.names.len() as u64 + 1;
            self.lines.push(format!(
                r#"{{"in":{index},"str":{{"pre":{pre},"str":{}}}}}"#,
                json(part)
            ));
            self.names.insert(prefix.to_owned(), index);
            pre = index;
        }
        pre
    }

    /// `pre.i`, a numeric name component.
    pub(crate) fn num_name(&mut self, pre: &str, i: u64) -> u64 {
        let key = format!("{pre}#{i}");
        if let Some(index) = self.names.get(&key) {
            return *index;
        }
        let pre = self.name(pre);
        let index = self.names.len() as u64 + 1;
        self.lines
            .push(format!(r#"{{"in":{index},"num":{{"i":{i},"pre":{pre}}}}}"#));
        self.names.insert(key, index);
        index
    }

    fn level(&mut self, payload: String) -> u64 {
        if let Some(index) = self.levels.get(&payload) {
            return *index;
        }
        let index = self.levels.len() as u64 + 1;
        self.lines.push(format!(r#"{{"il":{index},{payload}}}"#));
        self.levels.insert(payload, index);
        index
    }

    pub(crate) fn param(&mut self, name: &str) -> u64 {
        let name = self.name(name);
        self.level(format!(r#""param":{name}"#))
    }

    pub(crate) fn succ(&mut self, level: u64) -> u64 {
        self.level(format!(r#""succ":{level}"#))
    }

    pub(crate) fn max(&mut self, a: u64, b: u64) -> u64 {
        self.level(format!(r#""max":[{a},{b}]"#))
    }

    pub(crate) fn imax(&mut self, a: u64, b: u64) -> u64 {
        self.level(format!(r#""imax":[{a},{b}]"#))
    }

    /// An expression record; `payload` is `"key":value` and is sorted with `ie`.
    fn expr(&mut self, key: &str, value: String) -> u64 {
        let payload = format!(r#""{key}":{value}"#);
        if let Some(index) = self.exprs.get(&payload) {
            return *index;
        }
        let index = self.exprs.len() as u64;
        let line = if key < "ie" {
            format!(r#"{{{payload},"ie":{index}}}"#)
        } else {
            format!(r#"{{"ie":{index},{payload}}}"#)
        };
        self.lines.push(line);
        self.exprs.insert(payload, index);
        index
    }

    pub(crate) fn bvar(&mut self, index: u64) -> u64 {
        self.expr("bvar", index.to_string())
    }

    pub(crate) fn sort(&mut self, level: u64) -> u64 {
        self.expr("sort", level.to_string())
    }

    pub(crate) fn cnst(&mut self, name: &str, us: &[u64]) -> u64 {
        let name = self.name(name);
        self.expr("const", format!(r#"{{"name":{name},"us":{}}}"#, list(us)))
    }

    pub(crate) fn app(&mut self, f: u64, a: u64) -> u64 {
        self.expr("app", format!(r#"{{"arg":{a},"fn":{f}}}"#))
    }

    pub(crate) fn binder(&mut self, key: &str, name: &str, info: &str, ty: u64, body: u64) -> u64 {
        let name = self.name(name);
        self.expr(
            key,
            format!(r#"{{"binderInfo":"{info}","body":{body},"name":{name},"type":{ty}}}"#),
        )
    }

    pub(crate) fn pi(&mut self, name: &str, ty: u64, body: u64) -> u64 {
        self.binder("forallE", name, "default", ty, body)
    }

    pub(crate) fn lam(&mut self, name: &str, ty: u64, body: u64) -> u64 {
        self.binder("lam", name, "default", ty, body)
    }

    pub(crate) fn let_in(
        &mut self,
        name: &str,
        ty: u64,
        value: u64,
        body: u64,
        nondep: bool,
    ) -> u64 {
        let name = self.name(name);
        self.expr(
            "letE",
            format!(
                r#"{{"body":{body},"name":{name},"nondep":{nondep},"type":{ty},"value":{value}}}"#
            ),
        )
    }

    pub(crate) fn proj(&mut self, type_name: &str, idx: u64, structure: u64) -> u64 {
        let type_name = self.name(type_name);
        self.expr(
            "proj",
            format!(r#"{{"idx":{idx},"struct":{structure},"typeName":{type_name}}}"#),
        )
    }

    pub(crate) fn nat(&mut self, digits: &str) -> u64 {
        self.expr("natVal", json(digits))
    }

    pub(crate) fn string(&mut self, text: &str) -> u64 {
        self.expr("strVal", json(text))
    }

    pub(crate) fn mdata(&mut self, inner: u64) -> u64 {
        self.expr("mdata", format!(r#"{{"data":{{"k":"v"}},"expr":{inner}}}"#))
    }

    /// Name indices of universe parameters, after their `param` levels.
    fn uparams(&mut self, params: &[&str]) -> String {
        let names: Vec<u64> = params.iter().map(|param| self.name(param)).collect();
        for param in params {
            self.param(param);
        }
        list(&names)
    }

    fn names(&mut self, names: &[&str]) -> String {
        let indices: Vec<u64> = names.iter().map(|name| self.name(name)).collect();
        list(&indices)
    }

    pub(crate) fn axiom(&mut self, name: &str, params: &[&str], ty: u64) {
        let n = self.name(name);
        let params = self.uparams(params);
        self.lines.push(format!(
            r#"{{"axiom":{{"isUnsafe":false,"levelParams":{params},"name":{n},"type":{ty}}}}}"#
        ));
    }

    pub(crate) fn def(&mut self, name: &str, params: &[&str], ty: u64, value: u64) {
        let n = self.name(name);
        let params = self.uparams(params);
        let all = self.names(&[name]);
        self.lines.push(format!(
            r#"{{"def":{{"all":{all},"hints":{{"regular":1}},"levelParams":{params},"name":{n},"safety":"safe","type":{ty},"value":{value}}}}}"#
        ));
    }

    pub(crate) fn thm(&mut self, name: &str, params: &[&str], ty: u64, value: u64) {
        let n = self.name(name);
        let params = self.uparams(params);
        let all = self.names(&[name]);
        self.lines.push(format!(
            r#"{{"thm":{{"all":{all},"levelParams":{params},"name":{n},"type":{ty},"value":{value}}}}}"#
        ));
    }

    pub(crate) fn opaque(&mut self, name: &str, params: &[&str], ty: u64, value: u64) {
        let n = self.name(name);
        let params = self.uparams(params);
        let all = self.names(&[name]);
        self.lines.push(format!(
            r#"{{"opaque":{{"all":{all},"isUnsafe":false,"levelParams":{params},"name":{n},"type":{ty},"value":{value}}}}}"#
        ));
    }

    /// One inductive type without parameters or indices, with its recursor.
    /// `ctors` are (name, type, fields); `rules` are (constructor, fields, rhs).
    pub(crate) fn inductive(
        &mut self,
        name: &str,
        ty: u64,
        ctors: &[(&str, u64, u64)],
        rec: (&str, &[&str], u64),
        rules: &[(&str, u64, u64)],
    ) {
        let n = self.name(name);
        let all = self.names(&[name]);
        let ctor_names: Vec<&str> = ctors.iter().map(|(ctor, _, _)| *ctor).collect();
        let ctor_list = self.names(&ctor_names);
        let ty_json = format!(
            r#"{{"all":{all},"ctors":{ctor_list},"isRec":false,"isReflexive":false,"isUnsafe":false,"levelParams":[],"name":{n},"numIndices":0,"numNested":0,"numParams":0,"type":{ty}}}"#
        );
        let mut ctor_json = Vec::new();
        for (cidx, (ctor, ctor_ty, fields)) in ctors.iter().enumerate() {
            let c = self.name(ctor);
            ctor_json.push(format!(
                r#"{{"cidx":{cidx},"induct":{n},"isUnsafe":false,"levelParams":[],"name":{c},"numFields":{fields},"numParams":0,"type":{ctor_ty}}}"#
            ));
        }
        let (rec_name, rec_params, rec_ty) = rec;
        let r = self.name(rec_name);
        let rec_params = self.uparams(rec_params);
        let mut rule_json = Vec::new();
        for (ctor, fields, rhs) in rules {
            let c = self.name(ctor);
            rule_json.push(format!(r#"{{"ctor":{c},"nfields":{fields},"rhs":{rhs}}}"#));
        }
        let rec_json = format!(
            r#"{{"all":{all},"isUnsafe":false,"k":false,"levelParams":{rec_params},"name":{r},"numIndices":0,"numMinors":{},"numMotives":1,"numParams":0,"rules":[{}],"type":{rec_ty}}}"#,
            ctors.len(),
            rule_json.join(",")
        );
        self.lines.push(format!(
            r#"{{"inductive":{{"ctors":[{}],"recs":[{rec_json}],"types":[{ty_json}]}}}}"#,
            ctor_json.join(",")
        ));
    }

    /// The four quotient records, in LeanExport's order.
    pub(crate) fn quot(&mut self, members: [(&str, u64, &str); 4]) {
        for (name, ty, kind) in members {
            let n = self.name(name);
            let params = self.uparams(&["u"]);
            self.lines.push(format!(
                r#"{{"quot":{{"kind":"{kind}","levelParams":{params},"name":{n},"type":{ty}}}}}"#
            ));
        }
    }

    /// A record written as is.
    pub(crate) fn raw(&mut self, line: String) {
        self.lines.push(line);
    }

    pub(crate) fn hide(&mut self, name: &str) {
        let index = self.name(name);
        self.hidden.push(index);
    }

    /// The file as a plain LeanExport file.
    pub(crate) fn plain(&self) -> String {
        let mut text = serde_json::json!({ "meta": meta() }).to_string();
        text.push('\n');
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    /// The file as a `lev-export/1` module file.
    pub(crate) fn module(&self, module: &str) -> String {
        let header = serde_json::json!({
            "lev": {"exporter": EXPORT_FORMAT, "module": module},
            "meta": meta(),
        });
        let mut text = header.to_string();
        text.push('\n');
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str(&format!(r#"{{"lev":{{"hidden":{}}}}}"#, list(&self.hidden)));
        text.push('\n');
        text
    }
}

/// Write a `lev-export/1` directory: (module, imports, file) in manifest order.
pub(crate) fn write_export(dir: &Path, modules: &[(&str, &[&str], &File)]) {
    fs::create_dir_all(dir).expect("fixture directory");
    let mut entries = Vec::new();
    for (name, imports, file) in modules {
        let file_name = format!("{name}.ndjson");
        fs::write(dir.join(&file_name), file.module(name)).expect("fixture module");
        entries.push(serde_json::json!({
            "name": name,
            "file": file_name,
            "imports": imports.iter().map(|m| serde_json::json!({"module": m, "all": false})).collect::<Vec<_>>(),
            "isModule": true,
            "artifacts": [],
        }));
    }
    let manifest = serde_json::json!({
        "format": EXPORT_FORMAT,
        "lean": {"version": "0.0.0", "githash": GITHASH},
        "roots": [modules.last().map(|(name, _, _)| *name).unwrap_or_default()],
        "modules": entries,
    });
    fs::write(dir.join("manifest.json"), manifest.to_string()).expect("fixture manifest");
}

/// `Nat` with `zero`, `succ` and a recursor (types simplified).
pub(crate) fn nat(file: &mut File) {
    let one = file.succ(0);
    let ty = file.sort(one);
    let nat = file.cnst("Nat", &[]);
    let succ_ty = file.pi("n", nat, nat);
    let u = file.param("u");
    let motive_ty = file.sort(u);
    let rec_ty = file.pi("motive", nat, motive_ty);
    let rhs = file.bvar(0);
    file.inductive(
        "Nat",
        ty,
        &[("Nat.zero", nat, 0), ("Nat.succ", succ_ty, 1)],
        ("Nat.rec", &["u"], rec_ty),
        &[("Nat.zero", 0, rhs), ("Nat.succ", 1, rhs)],
    );
}

/// `∀ (p : Prop), p → p`.
pub(crate) fn prop_id(file: &mut File) -> u64 {
    let prop = file.sort(0);
    let p = file.bvar(0);
    let p1 = file.bvar(1);
    let inner = file.pi("h", p, p1);
    file.pi("p", prop, inner)
}

/// Module Base: `Nat`, an exposed and a hidden def, a theorem whose proof
/// mentions `Base.helper`, and an opaque. Module Top uses each of them.
pub(crate) fn base_and_top() -> (File, File) {
    let mut base = File::new();
    nat(&mut base);
    let nat_ty = base.cnst("Nat", &[]);
    let fun_ty = base.pi("n", nat_ty, nat_ty);
    let x = base.bvar(0);
    let id = base.lam("n", nat_ty, x);
    base.def("Base.id", &[], fun_ty, id);
    base.def("Base.hid", &[], fun_ty, id);
    base.hide("Base.hid");
    let zero = base.cnst("Nat.zero", &[]);
    base.def("Base.helper", &[], nat_ty, zero);
    let ty = prop_id(&mut base);
    let helper = base.cnst("Base.helper", &[]);
    let h = base.bvar(0);
    let k = base.lam("x", nat_ty, h);
    let body = base.app(k, helper);
    let p = base.bvar(0);
    let inner = base.lam("h", p, body);
    let prop = base.sort(0);
    let proof = base.lam("p", prop, inner);
    base.thm("Base.lemma", &[], ty, proof);
    base.opaque("Base.opq", &[], nat_ty, zero);

    let mut top = File::new();
    let nat_ty = top.cnst("Nat", &[]);
    let fun_ty = top.pi("n", nat_ty, nat_ty);
    let id = top.cnst("Base.id", &[]);
    top.def("Top.f", &[], fun_ty, id);
    let hid = top.cnst("Base.hid", &[]);
    top.def("Top.g", &[], fun_ty, hid);
    let ty = prop_id(&mut top);
    let lemma = top.cnst("Base.lemma", &[]);
    top.thm("Top.t", &[], ty, lemma);
    let opq = top.cnst("Base.opq", &[]);
    top.def("Top.o", &[], nat_ty, opq);
    (base, top)
}

/// A reference holding the same `Nat` as the fixtures.
pub(crate) fn reference(dir: &Path) -> PathBuf {
    let mut file = File::new();
    nat(&mut file);
    write_reference(dir, &file)
}

/// Write `file` as the plain reference export `dir/reference.ndjson`.
pub(crate) fn write_reference(dir: &Path, file: &File) -> PathBuf {
    let path = dir.join("reference.ndjson");
    fs::write(&path, file.plain()).expect("fixture reference");
    path
}

/// The pins of the reference [`reference`] writes.
pub(crate) fn pins() -> Pins {
    let dir = tempfile::tempdir().expect("tempdir");
    let reference = read_plain(&reference(dir.path())).expect("fixture reference");
    Pins::new(&reference, &super::hash::index(&reference, 1))
}

/// What no slice of `dev` axiomizes under the fixture reference's pins.
pub(crate) fn pinned(dev: &Development) -> HashSet<NameId> {
    Pinned::new(dev, &pins()).names
}
