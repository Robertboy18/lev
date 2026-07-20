//! Reusable environments for standalone Lean source files.
//!
//! Metadata uses a comment-delimited TOML block:
//!
//! ```text
//! -- /// lev
//! -- lean = "nightly"
//! --
//! -- [[dependencies]]
//! -- name = "example"
//! -- scope = "owner"
//! -- rev = "v1.2.3"
//! -- ///
//! ```
//!
//! Only metadata affects the environment key, so editing the Lean body does
//! not rebuild its dependencies.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cache::digest;
use crate::cli::{ScriptCommand, ScriptSourceArgs};
use crate::core::atomic_file::replace as atomic_write;
use crate::core::process::{exit_code, passthrough_status};
use crate::project::lakefile::{self, Dependency, DependencySource};
use crate::project::{Project, absolute};
use crate::toolchain;

use super::AppContext;
use super::isolated_environment;

const MAX_SCRIPT_BYTES: u64 = 16 * 1024 * 1024;
const BLOCK_START: &str = "-- /// lev";
const BLOCK_END: &str = "-- ///";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetadata {
    lean: Option<String>,
    #[serde(default)]
    dependencies: Vec<RawDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDependency {
    name: String,
    git: Option<String>,
    scope: Option<String>,
    rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScriptSpec {
    toolchain: String,
    dependencies: Vec<ScriptDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScriptDependency {
    name: String,
    source: ScriptDependencySource,
    revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ScriptDependencySource {
    Registry { scope: Option<String> },
    Git { url: String },
}

pub(super) fn script_command(context: &AppContext, command: ScriptCommand) -> Result<i32> {
    match command {
        ScriptCommand::Run(args) => execute(context, args.script, true, args.args),
        ScriptCommand::Check(args) => execute(context, args.script, false, Vec::new()),
    }
}

fn execute(
    context: &AppContext,
    args: ScriptSourceArgs,
    run_main: bool,
    program_args: Vec<OsString>,
) -> Result<i32> {
    let file = canonical_script(&args.file)?;
    let source =
        fs::read_to_string(&file).with_context(|| format!("failed to read {}", file.display()))?;
    let raw = parse_metadata(&source)
        .with_context(|| format!("failed to parse inline metadata in {}", file.display()))?;
    let spec = normalize_metadata(&file, raw, args.lean.as_deref())?;
    let key = environment_key(&spec)?;
    let root = context.cache.script_root();
    let project = isolated_environment::ensure(
        context,
        isolated_environment::Request {
            environments: &root.join("environments"),
            locks: &root.join("locks"),
            key: &key,
            spec: &spec,
            toolchain: &spec.toolchain,
            offline: args.offline,
        },
        |project_root| initialize_environment(&spec, project_root),
        |_| Ok(()),
    )?;

    let mut command = context.runtime_command(&spec.toolchain, OsStr::new("lake"), false)?;
    command.arg("env").arg("lean");
    if run_main {
        command.arg("--run");
    }
    command.arg(&file);
    if !program_args.is_empty() {
        command.arg("--").args(program_args);
    }
    context.command_env(&mut command, &project)?;
    Ok(exit_code(passthrough_status(&mut command)?))
}

fn canonical_script(path: &Path) -> Result<PathBuf> {
    let path = absolute(path)?;
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("script is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_SCRIPT_BYTES {
        bail!("script exceeds the 16 MiB input limit: {}", path.display());
    }
    fs::canonicalize(&path).with_context(|| format!("failed to resolve {}", path.display()))
}

fn parse_metadata(source: &str) -> Result<RawMetadata> {
    let mut block = None;
    let mut current = None::<Vec<String>>;
    for line in source.lines() {
        let line = line.trim_start();
        if line == BLOCK_START {
            if current.is_some() || block.is_some() {
                bail!("script contains more than one {BLOCK_START:?} block");
            }
            current = Some(Vec::new());
            continue;
        }
        let Some(lines) = current.as_mut() else {
            continue;
        };
        if line == BLOCK_END {
            block = current.take();
            continue;
        }
        let content = line.strip_prefix("--").with_context(|| {
            format!("every inline metadata line must be a Lean line comment, found {line:?}")
        })?;
        lines.push(content.strip_prefix(' ').unwrap_or(content).to_owned());
    }
    if current.is_some() {
        bail!("inline metadata block is missing its {BLOCK_END:?} terminator");
    }
    let block = block.with_context(|| {
        format!("script has no {BLOCK_START:?} metadata block; add one or use a Lake project")
    })?;
    let document = block.join("\n");
    toml_edit::de::from_str(&document).context("inline metadata is not valid TOML")
}

fn normalize_metadata(
    file: &Path,
    raw: RawMetadata,
    override_toolchain: Option<&str>,
) -> Result<ScriptSpec> {
    let toolchain = if let Some(toolchain) = override_toolchain.or(raw.lean.as_deref()) {
        toolchain::normalize(toolchain)?
    } else {
        Project::discover(file)
            .map(|project| project.toolchain)
            .with_context(|| {
                "standalone script metadata must declare `lean`, or the command must pass --lean"
            })?
    };
    let mut names = BTreeSet::new();
    let mut dependencies = Vec::with_capacity(raw.dependencies.len());
    for dependency in raw.dependencies {
        validate_name(&dependency.name, "dependency")?;
        if !names.insert(dependency.name.clone()) {
            bail!(
                "inline metadata declares dependency {:?} more than once",
                dependency.name
            );
        }
        if dependency.git.is_some() && dependency.scope.is_some() {
            bail!(
                "dependency {} cannot set both `git` and `scope`",
                dependency.name
            );
        }
        let source = if let Some(url) = dependency.git {
            if url.trim().is_empty() {
                bail!("dependency {} has an empty Git URL", dependency.name);
            }
            ScriptDependencySource::Git { url }
        } else {
            ScriptDependencySource::Registry {
                scope: dependency.scope,
            }
        };
        let revision = dependency.rev;
        if revision.as_deref().is_some_and(|revision| {
            revision.trim().is_empty() || revision.contains(char::is_whitespace)
        }) {
            bail!("dependency {} has an invalid revision", dependency.name);
        }
        dependencies.push(ScriptDependency {
            name: dependency.name,
            source,
            revision,
        });
    }
    Ok(ScriptSpec {
        toolchain,
        dependencies,
    })
}

fn initialize_environment(spec: &ScriptSpec, root: &Path) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
    atomic_write(
        &root.join("lean-toolchain"),
        format!("{}\n", spec.toolchain).as_bytes(),
    )?;
    let lakefile = root.join("lakefile.toml");
    atomic_write(&lakefile, b"[package]\nname = \"lev_script_host\"\n")?;
    for dependency in &spec.dependencies {
        let source = match &dependency.source {
            ScriptDependencySource::Registry { scope } => DependencySource::Registry {
                scope: scope.clone(),
            },
            ScriptDependencySource::Git { url } => DependencySource::Git { url: url.clone() },
        };
        lakefile::add(
            &lakefile,
            &Dependency {
                name: dependency.name.clone(),
                source,
                rev: dependency.revision.clone(),
            },
            false,
        )?;
    }
    Ok(())
}

fn environment_key(spec: &ScriptSpec) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(
        "lev-script-environment-v1",
        spec,
    ))?))
}

