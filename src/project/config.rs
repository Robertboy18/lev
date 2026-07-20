//! Parsing for optional `lev.toml` project settings.
//!
//! Lean and Lake files still own toolchains, dependencies, and revisions.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::core::atomic_file::replace as atomic_write;
use crate::dependency::reservoir::{ReservoirPackage, ReservoirSource};
use crate::project::manifest::LakeManifest;

/// Per-toolchain dependency-resolution policy.
///
/// Declarative Lakefiles normally need no entry: lev can select compatible
/// direct dependency releases automatically. An alternate Lakefile is the
/// explicit, safe escape hatch for executable `lakefile.lean` projects.
#[derive(Debug, Clone)]
pub struct EnvironmentConfig {
    /// Project-relative Lakefile used only inside the selected environment.
    pub lakefile: Option<PathBuf>,
    /// Exact direct dependency revisions that override automatic selection.
    pub dependencies: BTreeMap<String, String>,
    /// Whether Reservoir-compatible direct releases should be selected.
    pub auto: bool,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            lakefile: None,
            dependencies: BTreeMap::new(),
            auto: true,
        }
    }
}

/// Validated optional settings loaded from one project or workspace root.
#[derive(Debug, Default)]
pub struct LevConfig {
    /// Existing `lev.toml` path, or `None` when defaults were returned.
    pub path: Option<PathBuf>,
    /// Toolchains selected by `[matrix].toolchains`.
    pub matrix_toolchains: Vec<String>,
    /// Default command selected by `[matrix].command`.
    pub matrix_command: Vec<OsString>,
    /// Named commands declared under `[tasks]`.
    pub tasks: BTreeMap<String, Vec<OsString>>,
    /// Relative project paths or glob patterns owned by this monorepo.
    pub workspace_members: Vec<String>,
    /// Relative patterns removed from the expanded member set.
    pub workspace_exclude: Vec<String>,
    /// Canonical Lean toolchain to version-specific resolution policy.
    pub environments: BTreeMap<String, EnvironmentConfig>,
    /// Named package-metadata registries.
    pub registries: BTreeMap<String, ReservoirSource>,
    /// Exact `owner/package` or `*` routes to named registries.
    pub package_sources: BTreeMap<String, String>,
    /// Named sets of ordinary direct Lake dependencies.
    pub dependency_groups: BTreeMap<String, BTreeSet<String>>,
    /// Required selectors or immutable commits in every accepted lock.
    pub constraints: BTreeMap<String, String>,
}

impl LevConfig {
    /// Select the configured metadata registry for one Reservoir identity.
    pub fn reservoir_source(&self, package: &ReservoirPackage) -> Result<ReservoirSource> {
        let full_name = package.full_name();
        let selected = self
            .package_sources
            .get(&full_name)
            .or_else(|| self.package_sources.get("*"));
        let Some(selected) = selected else {
            return Ok(ReservoirSource::default());
        };
        self.registries.get(selected).cloned().with_context(|| {
            format!("package source {full_name} references unknown registry {selected:?}")
        })
    }

    pub fn group_packages(&self, group: &str) -> Result<Vec<String>> {
        self.dependency_groups
            .get(group)
            .map(|packages| packages.iter().cloned().collect())
            .with_context(|| format!("dependency group {group:?} is not configured"))
    }

    /// Enforce repository policy against Lake's completed resolution.
    ///
    /// A canonical commit constrains the immutable `rev`; other values
    /// constrain Lake's recorded `inputRev`. These are assertions, not hidden
    /// resolver overrides, so unsupported transitive edits never mutate the
    /// standard Lakefile behind the user's back.
    pub fn verify_constraints(&self, manifest: &LakeManifest) -> Result<()> {
        for (name, constraint) in &self.constraints {
            let package = manifest
                .packages
                .iter()
                .find(|package| package.name == *name)
                .with_context(|| {
                    format!("constraint references package {name:?}, which is not in the lock")
                })?;
            let (field, actual) = if crate::core::hex::is_git_object_id(constraint) {
                ("revision", package.rev.as_deref())
            } else {
                ("requested selector", package.input_rev.as_deref())
            };
            if actual != Some(constraint.as_str()) {
                bail!(
                    "constraint for {name} requires {field} {constraint:?}, found {}",
                    actual.unwrap_or("<missing>")
                );
            }
        }
        Ok(())
    }

