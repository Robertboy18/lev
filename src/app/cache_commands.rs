//! User-facing cache and remote snapshot commands.
//!
//! Cache modules own storage; this module handles command output and options.

use std::collections::HashSet;
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cache::lake_artifacts as lake_artifact_cache;
use crate::cache::registry;
use crate::cache::remote::{self as remote_cache, RemoteCacheIdentity};
use crate::cache::{digest, human_bytes, path_bytes};
use crate::cli::{ArtifactCacheCommand, CacheCommand, RemoteCacheCommand, RemoteSnapshotArgs};
use crate::core::json_output::{self, schema};
use crate::core::signing;
use crate::dependency::resolution as resolution_cache;
use crate::project::Project;
use crate::toolchain;

use super::AppContext;
use super::isolated_environment;

pub(super) fn cache_command(context: &AppContext, command: CacheCommand) -> Result<i32> {
    match command {
        CacheCommand::Dir => {
            println!("{}", context.cache.root.display());
        }
        CacheCommand::Status => {
            let stats = context.cache.stats()?;
            println!("directory: {}", context.cache.root.display());
            println!("size: {}", human_bytes(stats.bytes));
            println!("files: {}", stats.files);
            println!("Git mirrors: {}", stats.git_mirrors);
            println!(
                "shared dependency environments: {}",
                stats.dependency_environments
            );
            println!("Lake toolchain caches: {}", stats.lake_toolchains);
            println!(
                "Reservoir metadata: {} in {} files",
                human_bytes(stats.reservoir_bytes),
                stats.reservoir_files
            );
            println!(
                "dependency resolutions: {} in {} files",
                human_bytes(stats.resolution_bytes),
                stats.resolution_files
            );
            println!("script environments: {}", stats.script_environments);
            println!("local workspaces: {}", stats.workspaces);
            println!("registered project environments: {}", stats.projects);
        }
        CacheCommand::Verify { full } => {
            // These stores have different integrity models: registry records
            // are advisory, Git owns mirror verification, and artifact
            // mappings must resolve to locally stored content.
            let records = registry::load(&context.cache)?;
            let mirrors = context.cache.mirror_paths()?;
            let artifacts = lake_artifact_cache::inspect(&context.cache, None)?;
            let resolutions = resolution_cache::verify(&context.cache)?;
            let mut failures = 0;
            let mut stale_records = 0;

            for (_, record) in &records {
                if !record.root.join("lean-toolchain").is_file() {
                    stale_records += 1;
                    continue;
                }
                if let Some(expected) = &record.manifest_digest {
                    let current = fs::read(record.root.join("lake-manifest.json"))
                        .ok()
                        .map(|bytes| digest(&bytes));
                    if current.as_ref() != Some(expected) {
                        stale_records += 1;
                    }
                }
            }
            for mirror in &mirrors {
                let mut command = Command::new(&context.git);
                command.arg("--git-dir").arg(mirror).arg("fsck");
                if !full {
                    command.arg("--connectivity-only");
                }
                command.arg("--no-dangling");
                let status = command
                    .status()
                    .with_context(|| format!("failed to verify {}", mirror.display()))?;
                if !status.success() {
                    failures += 1;
                    eprintln!("invalid Git mirror: {}", mirror.display());
                }
            }
            println!("verified project records: {}", records.len());
            println!("stale project records: {stale_records}");
            println!("verified Git mirrors: {}", mirrors.len());
            println!("verified dependency resolutions: {}", resolutions.records);
            println!(
                "verified Lake artifact mappings: {}",
                artifacts.stats.mappings
            );
            println!("verified Lake artifacts: {}", artifacts.stats.artifacts);
            for missing in &artifacts.missing {
                eprintln!(
                    "missing Lake artifact: {}\treferenced by {}",
                    missing.artifact,
                    missing.mapping.display()
                );
            }
            failures += artifacts.missing.len();
            if failures > 0 {
                bail!("{failures} cache verification failure(s)");
            }
        }
        CacheCommand::Gc {
            max_age_days,
            apply,
        } => {
            let mut plan = registry::gc_plan(&context.cache, max_age_days)?;
            // Script environments have no persistent installation records.
            // Their last-used marker and age gate are therefore their complete
            // reachability policy.
            for path in isolated_environment::gc_paths(
                &context.cache.script_environment_root(),
                max_age_days,
                &HashSet::new(),
            )? {
                plan.candidates.push(registry::GcCandidate {
                    kind: "script-environment",
                    bytes: path_bytes(&path)?,
                    path,
                });
            }
            plan.candidates
                .sort_by(|left, right| left.path.cmp(&right.path));
            if plan.candidates.is_empty() {
                println!("No unreferenced cache entries are old enough to collect");
                return Ok(0);
            }
            for candidate in &plan.candidates {
                println!(
                    "{}\t{}\t{}",
                    candidate.kind,
                    human_bytes(candidate.bytes),
                    candidate.path.display()
                );
            }
            println!(
                "{} entries, {} reclaimable",
                plan.candidates.len(),
                human_bytes(plan.bytes())
            );
            if apply {
                plan.apply()?;
                println!("Removed {} cache entries", plan.candidates.len());
            } else {
                println!("Dry run; pass --apply to remove these entries");
            }
        }
        CacheCommand::Artifacts { command } => {
            artifact_cache_command(context, command)?;
        }
        CacheCommand::Remote { command } => {
            remote_cache_command(context, command)?;
        }
    }
    Ok(0)
}

