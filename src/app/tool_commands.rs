//! Reusable Lake environments for package executables.
//!
//! Installed names point to locked environments. One-shot runs reuse the same
//! cache without creating an installation record.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cache::{digest, human_bytes, path_bytes};
use crate::cli::{ToolCommand, ToolInstallArgs, ToolRunArgs, ToolSourceArgs};
use crate::core::atomic_file::replace as atomic_write;
use crate::core::bounded_io;
use crate::core::json_output::{self, schema};
use crate::core::process::{checked_status, exit_code, passthrough_status};
use crate::project::Project;
use crate::project::lakefile::{self, Dependency, DependencySource};
use crate::toolchain;

use super::AppContext;
use super::isolated_environment;

const TOOL_FORMAT_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_INSTALLED_TOOLS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ToolSpec {
    toolchain: String,
    package: String,
    executable: String,
    source: ToolSource,
    revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ToolSource {
    Registry { scope: Option<String> },
    Git { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolRecord {
    version: u32,
    name: String,
    environment: String,
    spec: ToolSpec,
}

#[derive(Debug, Serialize)]
struct ToolListing<'a> {
    name: &'a str,
    package: &'a str,
    executable: &'a str,
    toolchain: &'a str,
    revision: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct ToolGcCandidate {
    environment: String,
    path: PathBuf,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct ToolGcReport {
    installed_tools: usize,
    candidates: Vec<ToolGcCandidate>,
    reclaimable_bytes: u64,
    applied: bool,
}

pub(super) fn tool_command(context: &AppContext, command: ToolCommand) -> Result<i32> {
    match command {
        ToolCommand::Install(args) => install(context, args),
        ToolCommand::Run(args) => run(context, args),
        ToolCommand::List { json } => list(context, json),
        ToolCommand::Remove { name } => remove(context, &name),
        ToolCommand::Gc {
            max_age_days,
            apply,
            json,
        } => gc(context, max_age_days, apply, json),
    }
}

fn install(context: &AppContext, args: ToolInstallArgs) -> Result<i32> {
    let spec = spec_from_args(context, args.source)?;
    let name = args.name.unwrap_or_else(|| spec.executable.clone());
    validate_tool_name(&name, "installed tool")?;
    let project = ensure_environment(context, &spec, args.offline)?;
    let record = ToolRecord {
        version: TOOL_FORMAT_VERSION,
        name: name.clone(),
        environment: environment_key(&spec)?,
        spec,
    };
    write_record(context, &record)?;
    context.info(format!(
        "installed tool {name} in {}",
        project.root.display()
    ));
    println!("Run with: lev tool run {name}");
    Ok(0)
}

fn run(context: &AppContext, args: ToolRunArgs) -> Result<i32> {
    let ToolRunArgs {
        source,
        offline,
        args,
    } = args;
    let spec = run_spec(context, source)?;
    let project = ensure_environment(context, &spec, offline)?;
    let mut command = context.runtime_command(&spec.toolchain, OsStr::new("lake"), false)?;
    command
        .arg("exe")
        .arg(&spec.executable)
        .arg("--")
        .args(args);
    context.command_env(&mut command, &project)?;
    Ok(exit_code(passthrough_status(&mut command)?))
}

fn list(context: &AppContext, json: bool) -> Result<i32> {
    let records = installed_records(context)?;
    if json {
        let listings = records.iter().map(ToolListing::from).collect::<Vec<_>>();
        json_output::print(schema::TOOL_LIST, &listings)?;
        return Ok(0);
    }
    if records.is_empty() {
        println!("No tools installed");
        return Ok(0);
    }
    println!(
        "{:<20} {:<24} {:<20} TOOLCHAIN",
        "NAME", "PACKAGE", "EXECUTABLE"
    );
    for record in records {
        println!(
            "{:<20} {:<24} {:<20} {}",
            record.name, record.spec.package, record.spec.executable, record.spec.toolchain
        );
    }
    Ok(0)
}

fn remove(context: &AppContext, name: &str) -> Result<i32> {
    validate_tool_name(name, "installed tool")?;
    let path = record_path(context, name)?;
    let record = read_record(&path)?.with_context(|| format!("tool {name:?} is not installed"))?;
    if record.name != name {
        bail!("installed tool record does not match requested name {name:?}");
    }
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    context.info(format!("removed installed tool {name}"));
    Ok(0)
}

fn gc(context: &AppContext, max_age_days: u64, apply: bool, json: bool) -> Result<i32> {
    let records = installed_records(context)?;
    let live = records
        .iter()
        .map(|record| record.environment.clone())
        .collect::<HashSet<_>>();
    let root = tools_root(context)?;
    let paths = isolated_environment::gc_paths(&root.join("environments"), max_age_days, &live)?;
    let mut candidates = Vec::with_capacity(paths.len());
    for path in paths {
        let environment = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("tool environment path has no UTF-8 key")?
            .to_owned();
        candidates.push(ToolGcCandidate {
            environment,
            bytes: path_bytes(&path)?,
            path,
        });
    }
    let reclaimable_bytes = candidates.iter().map(|candidate| candidate.bytes).sum();
    let report = ToolGcReport {
        installed_tools: records.len(),
        candidates,
        reclaimable_bytes,
        applied: apply,
    };

    if json {
        json_output::print(schema::TOOL_GC, &report)?;
    } else if report.candidates.is_empty() {
        println!("No unreferenced tool environments are old enough to collect");
    } else {
        for candidate in &report.candidates {
            println!(
                "tool-environment\t{}\t{}",
                human_bytes(candidate.bytes),
                candidate.path.display()
            );
        }
        println!(
            "{} environments, {} reclaimable",
            report.candidates.len(),
            human_bytes(report.reclaimable_bytes)
        );
    }

    if apply {
        for candidate in &report.candidates {
            fs::remove_dir_all(&candidate.path)
                .with_context(|| format!("failed to remove {}", candidate.path.display()))?;
        }
        if !json {
            println!("Removed {} tool environments", report.candidates.len());
        }
    } else if !json && !report.candidates.is_empty() {
        println!("Dry run; pass --apply to remove these environments");
    }
    Ok(0)
}

fn run_spec(context: &AppContext, mut args: ToolSourceArgs) -> Result<ToolSpec> {
    // A bare installed name resolves through its record. Any source option
    // turns the command into an explicit one-shot specification instead.
    let has_source_override =
        args.git.is_some() || args.scope.is_some() || args.rev.is_some() || args.lean.is_some();
    if !has_source_override {
        if let Some(record) = read_record(&record_path(context, &args.package)?)? {
            let mut spec = record.spec;
            if let Some(executable) = args.exe.take() {
                validate_tool_name(&executable, "tool executable")?;
                spec.executable = executable;
            }
            return Ok(spec);
        }
    }
    spec_from_args(context, args)
}

fn spec_from_args(context: &AppContext, args: ToolSourceArgs) -> Result<ToolSpec> {
    validate_tool_name(&args.package, "package")?;
    let executable = args.exe.unwrap_or_else(|| args.package.clone());
    validate_tool_name(&executable, "tool executable")?;
    let toolchain = match args.lean {
        Some(toolchain) => toolchain::normalize(&toolchain)?,
        None => context
            .project()
            .map(|project| project.toolchain)
            .with_context(|| "tool execution outside a Lean project requires --lean <TOOLCHAIN>")?,
    };
    let source = if let Some(url) = args.git {
        if url.trim().is_empty() {
            bail!("tool Git URL cannot be empty");
        }
        ToolSource::Git { url }
    } else {
        ToolSource::Registry { scope: args.scope }
    };
    let revision = args.rev;
    if revision.as_deref().is_some_and(|revision| {
        revision.trim().is_empty() || revision.contains(char::is_whitespace)
    }) {
        bail!("tool revision must be non-empty and contain no whitespace");
    }
    Ok(ToolSpec {
        toolchain,
        package: args.package,
        executable,
        source,
        revision,
    })
}

fn ensure_environment(context: &AppContext, spec: &ToolSpec, offline: bool) -> Result<Project> {
    validate_spec(spec)?;
    // The complete normalized specification is the environment identity.
    // Different revisions, sources, executables, or toolchains cannot share a
    // mutable Lake tree by accident.
    let key = environment_key(spec)?;
    let root = tools_root(context)?;
    isolated_environment::ensure(
        context,
        isolated_environment::Request {
            environments: &root.join("environments"),
            locks: &root.join("locks"),
            key: &key,
            spec,
            toolchain: &spec.toolchain,
            offline,
        },
        |project_root| initialize_environment(spec, project_root),
        |project| build_executable(context, spec, project),
    )
}

fn initialize_environment(spec: &ToolSpec, root: &Path) -> Result<()> {
    // Tool packages live in a tiny synthetic Lake project. This keeps package
    // resolution standard while separating it from whichever project invoked
    // `lev tool`.
    fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
    atomic_write(
        &root.join("lean-toolchain"),
        format!("{}\n", spec.toolchain).as_bytes(),
    )?;
    atomic_write(
        &root.join("lakefile.toml"),
        b"[package]\nname = \"lev_tool_host\"\n",
    )?;
    let source = match &spec.source {
        ToolSource::Registry { scope } => DependencySource::Registry {
            scope: scope.clone(),
        },
        ToolSource::Git { url } => DependencySource::Git { url: url.clone() },
    };
    lakefile::add(
        &root.join("lakefile.toml"),
        &Dependency {
            name: spec.package.clone(),
            source,
            rev: spec.revision.clone(),
        },
        false,
    )
}

fn build_executable(context: &AppContext, spec: &ToolSpec, project: &Project) -> Result<()> {
    let mut build = context.runtime_command(&spec.toolchain, OsStr::new("lake"), false)?;
    build.arg("build").arg(&spec.executable);
    context.command_env(&mut build, project)?;
    checked_status(&mut build)
}

fn write_record(context: &AppContext, record: &ToolRecord) -> Result<()> {
    // Installation records are small aliases to content-keyed environments;
    // removing an alias does not eagerly destroy a tree another alias may use.
    validate_record(record)?;
    let path = record_path(context, &record.name)?;
    let parent = path.parent().context("installed tool path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    atomic_write(&path, &serde_json::to_vec_pretty(record)?)
}

fn installed_records(context: &AppContext) -> Result<Vec<ToolRecord>> {
    let directory = tools_root(context)?.join("installed");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(&directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", directory.display()))?;
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        {
            let record: ToolRecord = bounded_io::read_json_file(&entry.path(), MAX_RECORD_BYTES)?;
            validate_record(&record)?;
            records.push(record);
            if records.len() > MAX_INSTALLED_TOOLS {
                bail!("installed tool directory contains more than {MAX_INSTALLED_TOOLS} records");
            }
        }
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(records)
}

fn read_record(path: &Path) -> Result<Option<ToolRecord>> {
    if !path.is_file() {
        return Ok(None);
    }
    let record = bounded_io::read_json_file(path, MAX_RECORD_BYTES)?;
    validate_record(&record)?;
    Ok(Some(record))
}

fn validate_record(record: &ToolRecord) -> Result<()> {
    if record.version != TOOL_FORMAT_VERSION {
        bail!("installed tool {} uses an unsupported format", record.name);
    }
    validate_tool_name(&record.name, "installed tool")?;
    validate_spec(&record.spec)?;
    if record.environment != environment_key(&record.spec)? {
        bail!(
            "installed tool {} has an invalid environment key",
            record.name
        );
    }
    Ok(())
}

fn validate_spec(spec: &ToolSpec) -> Result<()> {
    validate_tool_name(&spec.package, "package")?;
    validate_tool_name(&spec.executable, "tool executable")?;
    if toolchain::normalize(&spec.toolchain)? != spec.toolchain {
        bail!("tool specification contains a non-canonical toolchain");
    }
    if matches!(&spec.source, ToolSource::Git { url } if url.trim().is_empty()) {
        bail!("tool specification contains an empty Git URL");
    }
    Ok(())
}

fn environment_key(spec: &ToolSpec) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(
        "lev-tool-environment-v1",
        spec,
    ))?))
}

fn record_path(context: &AppContext, name: &str) -> Result<PathBuf> {
    validate_tool_name(name, "installed tool")?;
    Ok(tools_root(context)?
        .join("installed")
        .join(format!("{}.json", digest(name.as_bytes()))))
}

fn tools_root(context: &AppContext) -> Result<PathBuf> {
    let data = context
        .store
        .root
        .parent()
        .context("toolchain store has no data-directory parent")?;
    Ok(data.join("tools-v1"))
}

fn validate_tool_name(value: &str, kind: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || matches!(value, "." | "..")
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || value.contains(['/', '\\'])
    {
        bail!("invalid {kind} name: {value:?}");
    }
    Ok(())
}

impl<'a> From<&'a ToolRecord> for ToolListing<'a> {
    fn from(record: &'a ToolRecord) -> Self {
        Self {
            name: &record.name,
            package: &record.spec.package,
            executable: &record.spec.executable,
            toolchain: &record.spec.toolchain,
            revision: record.spec.revision.as_deref(),
        }
    }
}