fn validate_name(value: &str, kind: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("invalid {kind} name: {value:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        RawDependency, RawMetadata, ScriptDependencySource, normalize_metadata, parse_metadata,
    };

    #[test]
    fn parses_comment_delimited_toml_and_rejects_ambiguous_blocks() {
        let source = r#"
-- /// lev
-- lean = "nightly"
--
-- [[dependencies]]
-- name = "example"
-- scope = "owner"
-- ///

def main : IO Unit := IO.println "hello"
"#;
        let metadata = parse_metadata(source).unwrap();
        assert_eq!(metadata.lean.as_deref(), Some("nightly"));
        assert_eq!(metadata.dependencies.len(), 1);
        assert_eq!(metadata.dependencies[0].name, "example");

        let duplicate = format!("{source}\n{source}");
        assert!(
            parse_metadata(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("more than one")
        );
        assert!(parse_metadata("def value := 1").is_err());
    }

    #[test]
    fn package_names_do_not_infer_registry_scope_or_revision() {
        let spec = normalize_metadata(
            Path::new("Example.lean"),
            RawMetadata {
                lean: Some("nightly".to_owned()),
                dependencies: vec![RawDependency {
                    name: "ordinary-package".to_owned(),
                    git: None,
                    scope: None,
                    rev: None,
                }],
            },
            None,
        )
        .unwrap();

        assert_eq!(
            spec.dependencies[0].source,
            ScriptDependencySource::Registry { scope: None }
        );
        assert_eq!(spec.dependencies[0].revision, None);
    }
}