    /// Read `<project_root>/lev.toml`, returning defaults when it is absent.
    pub fn read(project_root: &Path) -> Result<Self> {
        let path = project_root.join("lev.toml");
        if !path.is_file() {
            return Ok(Self::default());
        }
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let document: DocumentMut = source
            .parse()
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let mut config = Self {
            path: Some(path.clone()),
            ..Self::default()
        };

        if let Some(item) = document.get("matrix") {
            let matrix = item
                .as_table()
                .with_context(|| format!("matrix in {} must be a TOML table", path.display()))?;
            if let Some(item) = matrix.get("toolchains") {
                config.matrix_toolchains = string_array(item, "matrix.toolchains", &path)?;
            }
            if let Some(item) = matrix.get("command") {
                config.matrix_command = string_array(item, "matrix.command", &path)?
                    .into_iter()
                    .map(OsString::from)
                    .collect();
            }
        }

        if let Some(tasks) = document.get("tasks").and_then(Item::as_table) {
            for (name, item) in tasks {
                let command = string_array(item, &format!("tasks.{name}"), &path)?
                    .into_iter()
                    .map(OsString::from)
                    .collect::<Vec<_>>();
                if command.is_empty() {
                    bail!("task {name:?} in {} has an empty command", path.display());
                }
                config.tasks.insert(name.to_owned(), command);
            }
        }

        if let Some(workspace) = document.get("workspace").and_then(Item::as_table) {
            if let Some(item) = workspace.get("members") {
                config.workspace_members = string_array(item, "workspace.members", &path)?;
            }
            if let Some(item) = workspace.get("exclude") {
                config.workspace_exclude = string_array(item, "workspace.exclude", &path)?;
            }
            if config.workspace_members.is_empty() {
                bail!(
                    "workspace.members in {} must contain at least one project pattern",
                    path.display()
                );
            }
        }

        if let Some(item) = document.get("registries") {
            let registries = item.as_table().with_context(|| {
                format!("registries in {} must be a TOML table", path.display())
            })?;
            for (name, item) in registries {
                let table = item.as_table().with_context(|| {
                    format!(
                        "registries.{name} in {} must be a TOML table",
                        path.display()
                    )
                })?;
                for (field, _) in table {
                    if !matches!(field, "url" | "token-env") {
                        bail!(
                            "unknown registries.{name}.{field} field in {}",
                            path.display()
                        );
                    }
                }
                let url = table
                    .get("url")
                    .with_context(|| {
                        format!("registries.{name}.url is required in {}", path.display())
                    })
                    .and_then(|item| {
                        string_value(item, &format!("registries.{name}.url"), &path)
                    })?;
                let token_env = table
                    .get("token-env")
                    .map(|item| string_value(item, &format!("registries.{name}.token-env"), &path))
                    .transpose()?;
                let registry = ReservoirSource::new(name, url, token_env)?;
                config.registries.insert(name.to_owned(), registry);
            }
        }

        if let Some(item) = document.get("sources") {
            let sources = item
                .as_table()
                .with_context(|| format!("sources in {} must be a TOML table", path.display()))?;
            for (package, item) in sources {
                if package != "*" && !valid_package_route(package) {
                    bail!(
                        "source key {package:?} in {} must be `owner/package` or `*`",
                        path.display()
                    );
                }
                let registry = string_value(item, &format!("sources.{package}"), &path)?.to_owned();
                config.package_sources.insert(package.to_owned(), registry);
            }
        }
        for (package, registry) in &config.package_sources {
            if !config.registries.contains_key(registry) {
                bail!(
                    "source {package:?} in {} references unknown registry {registry:?}",
                    path.display()
                );
            }
        }

        if let Some(item) = document.get("dependency-groups") {
            let groups = item.as_table().with_context(|| {
                format!(
                    "dependency-groups in {} must be a TOML table",
                    path.display()
                )
            })?;
            for (group, item) in groups {
                validate_policy_name(group, "dependency group")?;
                let packages = string_array(item, &format!("dependency-groups.{group}"), &path)?;
                if packages.is_empty() {
                    bail!(
                        "dependency-groups.{group} in {} must not be empty",
                        path.display()
                    );
                }
                let mut unique = BTreeSet::new();
                for package in packages {
                    validate_policy_name(&package, "dependency")?;
                    if !unique.insert(package.clone()) {
                        bail!(
                            "dependency-groups.{group} in {} contains duplicate package {package:?}",
                            path.display()
                        );
                    }
                }
                config.dependency_groups.insert(group.to_owned(), unique);
            }
        }

        if let Some(item) = document.get("constraints") {
            let constraints = item.as_table().with_context(|| {
                format!("constraints in {} must be a TOML table", path.display())
            })?;
            for (package, item) in constraints {
                validate_policy_name(package, "constraint package")?;
                let revision = string_value(item, &format!("constraints.{package}"), &path)?;
                validate_revision(revision, &format!("constraints.{package}"))?;
                config
                    .constraints
                    .insert(package.to_owned(), revision.to_owned());
            }
        }

        if let Some(item) = document.get("environments") {
            let environments = item.as_table().with_context(|| {
                format!("environments in {} must be a TOML table", path.display())
            })?;
            for (selector, item) in environments {
                let table = item.as_table().with_context(|| {
                    format!(
                        "environments.{selector:?} in {} must be a TOML table",
                        path.display()
                    )
                })?;
                for (field, _) in table {
                    if !matches!(field, "lakefile" | "dependencies" | "auto") {
                        bail!(
                            "unknown environments.{selector}.{field} field in {}",
                            path.display()
                        );
                    }
                }

                let canonical = crate::toolchain::normalize(selector)?;
                let mut environment = EnvironmentConfig::default();
                if let Some(item) = table.get("lakefile") {
                    let value =
                        string_value(item, &format!("environments.{selector}.lakefile"), &path)?;
                    let lakefile = PathBuf::from(value);
                    validate_relative_path(&lakefile, selector, &path)?;
                    environment.lakefile = Some(lakefile);
                }
                if let Some(item) = table.get("auto") {
                    environment.auto = item.as_bool().with_context(|| {
                        format!(
                            "environments.{selector}.auto in {} must be a boolean",
                            path.display()
                        )
                    })?;
                }
                if let Some(item) = table.get("dependencies") {
                    let dependencies = item.as_table().with_context(|| {
                        format!(
                            "environments.{selector}.dependencies in {} must be a TOML table",
                            path.display()
                        )
                    })?;
                    for (name, item) in dependencies {
                        if name.trim().is_empty() || name.chars().any(char::is_whitespace) {
                            bail!(
                                "invalid dependency name {name:?} in environments.{selector}.dependencies"
                            );
                        }
                        let revision = string_value(
                            item,
                            &format!("environments.{selector}.dependencies.{name}"),
                            &path,
                        )?;
                        if revision.trim().is_empty() || revision.chars().any(char::is_whitespace) {
                            bail!(
                                "invalid revision for environments.{selector}.dependencies.{name}"
                            );
                        }
                        environment
                            .dependencies
                            .insert(name.to_owned(), revision.to_owned());
                    }
                }
                if config
                    .environments
                    .insert(canonical.clone(), environment)
                    .is_some()
                {
                    bail!(
                        "duplicate environment selectors normalize to {canonical} in {}",
                        path.display()
                    );
                }
            }
        }
        Ok(config)
    }

