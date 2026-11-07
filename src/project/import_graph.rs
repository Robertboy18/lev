//! Lean module names, the reverse-import walk, and a header-only import scan.
//!
//! `inspect build` names modules from their trace paths and walks import edges
//! read from Lake's traces. The header scan reads the `import` lines of a source
//! file without elaborating it; it is kept for predicting what an edit rebuilds.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path};

/// Map a path below a library root, such as `Mathlib/Logic/Basic.trace`, to its
/// module name. Components that are not plain identifiers are written `«...»`,
/// as Lean prints them.
pub(crate) fn module_from_path(path: &Path) -> Option<String> {
    let mut components = Vec::new();
    for component in path.with_extension("").components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(render_component(value.to_str()?)?),
            _ => return None,
        }
    }
    (!components.is_empty()).then(|| components.join("."))
}

/// Every module that transitively imports one of `roots`, the roots included.
///
/// `imports` maps each module to its direct imports.
pub(crate) fn downstream(
    imports: &BTreeMap<String, BTreeSet<String>>,
    roots: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut importers = BTreeMap::<&str, Vec<&str>>::new();
    for (module, direct) in imports {
        for import in direct {
            importers.entry(import).or_default().push(module);
        }
    }
    let mut reached = roots.clone();
    let mut pending = roots.iter().map(String::as_str).collect::<VecDeque<_>>();
    while let Some(module) = pending.pop_front() {
        for importer in importers.get(module).into_iter().flatten() {
            if reached.insert((*importer).to_owned()) {
                pending.push_back(importer);
            }
        }
    }
    reached
}

/// Read the imports of a Lean source file's header.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the edit-cost query will be the first caller")
)]
pub(crate) mod header {
    use super::{identifier_rest, identifier_start, render_component};

    /// One `import` line of a module header.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Import {
        pub module: String,
        /// `public import`: the module's public olean re-exports the import.
        pub public: bool,
        /// `meta import`: the import is used at compile time.
        pub meta: bool,
        /// `import all`: the importer also sees private declarations.
        pub all: bool,
    }

    /// The scan skips comments, accepts the `module` and `prelude` keywords and
    /// the `public`, `private`, `meta`, and `all` modifiers, and stops at the
    /// first token that cannot belong to the header.
    pub(crate) fn imports(source: &str) -> Vec<Import> {
        let mut rest = skip_trivia(source);
        if let Some(after) = keyword(rest, "module") {
            rest = after;
        }
        if let Some(after) = keyword(rest, "prelude") {
            rest = after;
        }
        let mut imports = Vec::new();
        loop {
            let mut import = Import {
                module: String::new(),
                public: false,
                meta: false,
                all: false,
            };
            let mut cursor = rest;
            if let Some(after) = keyword(cursor, "public") {
                import.public = true;
                cursor = after;
            } else if let Some(after) = keyword(cursor, "private") {
                cursor = after;
            }
            if let Some(after) = keyword(cursor, "meta") {
                import.meta = true;
                cursor = after;
            }
            let Some(after) = keyword(cursor, "import") else {
                break;
            };
            cursor = after;
            if let Some(after) = keyword(cursor, "all") {
                import.all = true;
                cursor = after;
            }
            let Some((module, after)) = module_name(cursor) else {
                break;
            };
            import.module = module;
            imports.push(import);
            rest = skip_trivia(after);
        }
        imports
    }

    /// Skip whitespace, `--` line comments, and nested `/- -/` block comments.
    fn skip_trivia(text: &str) -> &str {
        let mut rest = text;
        loop {
            rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix("--") {
                rest = after.find('\n').map_or("", |end| &after[end + 1..]);
            } else if rest.starts_with("/-") {
                rest = skip_block_comment(rest);
            } else {
                return rest;
            }
        }
    }

    fn skip_block_comment(text: &str) -> &str {
        let bytes = text.as_bytes();
        let mut depth = 0_usize;
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index..].starts_with(b"/-") {
                depth += 1;
                index += 2;
            } else if bytes[index..].starts_with(b"-/") {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    return &text[index..];
                }
            } else {
                index += 1;
            }
        }
        ""
    }

    fn keyword<'a>(text: &'a str, word: &str) -> Option<&'a str> {
        let rest = text.strip_prefix(word)?;
        rest.starts_with(char::is_whitespace)
            .then(|| skip_trivia(rest))
    }

    fn module_name(text: &str) -> Option<(String, &str)> {
        let mut rest = text;
        let mut components = Vec::new();
        loop {
            let (part, after) = component(rest)?;
            components.push(render_component(part)?);
            rest = after;
            let Some(after_dot) = rest.strip_prefix('.') else {
                break;
            };
            rest = after_dot;
        }
        Some((components.join("."), rest))
    }

    fn component(text: &str) -> Option<(&str, &str)> {
        if let Some(escaped) = text.strip_prefix('«') {
            let end = escaped.find('»')?;
            return Some((&escaped[..end], &escaped[end + '»'.len_utf8()..]));
        }
        let mut characters = text.char_indices();
        let (_, first) = characters.next()?;
        if !identifier_start(first) {
            return None;
        }
        let mut end = first.len_utf8();
        for (index, character) in characters {
            if !identifier_rest(character) {
                break;
            }
            end = index + character.len_utf8();
        }
        Some((&text[..end], &text[end..]))
    }
}

