//! The documentation site and the README link only to pages and anchors that
//! exist.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Where the `docs/` pages are published.
const SITE: &str = "https://robertboy18.github.io/lev/";

#[test]
fn documentation_links_resolve() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let docs = root.join("docs");
    if !docs.is_dir() {
        // The published crate does not ship the site.
        return;
    }
    let mut pages = BTreeMap::new();
    for entry in fs::read_dir(&docs).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "html")
        {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            pages.insert(name, fs::read_to_string(&path).unwrap());
        }
    }
    let anchors = pages
        .iter()
        .map(|(name, text)| (name.as_str(), attribute_values(text, "id")))
        .collect::<BTreeMap<_, _>>();

    let mut broken = Vec::new();
    for (name, text) in &pages {
        let targets = attribute_values(text, "href")
            .into_iter()
            .chain(attribute_values(text, "src"));
        for target in targets {
            if let Some(problem) = check(&docs, &anchors, name, &target) {
                broken.push(format!("docs/{name}: {target}: {problem}"));
            }
        }
    }

    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    for target in markdown_links(&readme) {
        let problem = if let Some(page) = target.strip_prefix(SITE) {
            let page = if page.is_empty() { "index.html" } else { page };
            check(&docs, &anchors, "index.html", page)
        } else if target.contains("://") || target.starts_with('#') {
            None
        } else {
            let file = target.split('#').next().unwrap_or_default();
            (!root.join(file).exists()).then(|| "missing file".to_owned())
        };
        if let Some(problem) = problem {
            broken.push(format!("README.md: {target}: {problem}"));
        }
    }

    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

/// Check one link from `page`, ignoring links that leave the site.
fn check(
    docs: &Path,
    anchors: &BTreeMap<&str, BTreeSet<String>>,
    page: &str,
    target: &str,
) -> Option<String> {
    if target.contains("://") || target.starts_with("mailto:") {
        return None;
    }
    let (file, fragment) = target.split_once('#').unwrap_or((target, ""));
    let file = if file.is_empty() { page } else { file };
    if !docs.join(file).exists() {
        return Some("missing file".to_owned());
    }
    match anchors.get(file) {
        Some(ids) if !fragment.is_empty() && !ids.contains(fragment) => {
            Some("missing anchor".to_owned())
        }
        _ => None,
    }
}

/// Every value of ` NAME="..."` in an HTML page.
fn attribute_values(text: &str, name: &str) -> BTreeSet<String> {
    let needle = format!(" {name}=\"");
    text.match_indices(&needle)
        .filter_map(|(start, _)| {
            text[start + needle.len()..]
                .split_once('"')
                .map(|(value, _)| value.to_owned())
        })
        .collect()
}

/// Every `[text](target)` target in a Markdown file.
fn markdown_links(text: &str) -> Vec<String> {
    text.match_indices("](")
        .filter_map(|(start, _)| {
            text[start + 2..]
                .split_once(')')
                .map(|(target, _)| target.to_owned())
        })
        .collect()
}