    /// Add one dependency to a named group while preserving unrelated TOML.
    pub fn add_to_dependency_group(
        project_root: &Path,
        group: &str,
        package: &str,
    ) -> Result<PathBuf> {
        validate_policy_name(group, "dependency group")?;
        validate_policy_name(package, "dependency")?;
        let path = project_root.join("lev.toml");
        let mut document = read_document(&path)?;
        if !document.contains_key("dependency-groups") {
            document["dependency-groups"] = Item::Table(Table::new());
        }
        let groups = document["dependency-groups"]
            .as_table_mut()
            .with_context(|| {
                format!(
                    "dependency-groups in {} must be a TOML table",
                    path.display()
                )
            })?;
        if !groups.contains_key(group) {
            groups[group] = Item::Value(Value::Array(Array::new()));
        }
        let packages = groups[group].as_array_mut().with_context(|| {
            format!(
                "dependency-groups.{group} in {} must be an array of strings",
                path.display()
            )
        })?;
        if packages.iter().any(|value| value.as_str() == Some(package)) {
            bail!("dependency {package} is already in group {group:?}");
        }
        packages.push(package);
        write_document(&path, &document)?;
        Ok(path)
    }

    /// Remove a dependency from every group after its Lake declaration leaves.
    pub fn remove_from_dependency_groups(project_root: &Path, package: &str) -> Result<()> {
        let path = project_root.join("lev.toml");
        if !path.is_file() {
            return Ok(());
        }
        let mut document = read_document(&path)?;
        let Some(groups) = document
            .get_mut("dependency-groups")
            .and_then(Item::as_table_mut)
        else {
            return Ok(());
        };
        let mut empty = Vec::new();
        for (group, item) in groups.iter_mut() {
            let packages = item.as_array_mut().with_context(|| {
                format!(
                    "dependency-groups.{group} in {} must be an array of strings",
                    path.display()
                )
            })?;
            let indexes = packages
                .iter()
                .enumerate()
                .filter_map(|(index, value)| (value.as_str() == Some(package)).then_some(index))
                .collect::<Vec<_>>();
            for index in indexes.into_iter().rev() {
                packages.remove(index);
            }
            if packages.is_empty() {
                empty.push(group.to_owned());
            }
        }
        for group in empty {
            groups.remove(&group);
        }
        if groups.is_empty() {
            document.remove("dependency-groups");
        }
        write_document(&path, &document)
    }