fn artifact_cache_command(context: &AppContext, command: ArtifactCacheCommand) -> Result<i32> {
    match command {
        ArtifactCacheCommand::Status { toolchain, json } => {
            let toolchain = normalize_optional_toolchain(toolchain)?;
            let report = lake_artifact_cache::inspect(&context.cache, toolchain.as_deref())?;
            if json {
                json_output::print(schema::CACHE_ARTIFACTS_STATUS, &report)?;
            } else {
                print_artifact_cache_report(&report);
            }
        }
        ArtifactCacheCommand::Verify { toolchain, json } => {
            let toolchain = normalize_optional_toolchain(toolchain)?;
            let report = lake_artifact_cache::inspect(&context.cache, toolchain.as_deref())?;
            if json {
                json_output::print(schema::CACHE_ARTIFACTS_VERIFY, &report)?;
            } else {
                print_artifact_cache_report(&report);
                for missing in &report.missing {
                    eprintln!(
                        "missing {}\treferenced by {}",
                        missing.artifact,
                        missing.mapping.display()
                    );
                }
            }
            if !report.missing.is_empty() {
                bail!(
                    "{} locally-authored Lake artifact reference(s) are missing",
                    report.missing.len()
                );
            }
        }
        ArtifactCacheCommand::Gc {
            toolchain,
            max_age_days,
            apply,
            json,
        } => {
            let toolchain = normalize_optional_toolchain(toolchain)?;
            let transaction =
                lake_artifact_cache::gc_plan(&context.cache, toolchain.as_deref(), max_age_days)?;
            let report = &transaction.report;
            let plan = &transaction.plan;
            if !report.missing.is_empty() {
                bail!(
                    "refusing artifact GC while {} local reference(s) are missing; run `lev cache artifacts verify`",
                    report.missing.len()
                );
            }
            let bytes = plan.bytes();
            if json {
                let payload = serde_json::json!({
                    "report": report,
                    "candidates": &plan.candidates,
                    "reclaimable_bytes": bytes,
                    "applied": apply,
                });
                json_output::print(schema::CACHE_ARTIFACTS_GC, &payload)?;
            } else if plan.candidates.is_empty() {
                println!("No unreferenced Lake artifacts are old enough to collect");
            } else {
                for candidate in &plan.candidates {
                    println!(
                        "lake-artifact\t{}\t{}",
                        human_bytes(candidate.bytes),
                        candidate.path.display()
                    );
                }
                println!(
                    "{} artifacts, {} reclaimable",
                    plan.candidates.len(),
                    human_bytes(bytes)
                );
            }
            if apply {
                transaction.apply()?;
                if !json {
                    println!("Removed {} Lake artifacts", plan.candidates.len());
                }
            } else if !json && !plan.candidates.is_empty() {
                println!("Dry run; pass --apply to remove these artifacts");
            }
        }
    }
    Ok(0)
}

fn normalize_optional_toolchain(toolchain: Option<String>) -> Result<Option<String>> {
    toolchain
        .map(|toolchain| toolchain::normalize(&toolchain))
        .transpose()
}

