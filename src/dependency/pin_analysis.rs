//! Explain how Lake selected revisions requested by several parent packages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::project::Project;
use crate::project::manifest::{LakeManifest, ManifestPackage};

use super::graph::package_root;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PinStatus {
    Aligned,
    Conflict,
    RootOverride,
    Stale,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinExpectation {
    pub parent: String,
    pub revision: Option<String>,
    pub requested: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackagePin {
    pub name: String,
    pub kind: String,
    pub direct: bool,
    pub selected_revision: Option<String>,
    pub status: PinStatus,
    pub expectations: Vec<PinExpectation>,
}

#[derive(Debug, Serialize)]
pub struct PinAnalysis {
    pub root: String,
    pub complete: bool,
    pub packages: Vec<PackagePin>,
    pub warnings: Vec<String>,
}

impl PinAnalysis {
    pub fn count(&self, status: PinStatus) -> usize {
        self.packages
            .iter()
            .filter(|package| package.status == status)
            .count()
    }
}

pub fn analyze(project: &Project) -> Result<PinAnalysis> {
    let manifest = LakeManifest::read(&project.manifest_path())?;
    let packages_dir = manifest.packages_path(&project.root)?;
    let root = manifest.name.clone().unwrap_or_else(|| {
        project
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.root.display().to_string())
    });
    let selected = manifest
        .packages
        .iter()
        .map(|package| (package.name.clone(), package.clone()))
        .collect::<BTreeMap<_, _>>();
    let direct = manifest
        .packages
        .iter()
        .filter(|package| !package.inherited)
        .map(|package| package.name.clone())
        .collect::<BTreeSet<_>>();
    let mut expectations = BTreeMap::<String, Vec<PinExpectation>>::new();
    record_expectations(&root, &manifest, &mut expectations);

    let mut warnings = Vec::new();
    let mut complete = true;
    for package in selected.values() {
        let package_root =
            package_root(&project.root, &packages_dir, package).with_context(|| {
                format!("failed to locate materialized dependency {}", package.name)
            })?;
        let manifest_path = package_root.join(
            package
                .manifest_file
                .as_deref()
                .unwrap_or_else(|| Path::new("lake-manifest.json")),
        );
        if !manifest_path.is_file() {
            complete = false;
            warnings.push(format!(
                "{} has no materialized manifest at {}; its dependency requests are unknown",
                package.name,
                manifest_path.display()
            ));
            continue;
        }
        match LakeManifest::read(&manifest_path) {
            Ok(child) => record_expectations(&package.name, &child, &mut expectations),
            Err(error) => {
                complete = false;
                warnings.push(format!(
                    "could not read {} for {}: {error:#}",
                    manifest_path.display(),
                    package.name
                ));
            }
        }
    }

    for (name, requests) in &expectations {
        if !selected.contains_key(name) {
            complete = false;
            warnings.push(format!(
                "{name} is requested by {} but has no selected entry in the root manifest",
                parent_list(requests)
            ));
        }
    }
    warnings.sort();
    warnings.dedup();

    let names = selected
        .keys()
        .chain(expectations.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let packages = names
        .into_iter()
        .map(|name| {
            let package = selected.get(&name);
            let mut requests = expectations.remove(&name).unwrap_or_default();
            requests.sort_by(|left, right| {
                left.parent
                    .cmp(&right.parent)
                    .then_with(|| left.revision.cmp(&right.revision))
                    .then_with(|| left.requested.cmp(&right.requested))
            });
            requests.dedup();
            let selected_revision = package.and_then(|package| package.rev.clone());
            let is_direct = direct.contains(&name);
            let status = classify(selected_revision.as_deref(), is_direct, &requests, complete);
            PackagePin {
                name,
                kind: package
                    .map(|package| package.kind.clone())
                    .unwrap_or_else(|| "unknown".to_owned()),
                direct: is_direct,
                selected_revision,
                status,
                expectations: requests,
            }
        })
        .collect();

    Ok(PinAnalysis {
        root,
        complete,
        packages,
        warnings,
    })
}

fn record_expectations(
    parent: &str,
    manifest: &LakeManifest,
    expectations: &mut BTreeMap<String, Vec<PinExpectation>>,
) {
    for package in manifest
        .packages
        .iter()
        .filter(|package| !package.inherited)
    {
        expectations
            .entry(package.name.clone())
            .or_default()
            .push(expectation(parent, package));
    }
}

fn expectation(parent: &str, package: &ManifestPackage) -> PinExpectation {
    PinExpectation {
        parent: parent.to_owned(),
        revision: package.rev.clone(),
        requested: package.input_rev.clone(),
    }
}

fn classify(
    selected: Option<&str>,
    direct: bool,
    expectations: &[PinExpectation],
    complete: bool,
) -> PinStatus {
    let Some(selected) = selected else {
        return PinStatus::Unknown;
    };
    let revisions = expectations
        .iter()
        .filter_map(|request| request.revision.as_deref())
        .collect::<Vec<_>>();
    if revisions.len() != expectations.len() || revisions.is_empty() {
        return PinStatus::Unknown;
    }

    let matching = revisions
        .iter()
        .filter(|revision| **revision == selected)
        .count();
    let differing = revisions.len() - matching;
    if matching > 0 && differing > 0 {
        if direct {
            PinStatus::RootOverride
        } else {
            PinStatus::Conflict
        }
    } else if matching == revisions.len() {
        if complete {
            PinStatus::Aligned
        } else {
            PinStatus::Unknown
        }
    } else if complete {
        PinStatus::Stale
    } else {
        PinStatus::Unknown
    }
}

fn parent_list(expectations: &[PinExpectation]) -> String {
    expectations
        .iter()
        .map(|expectation| expectation.parent.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{PinStatus, analyze};
    use crate::project::Project;

    const SELECTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const THIRD: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn distinguishes_conflicts_stale_selections_and_root_overrides() {
        let temp = tempdir().unwrap();
        write_project(
            temp.path(),
            &format!(
                r#"{{
                  "name":"root",
                  "packagesDir":".lake/packages",
                  "packages":[
                    {{"name":"a","type":"git","rev":"{SELECTED}","inherited":false}},
                    {{"name":"b","type":"git","rev":"{OTHER}","inherited":false}},
                    {{"name":"c","type":"git","rev":"{THIRD}","inherited":false}},
                    {{"name":"conflict","type":"git","rev":"{SELECTED}","inherited":true}},
                    {{"name":"stale","type":"git","rev":"{SELECTED}","inherited":true}},
                    {{"name":"override","type":"git","rev":"{SELECTED}","inherited":false}}
                  ]
                }}"#
            ),
        );
        write_child(
            temp.path(),
            "a",
            &format!(
                r#"{{"name":"a","packages":[
                  {{"name":"conflict","type":"git","rev":"{SELECTED}","inherited":false}},
                  {{"name":"stale","type":"git","rev":"{OTHER}","inherited":false}},
                  {{"name":"override","type":"git","rev":"{OTHER}","inherited":false}}
                ]}}"#
            ),
        );
        write_child(
            temp.path(),
            "b",
            &format!(
                r#"{{"name":"b","packages":[
                  {{"name":"conflict","type":"git","rev":"{OTHER}","inherited":false}},
                  {{"name":"stale","type":"git","rev":"{THIRD}","inherited":false}}
                ]}}"#
            ),
        );
        for package in ["c", "conflict", "stale", "override"] {
            write_child(
                temp.path(),
                package,
                &format!(r#"{{"name":"{package}","packages":[]}}"#),
            );
        }

        let report = analyze(&Project::discover(temp.path()).unwrap()).unwrap();
        assert!(report.complete);
        assert_eq!(status(&report, "conflict"), PinStatus::Conflict);
        assert_eq!(status(&report, "stale"), PinStatus::Stale);
        assert_eq!(status(&report, "override"), PinStatus::RootOverride);
        assert_eq!(report.count(PinStatus::Conflict), 1);
    }

    #[test]
    fn missing_parent_manifests_make_non_conflicting_conclusions_unknown() {
        let temp = tempdir().unwrap();
        write_project(
            temp.path(),
            &format!(
                r#"{{
                  "name":"root",
                  "packagesDir":".lake/packages",
                  "packages":[
                    {{"name":"parent","type":"git","rev":"{SELECTED}","inherited":false}},
                    {{"name":"dep","type":"git","rev":"{SELECTED}","inherited":true}}
                  ]
                }}"#
            ),
        );
        write_child(temp.path(), "dep", r#"{"name":"dep","packages":[]}"#);

        let report = analyze(&Project::discover(temp.path()).unwrap()).unwrap();
        assert!(!report.complete);
        assert_eq!(status(&report, "dep"), PinStatus::Unknown);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("parent") && warning.contains("unknown"))
        );
    }

    fn status(report: &super::PinAnalysis, name: &str) -> PinStatus {
        report
            .packages
            .iter()
            .find(|package| package.name == name)
            .unwrap()
            .status
    }

    fn write_project(root: &std::path::Path, manifest: &str) {
        fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.test\n").unwrap();
        fs::write(root.join("lakefile.toml"), "name = \"root\"\n").unwrap();
        fs::write(root.join("lake-manifest.json"), manifest).unwrap();
    }

    fn write_child(root: &std::path::Path, package: &str, manifest: &str) {
        let directory = root.join(".lake/packages").join(package);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("lake-manifest.json"), manifest).unwrap();
    }
}