    /// Add `[matrix]` only when the project does not already define it.
    pub fn initialize_matrix(
        project_root: &Path,
        toolchains: &[String],
        command: &[OsString],
    ) -> Result<PathBuf> {
        if toolchains.is_empty() {
            bail!("matrix initialization requires at least one Lean toolchain");
        }
        if command.is_empty() {
            bail!("matrix initialization requires a command");
        }

        let path = project_root.join("lev.toml");
        let source = if path.is_file() {
            fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?
        } else {
            String::new()
        };
        let mut document: DocumentMut = source
            .parse()
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if document.get("matrix").is_some() {
            bail!(
                "matrix configuration already exists in {}; edit it directly",
                path.display()
            );
        }

        let mut matrix = Table::new();
        matrix.insert(
            "toolchains",
            Item::Value(Value::Array(string_values(
                toolchains.iter().map(String::as_str),
            ))),
        );
        matrix.insert(
            "command",
            Item::Value(Value::Array(string_values(
                command
                    .iter()
                    .map(|part| os_string_value(part, &path))
                    .collect::<Result<Vec<_>>>()?,
            ))),
        );
        document.insert("matrix", Item::Table(matrix));

        let mut rendered = document.to_string();
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        atomic_write(&path, rendered.as_bytes())?;
        Ok(path)
    }
}

fn string_values<'a>(values: impl IntoIterator<Item = &'a str>) -> Array {
    let mut array = Array::new();
    for value in values {
        array.push(value);
    }
    array
}

fn read_document(path: &Path) -> Result<DocumentMut> {
    let source = if path.is_file() {
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };
    source
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn write_document(path: &Path, document: &DocumentMut) -> Result<()> {
    let mut rendered = document.to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    atomic_write(path, rendered.as_bytes())
}

fn os_string_value<'a>(value: &'a OsStr, path: &Path) -> Result<&'a str> {
    value.to_str().with_context(|| {
        format!(
            "matrix commands in {} must contain valid UTF-8",
            path.display()
        )
    })
}

fn string_value<'a>(item: &'a Item, field: &str, path: &Path) -> Result<&'a str> {
    item.as_str()
        .with_context(|| format!("{field} in {} must be a string", path.display()))
}