fn print_artifact_cache_report(report: &lake_artifact_cache::ArtifactCacheReport) {
    let stats = &report.stats;
    println!("Lake toolchain caches: {}", stats.toolchain_caches);
    println!(
        "artifacts: {} ({})",
        stats.artifacts,
        human_bytes(stats.artifact_bytes)
    );
    println!("input-to-output mappings: {}", stats.mappings);
    println!("referenced artifacts: {}", stats.referenced_artifacts);
    println!(
        "hard-linked artifacts: {} ({})",
        stats.hardlinked_artifacts,
        human_bytes(stats.hardlinked_bytes)
    );
    println!(
        "unreferenced artifacts: {} ({})",
        stats.unreferenced_artifacts,
        human_bytes(stats.unreferenced_bytes)
    );
    println!("missing local artifacts: {}", stats.missing_local_artifacts);
}

fn remote_cache_command(context: &AppContext, command: RemoteCacheCommand) -> Result<i32> {
    match command {
        RemoteCacheCommand::Keygen {
            private_key,
            public_key,
            force,
        } => {
            let generated = signing::generate_key_pair(&private_key, &public_key, force)?;
            println!("private key: {}", private_key.display());
            println!("public key: {}", public_key.display());
            println!("fingerprint: {}", generated.fingerprint);
        }
        RemoteCacheCommand::Push {
            remote,
            signing_key,
            snapshot,
        } => {
            let RemoteSnapshotArgs {
                namespace,
                revision,
                toolchain,
                platform,
                allow_insecure_http,
                json,
            } = snapshot;
            let identity =
                resolve_remote_identity(context, namespace, revision, toolchain, platform)?;
            let report = remote_cache::push(
                &context.cache,
                &remote,
                &identity,
                &signing_key,
                allow_insecure_http,
            )?;
            if json {
                json_output::print(schema::CACHE_REMOTE_PUSH, &report)?;
            } else {
                println!("manifest: {}", report.manifest);
                println!("entries: {}", report.entries);
                println!(
                    "artifacts: {}, mappings: {}",
                    report.artifacts, report.mappings
                );
                println!(
                    "blobs: {} uploaded, {} reused",
                    report.blobs_uploaded, report.blobs_reused
                );
                println!(
                    "uploaded: {}",
                    human_bytes(report.compressed_bytes_uploaded)
                );
                println!("signing key: {}", report.signing_key_fingerprint);
            }
        }
        RemoteCacheCommand::Pull {
            remote,
            public_key,
            snapshot,
        } => {
            let RemoteSnapshotArgs {
                namespace,
                revision,
                toolchain,
                platform,
                allow_insecure_http,
                json,
            } = snapshot;
            let identity =
                resolve_remote_identity(context, namespace, revision, toolchain, platform)?;
            let report = remote_cache::pull(
                &context.cache,
                &remote,
                &identity,
                &public_key,
                allow_insecure_http,
            )?;
            if json {
                json_output::print(schema::CACHE_REMOTE_PULL, &report)?;
            } else {
                println!("manifest: {}", report.manifest);
                println!("entries: {}", report.entries);
                println!(
                    "files: {} created, {} reused",
                    report.files_created, report.files_reused
                );
                println!(
                    "downloaded: {}",
                    human_bytes(report.compressed_bytes_downloaded)
                );
                println!("trusted key: {}", report.trusted_key_fingerprint);
            }
        }
    }
    Ok(0)
}

fn resolve_remote_identity(
    context: &AppContext,
    namespace: String,
    revision: Option<String>,
    requested_toolchain: Option<String>,
    platform: Option<String>,
) -> Result<RemoteCacheIdentity> {
    // A project is optional when every identity field was supplied.
    let project = Project::discover(&context.project_start).ok();
    let toolchain = match requested_toolchain {
        Some(toolchain) => toolchain::normalize(&toolchain)?,
        None => project
            .as_ref()
            .map(|project| project.toolchain.clone())
            .context("no project found; pass --toolchain explicitly")?,
    };
    let revision = match revision {
        Some(revision) => revision,
        None => {
            let project = project
                .as_ref()
                .context("no project found; pass --revision explicitly")?;
            // Prefer lev.lock because it captures the complete environment.
            // A Lake manifest digest is still a stable identity for projects
            // that have not adopted lev's lock file.
            let source = if project.lock_path().is_file() {
                project.lock_path()
            } else if project.manifest_path().is_file() {
                project.manifest_path()
            } else {
                bail!("neither lev.lock nor lake-manifest.json exists; pass --revision explicitly");
            };
            let bytes = fs::read(&source)
                .with_context(|| format!("failed to read {}", source.display()))?;
            digest(&bytes)
        }
    };
    RemoteCacheIdentity::new(
        namespace,
        revision,
        toolchain,
        platform.unwrap_or_else(crate::core::platform::host_id),
    )
}
