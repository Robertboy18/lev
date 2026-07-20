//! Deterministic project inventory and CycloneDX export.
//!
//! Exports come from the verified root lock and omit timestamps and local
//! paths, so materialized checkouts do not affect the result.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cache::digest;
use crate::project::Project;
use crate::project::lockfile;
use crate::project::manifest::{LakeManifest, ManifestPackage, validate_package_name};

const EXPORT_SCHEMA: &str = "lev.project-export/v1";

/// Machine-readable format produced by [`render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// lev's compact dependency and integrity inventory.
    LevJson,
    /// A deterministic CycloneDX 1.6 software bill of materials.
    CycloneDxJson,
}

/// Verify and render the project's currently locked dependency state.
pub fn render(project: &Project, format: ExportFormat) -> Result<Vec<u8>> {
    lockfile::verify(project)?;
    let inventory = LockedInventory::load(project)?;
    let integrity = read_integrity(project)?;

    let mut bytes = match format {
        ExportFormat::LevJson => render_lev(project, inventory, integrity)?,
        ExportFormat::CycloneDxJson => render_cyclonedx(project, inventory, integrity)?,
    };
    bytes.push(b'\n');
    Ok(bytes)
}

#[derive(Debug, Clone, Serialize)]
struct ExportIntegrity {
    configuration: String,
    configuration_sha256: String,
    manifest_sha256: String,
    lock_sha256: String,
}

fn read_integrity(project: &Project) -> Result<ExportIntegrity> {
    let (configuration, configuration_bytes) = read_configuration(&project.root)?;
    let manifest = fs::read(project.manifest_path())
        .with_context(|| format!("failed to read {}", project.manifest_path().display()))?;
    let lock = fs::read(project.lock_path())
        .with_context(|| format!("failed to read {}", project.lock_path().display()))?;
    Ok(ExportIntegrity {
        configuration,
        configuration_sha256: digest(&configuration_bytes),
        manifest_sha256: digest(&manifest),
        lock_sha256: digest(&lock),
    })
}

fn read_configuration(root: &Path) -> Result<(String, Vec<u8>)> {
    for name in ["lakefile.toml", "lakefile.lean"] {
        let path = root.join(name);
        if path.is_file() {
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            return Ok((name.to_owned(), bytes));
        }
    }
    bail!(
        "no lakefile.toml or lakefile.lean found in {}",
        root.display()
    )
}

#[derive(Debug, Serialize)]
struct LevProjectExport {
    schema: &'static str,
    project: ExportProject,
    integrity: ExportIntegrity,
    direct_dependencies: Vec<String>,
    packages: Vec<ExportPackage>,
}

#[derive(Debug, Serialize)]
struct ExportProject {
    name: String,
    toolchain: String,
}

#[derive(Debug)]
struct LockedInventory {
    root: String,
    direct_dependencies: Vec<String>,
    packages: Vec<ExportPackage>,
}

#[derive(Debug, Clone, Serialize)]
struct ExportPackage {
    name: String,
    kind: String,
    direct: bool,
    source: Option<String>,
    revision: Option<String>,
    requested: Option<String>,
    scope: Option<String>,
    subdir: Option<String>,
}

impl LockedInventory {
    fn load(project: &Project) -> Result<Self> {
        let manifest = LakeManifest::read(&project.manifest_path())?;
        let root = manifest.name.clone().unwrap_or_else(|| "root".to_owned());
        let mut names = HashSet::new();
        let mut packages = Vec::with_capacity(manifest.packages.len());
        for package in &manifest.packages {
            validate_package_name(&package.name)?;
            if !names.insert(package.name.clone()) {
                bail!(
                    "duplicate package name in lake-manifest.json: {}",
                    package.name
                );
            }
            packages.push(ExportPackage::from_manifest(package)?);
        }
        packages.sort_by(|left, right| {
            (&left.name, &left.kind, &left.source).cmp(&(&right.name, &right.kind, &right.source))
        });
        let direct_dependencies = packages
            .iter()
            .filter(|package| package.direct)
            .map(|package| package.name.clone())
            .collect();
        Ok(Self {
            root,
            direct_dependencies,
            packages,
        })
    }
}