fn string_array(item: &Item, field: &str, path: &Path) -> Result<Vec<String>> {
    let array = item
        .as_array()
        .with_context(|| format!("{field} in {} must be an array of strings", path.display()))?;
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("{field} in {} must contain only strings", path.display()))
        })
        .collect()
}

fn validate_relative_path(path: &Path, selector: &str, config: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!(
            "environments.{selector}.lakefile in {} must stay within the project",
            config.display()
        );
    }
    Ok(())
}

fn valid_package_route(value: &str) -> bool {
    let Some((owner, package)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !package.is_empty()
        && !package.contains('/')
        && [owner, package].into_iter().all(|component| {
            component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn validate_policy_name(value: &str, kind: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
        bail!("invalid {kind} name: {value:?}");
    }
    Ok(())
}

fn validate_revision(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
        bail!("{field} must be a non-empty revision without whitespace");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use tempfile::tempdir;

    use crate::dependency::reservoir::ReservoirPackage;

    use super::LevConfig;

    #[test]
    fn reads_matrix_and_task_configuration() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("lev.toml"),
            r#"
[matrix]
toolchains = ["4.fixture-a", "4.fixture-d"]
command = ["lake", "build", "--wfail"]

[tasks]
ci = ["lake", "test"]
check = ["lake", "build", "--wfail"]

[workspace]
members = ["packages/*", "tools/linter"]
exclude = ["packages/experimental"]

[environments."4.fixture-c"]
auto = false
lakefile = "compat/alternate/lakefile.lean"

[environments."4.fixture-c".dependencies]
solver = "release-c"
"#,
        )
        .unwrap();

        let config = LevConfig::read(temp.path()).unwrap();
        assert_eq!(config.matrix_toolchains, ["4.fixture-a", "4.fixture-d"]);
        assert_eq!(config.matrix_command.len(), 3);
        assert_eq!(config.tasks.len(), 2);
        assert_eq!(config.workspace_members, ["packages/*", "tools/linter"]);
        assert_eq!(config.workspace_exclude, ["packages/experimental"]);
        let environment = &config.environments["leanprover/lean4:v4.fixture-c"];
        assert!(!environment.auto);
        assert_eq!(
            environment.lakefile.as_deref(),
            Some(std::path::Path::new("compat/alternate/lakefile.lean"))
        );
        assert_eq!(environment.dependencies["solver"], "release-c");
    }

    #[test]
    fn reads_and_validates_package_registry_routes() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("lev.toml"),
            r#"
[registries.private]
url = "https://packages.example.invalid/reservoir/api/v1/"
token-env = "PRIVATE_REGISTRY_TOKEN"

[sources]
"acme/private-package" = "private"
"*" = "private"
"#,
        )
        .unwrap();

        let config = LevConfig::read(temp.path()).unwrap();
        let source = config
            .reservoir_source(&ReservoirPackage {
                owner: "acme".to_owned(),
                name: "private-package".to_owned(),
            })
            .unwrap();
        assert_eq!(
            source.api_url,
            "https://packages.example.invalid/reservoir/api/v1"
        );
        assert_eq!(source.token_env.as_deref(), Some("PRIVATE_REGISTRY_TOKEN"));

        fs::write(
            temp.path().join("lev.toml"),
            r#"
[registries.private]
url = "http://packages.example.invalid/api"

[sources]
"acme/private-package" = "missing"
"#,
        )
        .unwrap();
        let error = LevConfig::read(temp.path()).unwrap_err().to_string();
        assert!(error.contains("must use HTTPS"), "{error}");

        fs::write(
            temp.path().join("lev.toml"),
            r#"
[registries.private]
url = "https://packages.example.invalid/api"

[sources]
"acme/private-package" = "missing"
"#,
        )
        .unwrap();
        let error = LevConfig::read(temp.path()).unwrap_err().to_string();
        assert!(error.contains("unknown registry"), "{error}");
    }

    #[test]
    fn edits_dependency_groups_and_enforces_lock_constraints() {
        let temp = tempdir().unwrap();
        LevConfig::add_to_dependency_group(temp.path(), "dev", "test-support").unwrap();
        LevConfig::add_to_dependency_group(temp.path(), "dev", "documentation").unwrap();
        let duplicate =
            LevConfig::add_to_dependency_group(temp.path(), "dev", "documentation").unwrap_err();
        assert!(duplicate.to_string().contains("already in group"));

        let config = LevConfig::read(temp.path()).unwrap();
        assert_eq!(
            config.group_packages("dev").unwrap(),
            ["documentation", "test-support"]
        );

        LevConfig::remove_from_dependency_groups(temp.path(), "documentation").unwrap();
        assert_eq!(
            LevConfig::read(temp.path())
                .unwrap()
                .group_packages("dev")
                .unwrap(),
            ["test-support"]
        );
        LevConfig::remove_from_dependency_groups(temp.path(), "test-support").unwrap();
        assert!(
            LevConfig::read(temp.path())
                .unwrap()
                .dependency_groups
                .is_empty()
        );

        let revision = "0123456789abcdef0123456789abcdef01234567";
        fs::write(
            temp.path().join("lev.toml"),
            format!("[constraints]\ndep = \"{revision}\"\n"),
        )
        .unwrap();
        let manifest: crate::project::manifest::LakeManifest = serde_json::from_str(&format!(
            r#"{{
                "packages": [{{
                    "name": "dep",
                    "type": "git",
                    "rev": "{revision}",
                    "inputRev": "v1.0.0"
                }}]
            }}"#
        ))
        .unwrap();
        LevConfig::read(temp.path())
            .unwrap()
            .verify_constraints(&manifest)
            .unwrap();

        fs::write(
            temp.path().join("lev.toml"),
            "[constraints]\ndep = \"v2.0.0\"\n",
        )
        .unwrap();
        let error = LevConfig::read(temp.path())
            .unwrap()
            .verify_constraints(&manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requested selector"), "{error}");
    }

    #[test]
    fn initializes_matrix_without_replacing_existing_automation() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("lev.toml");
        fs::write(
            &path,
            r#"# Repository-owned commands.
[tasks]
check = ["lake", "build", "--wfail"]
"#,
        )
        .unwrap();

        let written = LevConfig::initialize_matrix(
            temp.path(),
            &[
                "leanprover/lean4:v4.fixture-c".to_owned(),
                "leanprover/lean4:v4.fixture-d".to_owned(),
            ],
            &[
                OsString::from("lake"),
                OsString::from("build"),
                OsString::from("--wfail"),
            ],
        )
        .unwrap();
        assert_eq!(written, path);

        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains("# Repository-owned commands."), "{source}");
        let config = LevConfig::read(temp.path()).unwrap();
        assert_eq!(config.tasks.len(), 1);
        assert_eq!(
            config.matrix_toolchains,
            [
                "leanprover/lean4:v4.fixture-c",
                "leanprover/lean4:v4.fixture-d"
            ]
        );
        assert_eq!(
            config.matrix_command,
            ["lake", "build", "--wfail"].map(OsString::from)
        );

        let error = LevConfig::initialize_matrix(
            temp.path(),
            &["leanprover/lean4:v4.fixture-d".to_owned()],
            &[OsString::from("lake"), OsString::from("test")],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn rejects_a_non_table_matrix_configuration() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("lev.toml"), "matrix = true\n").unwrap();

        let error = LevConfig::read(temp.path()).unwrap_err().to_string();
        assert!(error.contains("must be a TOML table"), "{error}");
    }

    #[test]
    fn rejects_ambiguous_or_escaping_environment_configuration() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("lev.toml"),
            r#"
[environments."4.fixture-c"]
lakefile = "../outside.lean"
"#,
        )
        .unwrap();
        let error = LevConfig::read(temp.path()).unwrap_err().to_string();
        assert!(error.contains("must stay within the project"), "{error}");

        fs::write(
            temp.path().join("lev.toml"),
            r#"
[environments."4.fixture-c"]
auto = true

[environments."v4.fixture-c"]
auto = false
"#,
        )
        .unwrap();
        let error = LevConfig::read(temp.path()).unwrap_err().to_string();
        assert!(error.contains("duplicate environment selectors"), "{error}");
    }
}