/// Write a name component the way Lean prints it: bare when it is an
/// identifier, `«...»` otherwise. A component containing `»` cannot be written.
fn render_component(value: &str) -> Option<String> {
    let mut characters = value.chars();
    if characters.next().is_some_and(identifier_start) && characters.all(identifier_rest) {
        Some(value.to_owned())
    } else if value.is_empty() || value.contains('»') {
        None
    } else {
        Some(format!("«{value}»"))
    }
}

fn identifier_start(character: char) -> bool {
    let value = character as u32;
    character == '_'
        || character.is_ascii_alphabetic()
        || (0x03b1..=0x03c9).contains(&value) && value != 0x03bb
        || (0x0391..=0x03a9).contains(&value) && !matches!(value, 0x03a0 | 0x03a3)
        || (0x03ca..=0x03fb).contains(&value)
        || (0x1f00..=0x1ffe).contains(&value)
        || (0x2100..=0x214f).contains(&value)
        || (0x1d49c..=0x1d59f).contains(&value)
        || (0x00c0..=0x00ff).contains(&value) && !matches!(value, 0x00d7 | 0x00f7)
        || (0x0100..=0x017f).contains(&value)
}

fn identifier_rest(character: char) -> bool {
    let value = character as u32;
    character.is_ascii_alphanumeric()
        || matches!(character, '_' | '\'' | '!' | '?')
        || identifier_start(character)
        || (0x2080..=0x2089).contains(&value)
        || (0x2090..=0x209c).contains(&value)
        || (0x1d62..=0x1d6a).contains(&value)
        || value == 0x2c7c
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::header::{self, Import};
    use super::{downstream, module_from_path};

    fn import(module: &str, public: bool, meta: bool, all: bool) -> Import {
        Import {
            module: module.to_owned(),
            public,
            meta,
            all,
        }
    }

    #[test]
    fn reads_header_imports_with_modifiers_and_comments() {
        let imports = header::imports(
            r#"/- Copyright (c) 2026. /- nested -/ still a comment -/
-- import Fake
module
public import Mathlib.Data.Nat.Prime
meta import Project.Meta -- trailing comment
public meta import all Project.PublicMeta
import all Project.All import Project.SameLine
/- import Also.Fake -/
private import Project.Helper

/-! Module documentation. -/
def text := "import String.Fake"
import After.Body
"#,
        );
        assert_eq!(
            imports,
            [
                import("Mathlib.Data.Nat.Prime", true, false, false),
                import("Project.Meta", false, true, false),
                import("Project.PublicMeta", true, true, true),
                import("Project.All", false, false, true),
                import("Project.SameLine", false, false, false),
                import("Project.Helper", false, false, false),
            ]
        );
    }

    #[test]
    fn stops_at_the_first_command_and_keeps_escaped_names() {
        let imports = header::imports(
            "prelude\nimport «foo-bar»\nimport «foo.bar».Baz\nimport «foo--bar»\ntheorem t : True := trivial\nimport Late\n",
        );
        assert_eq!(
            imports
                .iter()
                .map(|import| import.module.as_str())
                .collect::<Vec<_>>(),
            ["«foo-bar»", "«foo.bar».Baz", "«foo--bar»"]
        );
        assert!(header::imports("def x := 1\nimport Late\n").is_empty());
    }

    #[test]
    fn names_modules_from_paths_as_lean_prints_them() {
        assert_eq!(
            module_from_path(Path::new("Mathlib/Logic/Basic.trace")).as_deref(),
            Some("Mathlib.Logic.Basic")
        );
        assert_eq!(
            module_from_path(Path::new("foo-bar.trace")).as_deref(),
            Some("«foo-bar»")
        );
        assert_eq!(
            module_from_path(Path::new("foo.bar/Baz.trace")).as_deref(),
            Some("«foo.bar».Baz")
        );
        assert_eq!(
            module_from_path(Path::new("中文.trace")).as_deref(),
            Some("«中文»")
        );
        assert_eq!(module_from_path(Path::new("../Escape.trace")), None);
        assert_eq!(module_from_path(Path::new("a»b.trace")), None);
    }

    #[test]
    fn walks_reverse_imports_through_diamonds_and_cycles() {
        let imports = BTreeMap::from([
            ("A".to_owned(), BTreeSet::new()),
            ("B".to_owned(), BTreeSet::from(["A".to_owned()])),
            ("C".to_owned(), BTreeSet::from(["A".to_owned()])),
            (
                "D".to_owned(),
                BTreeSet::from(["B".to_owned(), "C".to_owned()]),
            ),
            ("X".to_owned(), BTreeSet::from(["Y".to_owned()])),
            ("Y".to_owned(), BTreeSet::from(["X".to_owned()])),
        ]);
        let reached = downstream(&imports, &BTreeSet::from(["B".to_owned()]));
        assert_eq!(reached, BTreeSet::from(["B".to_owned(), "D".to_owned()]));

        let reached = downstream(&imports, &BTreeSet::from(["A".to_owned()]));
        assert_eq!(reached.len(), 4);

        let reached = downstream(&imports, &BTreeSet::from(["X".to_owned()]));
        assert_eq!(reached, BTreeSet::from(["X".to_owned(), "Y".to_owned()]));
    }
}