impl ExportPackage {
    fn from_manifest(package: &ManifestPackage) -> Result<Self> {
        let source = if package.kind == "path" {
            package
                .dir
                .as_ref()
                .map(|path| utf8_path(path, "path dependency source"))
                .transpose()?
        } else {
            package.url.clone()
        };
        Ok(Self {
            name: package.name.clone(),
            kind: package.kind.clone(),
            direct: !package.inherited,
            source,
            revision: package.rev.clone(),
            requested: package.input_rev.clone(),
            scope: package.scope.clone().filter(|scope| !scope.is_empty()),
            subdir: package
                .sub_dir
                .as_ref()
                .map(|path| utf8_path(path, "package subdirectory"))
                .transpose()?,
        })
    }
}

fn utf8_path(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{label} is not UTF-8: {}", path.display()))
}

fn render_lev(
    project: &Project,
    inventory: LockedInventory,
    integrity: ExportIntegrity,
) -> Result<Vec<u8>> {
    let document = LevProjectExport {
        schema: EXPORT_SCHEMA,
        project: ExportProject {
            name: inventory.root,
            toolchain: project.toolchain.clone(),
        },
        integrity,
        direct_dependencies: inventory.direct_dependencies,
        packages: inventory.packages,
    };
    serde_json::to_vec_pretty(&document).context("failed to serialize project export")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CycloneDxBom {
    bom_format: &'static str,
    spec_version: &'static str,
    version: u32,
    metadata: CycloneMetadata,
    components: Vec<CycloneComponent>,
    dependencies: Vec<CycloneDependency>,
}

#[derive(Debug, Serialize)]
struct CycloneMetadata {
    component: CycloneComponent,
    properties: Vec<CycloneProperty>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CycloneComponent {
    #[serde(rename = "type")]
    component_type: &'static str,
    #[serde(rename = "bom-ref")]
    bom_ref: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    properties: Vec<CycloneProperty>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    external_references: Vec<CycloneExternalReference>,
}

#[derive(Debug, Serialize)]
struct CycloneProperty {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct CycloneExternalReference {
    #[serde(rename = "type")]
    reference_type: &'static str,
    url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CycloneDependency {
    #[serde(rename = "ref")]
    reference: String,
    depends_on: Vec<String>,
}

fn render_cyclonedx(
    project: &Project,
    inventory: LockedInventory,
    integrity: ExportIntegrity,
) -> Result<Vec<u8>> {
    let root_reference = format!("urn:lev:project:{}", integrity.lock_sha256);
    let references = inventory
        .packages
        .iter()
        .map(|package| (package.name.clone(), package_reference(package)))
        .collect::<BTreeMap<_, _>>();
    let components = inventory
        .packages
        .iter()
        .map(package_component)
        .collect::<Vec<_>>();

    let mut dependencies = Vec::with_capacity(inventory.packages.len() + 1);
    dependencies.push(CycloneDependency {
        reference: root_reference.clone(),
        depends_on: resolve_references(&inventory.direct_dependencies, &references),
    });
    for package in &inventory.packages {
        dependencies.push(CycloneDependency {
            reference: references
                .get(&package.name)
                .expect("reference map mirrors package map")
                .clone(),
            // Lake's root manifest identifies direct versus inherited
            // dependencies but does not encode transitive edges. Reading
            // checkout-local child manifests here would destroy determinism.
            depends_on: Vec::new(),
        });
    }

    let document = CycloneDxBom {
        bom_format: "CycloneDX",
        spec_version: "1.6",
        version: 1,
        metadata: CycloneMetadata {
            component: CycloneComponent {
                component_type: "application",
                bom_ref: root_reference,
                name: inventory.root,
                version: None,
                scope: None,
                properties: Vec::new(),
                external_references: Vec::new(),
            },
            properties: vec![
                property("lev:toolchain", &project.toolchain),
                property("lev:configuration", &integrity.configuration),
                property("lev:configuration-sha256", &integrity.configuration_sha256),
                property("lev:manifest-sha256", &integrity.manifest_sha256),
                property("lev:lock-sha256", &integrity.lock_sha256),
            ],
        },
        components,
        dependencies,
    };
    serde_json::to_vec_pretty(&document).context("failed to serialize CycloneDX export")
}

fn package_reference(package: &ExportPackage) -> String {
    let identity = serde_json::to_vec(package).expect("graph package serialization cannot fail");
    format!("urn:lev:package:{}", digest(&identity))
}

fn package_component(package: &ExportPackage) -> CycloneComponent {
    let mut properties = vec![
        property("lev:dependency-kind", &package.kind),
        property("lev:direct", if package.direct { "true" } else { "false" }),
    ];
    if let Some(requested) = &package.requested {
        properties.push(property("lev:requested", requested));
    }
    if let Some(source) = &package.source {
        properties.push(property("lev:source", source));
    }
    if let Some(scope) = &package.scope {
        properties.push(property("lev:scope", scope));
    }
    if let Some(subdir) = &package.subdir {
        properties.push(property("lev:subdir", subdir));
    }

    let external_references = package
        .source
        .as_ref()
        .filter(|source| source.starts_with("https://") || source.starts_with("http://"))
        .map(|source| {
            vec![CycloneExternalReference {
                reference_type: "vcs",
                url: source.clone(),
            }]
        })
        .unwrap_or_default();

    CycloneComponent {
        component_type: "library",
        bom_ref: package_reference(package),
        name: package.name.clone(),
        version: package
            .revision
            .clone()
            .or_else(|| package.requested.clone()),
        scope: Some("required"),
        properties,
        external_references,
    }
}

fn resolve_references(names: &[String], references: &BTreeMap<String, String>) -> Vec<String> {
    names
        .iter()
        .filter_map(|name| references.get(name).cloned())
        .collect()
}

fn property(name: &str, value: &str) -> CycloneProperty {
    CycloneProperty {
        name: name.to_owned(),
        value: value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;
    use tempfile::tempdir;

    use super::{ExportFormat, render};
    use crate::project::Project;
    use crate::project::lockfile;

    fn project() -> (tempfile::TempDir, Project) {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lean-toolchain"), "v4.fixture-d\n").unwrap();
        fs::write(
            temp.path().join("lakefile.toml"),
            "[package]\nname = \"root\"\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("lake-manifest.json"),
            r#"{
              "name": "root",
              "packagesDir": ".lake/packages",
              "packages": [{
                "name": "dep",
                "type": "git",
                "url": "https://example.invalid/dep.git",
                "rev": "0123456789abcdef0123456789abcdef01234567",
                "inputRev": "v1.0.0",
                "inherited": false
              }]
            }"#,
        )
        .unwrap();
        fs::create_dir_all(temp.path().join(".lake/packages/dep")).unwrap();
        fs::write(
            temp.path().join(".lake/packages/dep/lake-manifest.json"),
            r#"{"name":"dep","packages":[]}"#,
        )
        .unwrap();
        let project = Project::discover(temp.path()).unwrap();
        lockfile::refresh(&project).unwrap();
        (temp, project)
    }

    #[test]
    fn renders_deterministic_inventory_and_cyclonedx_documents() {
        let (_temp, project) = project();
        let first = render(&project, ExportFormat::LevJson).unwrap();
        assert_eq!(render(&project, ExportFormat::LevJson).unwrap(), first);
        fs::write(
            project.root.join(".lake/packages/dep/lake-manifest.json"),
            r#"{"name":"locally-different","packages":[]}"#,
        )
        .unwrap();
        assert_eq!(
            render(&project, ExportFormat::LevJson).unwrap(),
            first,
            "materialized child manifests must not affect deterministic export"
        );
        let inventory: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(inventory["schema"], "lev.project-export/v1");
        assert_eq!(inventory["packages"][0]["name"], "dep");

        let bom: Value =
            serde_json::from_slice(&render(&project, ExportFormat::CycloneDxJson).unwrap())
                .unwrap();
        assert_eq!(bom["bomFormat"], "CycloneDX");
        assert_eq!(bom["specVersion"], "1.6");
        assert_eq!(bom["components"][0]["name"], "dep");
        assert_eq!(bom["dependencies"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn refuses_to_export_drifted_lock_state() {
        let (_temp, project) = project();
        fs::write(
            project.root.join("lakefile.toml"),
            "[package]\nname = \"changed\"\n",
        )
        .unwrap();
        assert!(render(&project, ExportFormat::LevJson).is_err());
    }
}
