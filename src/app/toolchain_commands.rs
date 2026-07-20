//! Toolchain installation, selection, linking, and removal.
//!
//! Backends include lev's store, signed chunks, direct releases, and elan.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cache::registry;
use crate::cache::{human_bytes, path_bytes};
use crate::cli::{
    ToolchainBackend, ToolchainChunksCommand, ToolchainCommand, ToolchainIndexCommand,
    ToolchainStoreCommand,
};
use crate::core::json_output::{self, schema};
use crate::core::process::{
    checked_output, checked_status, exit_code, output_text, passthrough_status,
};
use crate::project::lockfile;
use crate::project::{Project, absolute};
use crate::toolchain::index as toolchain_index;
use crate::toolchain::{self, chunks as toolchain_chunks, download as toolchain_download};

use super::AppContext;
use crate::core::atomic_file::replace as atomic_write;

use super::transaction::FileTransaction;

/// Dispatch toolchain inspection, installation, transport, and GC commands.
pub(super) fn toolchain_command(context: &AppContext, command: ToolchainCommand) -> Result<i32> {
    match command {
        ToolchainCommand::List { sizes } => list_toolchains(context, sizes),
        ToolchainCommand::Install {
            toolchain,
            backend,
            allow_unverified,
            remote,
            public_key,
            allow_insecure_http,
            pin,
        } => {
            let toolchain = toolchain::normalize(&toolchain)?;
            install_toolchain(
                context,
                &toolchain,
                ToolchainInstallOptions {
                    backend,
                    allow_unverified,
                    remote,
                    public_key,
                    allow_insecure_http,
                    pin,
                },
            )?;
            Ok(0)
        }
        ToolchainCommand::Remove { toolchain } => {
            let toolchain = toolchain::normalize(&toolchain)?;
            if let Some(stored) = context.store.find(&toolchain)? {
                context.store.remove(&toolchain)?;
                if elan_available(context) {
                    let mut command = Command::new(&context.elan);
                    command.arg("toolchain").arg("uninstall").arg(&stored.alias);
                    checked_status(&mut command)?;
                }
                context.info(format!("removed lev toolchain {toolchain}"));
                return Ok(0);
            }
            let mut command = Command::new(&context.elan);
            command.arg("toolchain").arg("uninstall").arg(&toolchain);
            Ok(exit_code(passthrough_status(&mut command)?))
        }
        ToolchainCommand::Which { toolchain } => {
            let toolchain = toolchain::normalize(&toolchain)?;
            println!("{}", toolchain_path(context, &toolchain)?.display());
            Ok(0)
        }
        ToolchainCommand::Import { toolchain, pin } => {
            let project = if pin { Some(context.project()?) } else { None };
            let toolchain = toolchain::normalize(&toolchain)?;
            ensure_elan_toolchain(context, &toolchain, true)?;
            let source = elan_toolchain_path(context, &toolchain)?;
            context.info(format!(
                "importing {toolchain} from {} into {}",
                source.display(),
                context.store.root.display()
            ));
            let imported = context.store.import(&toolchain, &source)?;
            register_with_elan_if_available(context, &imported.alias, &imported.view)?;

            if project.is_some() {
                pin_project_toolchain(context, &toolchain)?;
            }

            println!("alias: {}", imported.alias);
            println!("view: {}", imported.view.display());
            println!("files: {}", imported.files);
            println!("logical size: {}", human_bytes(imported.logical_bytes));
            println!("new objects: {}", human_bytes(imported.new_object_bytes));
            println!("reused objects: {}", human_bytes(imported.reused_bytes));
            Ok(0)
        }
        ToolchainCommand::Store { command } => match command {
            ToolchainStoreCommand::Status => {
                let stats = context.store.stats()?;
                println!("directory: {}", context.store.root.display());
                println!("manifests: {}", stats.manifests);
                println!("views: {}", stats.views);
                println!("objects: {}", stats.objects);
                println!("logical size: {}", human_bytes(stats.logical_bytes));
                println!("object size: {}", human_bytes(stats.object_bytes));
                println!(
                    "deduplicated: {}",
                    human_bytes(stats.logical_bytes.saturating_sub(stats.object_bytes))
                );
                Ok(0)
            }
            ToolchainStoreCommand::Verify => {
                let verified = context.store.verify()?;
                println!("verified manifests: {}", verified.manifests);
                println!("verified views: {}", verified.views);
                println!("verified objects: {}", verified.objects);
                Ok(0)
            }
            ToolchainStoreCommand::Gc { apply } => {
                let report = context.store.gc(apply)?;
                println!("unreferenced manifests: {}", report.manifests);
                println!("unreferenced views: {}", report.views);
                println!("unreferenced objects: {}", report.objects);
                println!(
                    "reclaimable object data: {}",
                    human_bytes(report.object_bytes)
                );
                if apply {
                    println!("Removed unreferenced toolchain-store entries");
                } else {
                    println!("Dry run; pass --apply to remove these entries");
                }
                Ok(0)
            }
        },
        ToolchainCommand::Chunks { command } => toolchain_chunks_command(context, command),
        ToolchainCommand::Index { command } => toolchain_index_command(command),
        ToolchainCommand::Gc { delete, json } => {
            let report = context.store.gc(delete)?;
            if !json {
                println!("lev store manifests: {}", report.manifests);
                println!("lev store views: {}", report.views);
                println!("lev store objects: {}", report.objects);
                println!(
                    "lev store reclaimable: {}",
                    human_bytes(report.object_bytes)
                );
            }
            if json {
                let elan = if elan_available(context) {
                    let mut command = Command::new(&context.elan);
                    command.arg("toolchain").arg("gc");
                    if delete {
                        command.arg("--delete");
                    }
                    command.arg("--json");
                    let output = checked_output(&mut command)?;
                    Some(
                        serde_json::from_slice::<serde_json::Value>(&output.stdout)
                            .context("elan toolchain GC returned invalid JSON")?,
                    )
                } else {
                    None
                };
                let payload = serde_json::json!({
                    "store": {
                        "manifests": report.manifests,
                        "views": report.views,
                        "objects": report.objects,
                        "object_bytes": report.object_bytes,
                        "applied": delete,
                    },
                    "elan": elan,
                });
                json_output::print(schema::TOOLCHAIN_GC, &payload)?;
                return Ok(0);
            }

            if !elan_available(context) {
                return Ok(0);
            }

            let mut command = Command::new(&context.elan);
            command.arg("toolchain").arg("gc");
            if delete {
                command.arg("--delete");
            }
            Ok(exit_code(passthrough_status(&mut command)?))
        }
    }
}

fn toolchain_chunks_command(context: &AppContext, command: ToolchainChunksCommand) -> Result<i32> {
    match command {
        ToolchainChunksCommand::Publish {
            toolchain,
            remote,
            source,
            signing_key,
            platform,
            allow_insecure_http,
            json,
        } => {
            let toolchain = toolchain::normalize(&toolchain)?;
            let source = match source {
                Some(source) => absolute(&source)?,
                None => {
                    if let Some(stored) = context.store.find_source(&toolchain)? {
                        stored.view
                    } else {
                        ensure_toolchain_name(context, &toolchain, false)?;
                        toolchain_path(context, &toolchain)?
                    }
                }
            };
            let platform = platform.unwrap_or_else(crate::core::platform::host_id);
            context.info(format!(
                "publishing signed chunks for {toolchain} from {}",
                source.display()
            ));
            let report = toolchain_chunks::publish(
                &context.store,
                &source,
                &toolchain,
                &platform,
                &remote,
                &signing_key,
                allow_insecure_http,
            )?;
            if json {
                json_output::print(schema::TOOLCHAIN_CHUNKS_PUBLISH, &report)?;
            } else {
                println!("manifest: {}", report.manifest);
                println!("files: {}", report.files);
                println!("logical size: {}", human_bytes(report.logical_bytes));
                println!(
                    "chunks: {} uploaded, {} reused",
                    report.chunks_uploaded, report.chunks_reused
                );
                println!(
                    "uploaded: {}",
                    human_bytes(report.compressed_bytes_uploaded)
                );
                println!("signing key: {}", report.signing_key_fingerprint);
            }
        }
        ToolchainChunksCommand::Install {
            toolchain,
            remote,
            public_key,
            platform,
            allow_insecure_http,
            pin,
            json,
        } => {
            if pin {
                context.project()?;
            }
            let toolchain = toolchain::normalize(&toolchain)?;
            let platform = platform.unwrap_or_else(crate::core::platform::host_id);
            let indexed_manifest_sha256 = toolchain_index::require_if_indexed(
                &remote,
                &public_key,
                allow_insecure_http,
                &toolchain,
                &platform,
            )?;
            context.info(format!(
                "installing signed incremental toolchain {toolchain}"
            ));
            let report = toolchain_chunks::install(
                &context.store,
                &toolchain,
                &platform,
                &remote,
                &public_key,
                indexed_manifest_sha256.as_deref(),
                allow_insecure_http,
            )?;
            register_with_elan_if_available(
                context,
                &report.imported.alias,
                &report.imported.view,
            )?;
            if pin {
                pin_project_toolchain(context, &toolchain)?;
            }
            if json {
                json_output::print(schema::TOOLCHAIN_CHUNKS_INSTALL, &report)?;
            } else {
                println!("alias: {}", report.imported.alias);
                println!("view: {}", report.imported.view.display());
                println!("manifest: {}", report.manifest);
                println!(
                    "complete files reused: {} ({})",
                    report.complete_files_reused,
                    human_bytes(report.complete_bytes_reused)
                );
                println!(
                    "local chunks reused: {} ({})",
                    report.local_chunks_reused,
                    human_bytes(report.local_chunk_bytes_reused)
                );
                println!(
                    "downloaded chunks: {} ({})",
                    report.chunks_downloaded,
                    human_bytes(report.compressed_bytes_downloaded)
                );
                println!("trusted key: {}", report.trusted_key_fingerprint);
            }
        }
    }
    Ok(0)
}

fn toolchain_index_command(command: ToolchainIndexCommand) -> Result<i32> {
    match command {
        ToolchainIndexCommand::Build {
            root,
            signing_key,
            public_key,
            json,
        } => {
            let report = toolchain_index::build(&root, &signing_key, &public_key)?;
            if json {
                json_output::print(schema::TOOLCHAIN_INDEX_BUILD, &report)?;
            } else {
                println!("index: {}", report.index);
                println!("signature: {}", report.signature);
                println!("entries: {}", report.entries);
                println!("toolchains: {}", report.toolchains);
                println!("platforms: {}", report.platforms);
                println!("signing key: {}", report.signing_key_fingerprint);
                println!("SHA-256: {}", report.index_sha256);
            }
        }
        ToolchainIndexCommand::List {
            remote,
            public_key,
            platform,
            allow_insecure_http,
            json,
        } => {
            let mut index = toolchain_index::load(&remote, &public_key, allow_insecure_http)?;
            if let Some(platform) = platform {
                index.entries.retain(|entry| entry.platform == platform);
            }
            if json {
                json_output::print(schema::TOOLCHAIN_INDEX_LIST, &index)?;
            } else if index.entries.is_empty() {
                println!("No matching toolchains");
            } else {
                for entry in index.entries {
                    println!(
                        "{}\t{}\t{}\t{} chunks",
                        entry.toolchain,
                        entry.platform,
                        human_bytes(entry.logical_bytes),
                        entry.unique_chunks
                    );
                }
            }
        }
        ToolchainIndexCommand::Verify {
            remote,
            public_key,
            allow_insecure_http,
            json,
        } => {
            let index = toolchain_index::load(&remote, &public_key, allow_insecure_http)?;
            let toolchains = index
                .entries
                .iter()
                .map(|entry| entry.toolchain.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            let platforms = index
                .entries
                .iter()
                .map(|entry| entry.platform.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            if json {
                let payload = serde_json::json!({
                    "schema": "lev.toolchain-index-verify/v1",
                    "entries": index.entries.len(),
                    "toolchains": toolchains,
                    "platforms": platforms,
                    "signing_key_fingerprint": index.signing_key_fingerprint,
                });
                json_output::print(schema::TOOLCHAIN_INDEX_VERIFY, &payload)?;
            } else {
                println!("verified entries: {}", index.entries.len());
                println!("toolchains: {toolchains}");
                println!("platforms: {platforms}");
                println!("signing key: {}", index.signing_key_fingerprint);
            }
        }
    }
    Ok(0)
}

/// Options shared by the toolchain install backends.
struct ToolchainInstallOptions {
    backend: ToolchainBackend,
    allow_unverified: bool,
    remote: Option<String>,
    public_key: Option<PathBuf>,
    allow_insecure_http: bool,
    pin: bool,
}

impl ToolchainInstallOptions {
    /// Reject trust options that the selected backend cannot honor.
    fn validate(&self) -> Result<()> {
        if matches!(
            self.backend,
            ToolchainBackend::Elan | ToolchainBackend::Chunks
        ) && self.allow_unverified
        {
            bail!("--allow-unverified applies only to direct archive installation");
        }
        if self.backend == ToolchainBackend::Elan
            && (self.remote.is_some() || self.public_key.is_some() || self.allow_insecure_http)
        {
            bail!("remote chunk options do not apply to the elan backend");
        }
        if self.backend == ToolchainBackend::Direct
            && (self.remote.is_some() || self.public_key.is_some() || self.allow_insecure_http)
        {
            bail!("remote chunk options do not apply to the direct backend");
        }
        Ok(())
    }
}

fn install_toolchain(
    context: &AppContext,
    toolchain: &str,
    options: ToolchainInstallOptions,
) -> Result<()> {
    options.validate()?;
    let ToolchainInstallOptions {
        backend,
        allow_unverified,
        remote,
        public_key,
        allow_insecure_http,
        pin,
    } = options;
    if pin {
        context.project()?;
    }

    let installed = match backend {
        ToolchainBackend::Elan => installed_elan_toolchains(context)?,
        ToolchainBackend::Auto => installed_elan_toolchains(context).unwrap_or_default(),
        ToolchainBackend::Direct | ToolchainBackend::Chunks => Vec::new(),
    };
    if matches!(backend, ToolchainBackend::Auto | ToolchainBackend::Elan)
        && installed.iter().any(|installed| installed == toolchain)
    {
        if pin {
            pin_project_toolchain(context, toolchain)?;
        }
        context.info(format!("toolchain already installed: {toolchain}"));
        return Ok(());
    }

    if backend != ToolchainBackend::Elan && backend != ToolchainBackend::Chunks {
        let stored = if backend == ToolchainBackend::Direct {
            context.store.find_direct(toolchain, allow_unverified)?
        } else {
            context.store.find(toolchain)?
        };
        if let Some(stored) = stored {
            register_with_elan_if_available(context, &stored.alias, &stored.view)?;
            if pin {
                pin_project_toolchain(context, &stored.source_toolchain)?;
            }
            context.info(format!(
                "toolchain already installed: {}",
                stored.source_toolchain
            ));
            return Ok(());
        }
    }

    if backend != ToolchainBackend::Elan && backend != ToolchainBackend::Direct {
        let remote = remote.or_else(|| std::env::var("LEV_TOOLCHAIN_REMOTE").ok());
        let public_key =
            public_key.or_else(|| std::env::var_os("LEV_TOOLCHAIN_PUBLIC_KEY").map(PathBuf::from));
        match (remote, public_key) {
            (Some(remote), Some(public_key)) => {
                let platform = crate::core::platform::host_id();
                let chunked = (|| {
                    let indexed_manifest_sha256 = toolchain_index::require_if_indexed(
                        &remote,
                        &public_key,
                        allow_insecure_http,
                        toolchain,
                        &platform,
                    )?;
                    toolchain_chunks::install(
                        &context.store,
                        toolchain,
                        &platform,
                        &remote,
                        &public_key,
                        indexed_manifest_sha256.as_deref(),
                        allow_insecure_http,
                    )
                })();
                match chunked {
                    Ok(report) => {
                        register_with_elan_if_available(
                            context,
                            &report.imported.alias,
                            &report.imported.view,
                        )?;
                        if pin {
                            pin_project_toolchain(context, toolchain)?;
                        }
                        context.info(format!(
                            "installed {} from signed chunks: {} complete files and {} chunks reused, {} chunks downloaded ({})",
                            report.imported.alias,
                            report.complete_files_reused,
                            report.local_chunks_reused,
                            report.chunks_downloaded,
                            human_bytes(report.compressed_bytes_downloaded)
                        ));
                        return Ok(());
                    }
                    Err(error) if backend == ToolchainBackend::Auto => {
                        context.detail(format!(
                            "signed chunk installation unavailable ({error:#}); falling back to official release metadata"
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
            (None, None) if backend == ToolchainBackend::Auto => {}
            (None, _) if backend == ToolchainBackend::Chunks => {
                bail!("the chunks backend requires --remote or LEV_TOOLCHAIN_REMOTE")
            }
            (_, None) if backend == ToolchainBackend::Chunks => {
                bail!("the chunks backend requires --public-key or LEV_TOOLCHAIN_PUBLIC_KEY")
            }
            (Some(_), None) | (None, Some(_)) => {
                bail!("signed chunk installation requires both a remote and an explicit public key")
            }
            _ => unreachable!("all chunk configuration combinations are handled"),
        }
    }

    if backend != ToolchainBackend::Elan {
        let resolved = match toolchain_download::resolve(toolchain) {
            Ok(asset) => asset,
            Err(error) if backend == ToolchainBackend::Auto => {
                context.detail(format!(
                    "direct release metadata unavailable ({error:#}); falling back to elan"
                ));
                None
            }
            Err(error) => return Err(error),
        };
        match resolved {
            Some(asset)
                if backend == ToolchainBackend::Direct
                    || asset.has_checksum()
                    || allow_unverified =>
            {
                context.info(format!(
                    "downloading {} ({})",
                    asset.name,
                    human_bytes(asset.bytes)
                ));
                let total = asset.bytes;
                let progress_enabled = context.progress;
                let mut last_percent = u64::MAX;
                let result = toolchain_download::download(
                    &context.store,
                    toolchain,
                    &asset,
                    allow_unverified,
                    |downloaded| {
                        if !progress_enabled {
                            return;
                        }
                        let percent = downloaded
                            .saturating_mul(100)
                            .checked_div(total)
                            .unwrap_or(0)
                            .min(100);
                        if percent == last_percent {
                            return;
                        }
                        last_percent = percent;
                        eprint!(
                            "\rlev: download {:>3}%  {} / {}",
                            percent,
                            human_bytes(downloaded),
                            human_bytes(total)
                        );
                        let _ = io::stderr().flush();
                    },
                );
                if progress_enabled {
                    eprintln!();
                }
                let downloaded = result?;
                register_with_elan_if_available(
                    context,
                    &downloaded.imported.alias,
                    &downloaded.imported.view,
                )?;
                if pin {
                    pin_project_toolchain(context, toolchain)?;
                }

                context.info(format!(
                    "installed {} as {}",
                    downloaded.release, downloaded.imported.alias
                ));
                context.detail(format!(
                    "archive {}: {}, SHA-256 {}{}",
                    downloaded.archive,
                    human_bytes(downloaded.compressed_bytes),
                    downloaded.sha256,
                    if downloaded.verified {
                        " (verified)"
                    } else {
                        " (unverified)"
                    }
                ));
                context.detail(format!(
                    "toolchain storage: {} new, {} reused",
                    human_bytes(downloaded.imported.new_object_bytes),
                    human_bytes(downloaded.imported.reused_bytes)
                ));
                if !downloaded.verified {
                    context.info("warning: installed archive had no trusted release digest");
                }
                return Ok(());
            }
            Some(asset) => {
                context.detail(format!(
                    "{} has no official SHA-256 digest; falling back to elan",
                    asset.name
                ));
            }
            None if backend == ToolchainBackend::Direct => {
                bail!(
                    "{toolchain} is not an exact official Lean release supported by the direct backend"
                );
            }
            None => {}
        }
    }

    ensure_elan_toolchain(context, toolchain, true)?;
    if pin {
        pin_project_toolchain(context, toolchain)?;
    }
    context.info(format!("toolchain ready through elan: {toolchain}"));
    Ok(())
}

/// Pin `lean-toolchain` and refresh `lev.lock` as one transaction.
fn pin_project_toolchain(context: &AppContext, toolchain: &str) -> Result<()> {
    let project = context.project()?;
    let toolchain_file = project.root.join("lean-toolchain");
    let lock_file = project.lock_path();
    let mut transaction = FileTransaction::capture([&toolchain_file, &lock_file])?;
    atomic_write(&toolchain_file, format!("{toolchain}\n").as_bytes())?;
    let pinned = Project::load(project.root)?;
    if pinned.manifest_path().is_file() {
        lockfile::refresh(&pinned)?;
    }
    registry::record(&context.cache, &pinned)?;
    transaction.commit();
    Ok(())
}

/// Ensure that the project's toolchain is available.
pub(super) fn ensure_toolchain(
    context: &AppContext,
    project: &Project,
    install: bool,
) -> Result<()> {
    ensure_toolchain_name(context, &project.toolchain, install)
}

/// Ensure that a normalized toolchain is available for a child process.
pub(super) fn ensure_toolchain_name(
    context: &AppContext,
    toolchain: &str,
    install: bool,
) -> Result<()> {
    if context.store.find(toolchain)?.is_some() {
        return Ok(());
    }
    if ensure_elan_toolchain(context, toolchain, false).is_ok() {
        return Ok(());
    }
    if install {
        return install_toolchain(
            context,
            toolchain,
            ToolchainInstallOptions {
                backend: ToolchainBackend::Auto,
                allow_unverified: false,
                remote: None,
                public_key: None,
                allow_insecure_http: false,
                pin: false,
            },
        );
    }
    ensure_elan_toolchain(context, toolchain, false)
}

/// Ask elan to resolve a toolchain, installing it when requested.
///
/// Warm checks use `elan which`; the next command performs the real launch.
fn ensure_elan_toolchain(context: &AppContext, toolchain: &str, install: bool) -> Result<()> {
    if install {
        let mut command = Command::new(&context.elan);
        command
            .arg("run")
            .arg("--install")
            .arg(toolchain)
            .arg("lean")
            .arg("--version");
        checked_output(&mut command)?;
    } else {
        elan_lean_executable(context, toolchain)?;
    }
    Ok(())
}

/// Start Lean to verify runtime health for doctor and audit commands.
pub(super) fn verify_toolchain_runnable(context: &AppContext, toolchain: &str) -> Result<()> {
    let mut command = context.runtime_command(toolchain, std::ffi::OsStr::new("lean"), false)?;
    command.arg("--version");
    checked_output(&mut command)?;
    Ok(())
}

/// Resolve the Lean executable selected by elan without starting Lean itself.
fn elan_lean_executable(context: &AppContext, toolchain: &str) -> Result<PathBuf> {
    let mut command = Command::new(&context.elan);
    command
        .arg("which")
        .arg("lean")
        .env("ELAN_TOOLCHAIN", toolchain);
    Ok(PathBuf::from(output_text(checked_output(&mut command)?)))
}

/// Return a toolchain root from elan's `<root>/bin/lean` executable path.
fn toolchain_path(context: &AppContext, toolchain: &str) -> Result<PathBuf> {
    if let Some(stored) = context.store.find(toolchain)? {
        return Ok(stored.view);
    }
    elan_toolchain_path(context, toolchain)
}

fn elan_toolchain_path(context: &AppContext, toolchain: &str) -> Result<PathBuf> {
    let executable = elan_lean_executable(context, toolchain)?;
    toolchain_root_from_executable(toolchain, &executable)
}

/// Interpret elan's executable result using its documented toolchain layout.
fn toolchain_root_from_executable(toolchain: &str, executable: &Path) -> Result<PathBuf> {
    executable
        .parent()
        .and_then(Path::parent)
        .map(Path::to_owned)
        .with_context(|| {
            format!(
                "elan returned an invalid Lean executable path for {toolchain}: {}",
                executable.display()
            )
        })
}

/// Make a lev store view selectable through elan.
fn link_toolchain(context: &AppContext, alias: &str, view: &Path) -> Result<()> {
    let installed = installed_elan_toolchains(context)?;
    if installed.iter().any(|toolchain| toolchain == alias) {
        let existing = elan_toolchain_path(context, alias)?;
        let existing = fs::canonicalize(&existing).unwrap_or(existing);
        let view = fs::canonicalize(view).unwrap_or_else(|_| view.to_owned());
        if existing == view {
            return Ok(());
        }
        let mut unlink = Command::new(&context.elan);
        unlink.arg("toolchain").arg("uninstall").arg(alias);
        checked_status(&mut unlink)?;
    }

    let mut link = Command::new(&context.elan);
    link.arg("toolchain").arg("link").arg(alias).arg(view);
    checked_status(&mut link)
}

/// Register an elan alias when elan is available.
fn register_with_elan_if_available(context: &AppContext, alias: &str, view: &Path) -> Result<bool> {
    if !elan_available(context) {
        context.detail("elan is not installed; using the lev toolchain store directly");
        return Ok(false);
    }
    link_toolchain(context, alias, view)?;
    Ok(true)
}

fn elan_available(context: &AppContext) -> bool {
    let mut command = Command::new(&context.elan);
    command.arg("--version");
    checked_output(&mut command).is_ok()
}

/// Parse elan's human-readable list into exact toolchain identifiers.
///
/// Elan appends annotations such as `(default)` to some rows. Stripping only
/// the annotation delimiter preserves channel names while making exact
/// membership checks independent of the selected default.
fn installed_elan_toolchains(context: &AppContext) -> Result<Vec<String>> {
    let mut command = Command::new(&context.elan);
    command.arg("toolchain").arg("list");
    let output = output_text(checked_output(&mut command)?);
    Ok(parse_installed_toolchains(&output))
}

/// Present the union of store-native and elan-only installations.
fn list_toolchains(context: &AppContext, sizes: bool) -> Result<i32> {
    let stored = context.store.installed()?;
    let stored_names = stored
        .iter()
        .map(|toolchain| toolchain.source_toolchain.clone())
        .collect::<BTreeSet<_>>();
    for toolchain in &stored {
        if sizes {
            println!(
                "{}\t{}\tlev-store\t{}",
                human_bytes(path_bytes(&toolchain.view)?),
                toolchain.source_toolchain,
                toolchain.view.display()
            );
        } else {
            println!("{} (lev store)", toolchain.source_toolchain);
        }
    }
    if let Ok(elan) = installed_elan_toolchains(context) {
        for toolchain in elan {
            if stored_names.contains(&toolchain) || toolchain.starts_with("lev-") {
                continue;
            }
            if sizes {
                let path = elan_toolchain_path(context, &toolchain)?;
                println!(
                    "{}\t{}\telan\t{}",
                    human_bytes(path_bytes(&path)?),
                    toolchain,
                    path.display()
                );
            } else {
                println!("{toolchain} (elan)");
            }
        }
    }
    Ok(0)
}

/// Normalize elan list rows while preserving exact toolchain identifiers.
fn parse_installed_toolchains(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.split_once(" (")
                .map_or(line, |(toolchain, _)| toolchain)
                .trim()
                .to_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(backend: ToolchainBackend) -> ToolchainInstallOptions {
        ToolchainInstallOptions {
            backend,
            allow_unverified: false,
            remote: None,
            public_key: None,
            allow_insecure_http: false,
            pin: false,
        }
    }

    #[test]
    fn installation_options_do_not_silently_ignore_trust_flags() {
        let mut elan = options(ToolchainBackend::Elan);
        elan.allow_unverified = true;
        assert!(
            elan.validate()
                .unwrap_err()
                .to_string()
                .contains("only to direct archive installation")
        );

        let mut chunks = options(ToolchainBackend::Chunks);
        chunks.allow_unverified = true;
        assert!(
            chunks
                .validate()
                .unwrap_err()
                .to_string()
                .contains("only to direct archive installation")
        );

        let mut elan_remote = options(ToolchainBackend::Elan);
        elan_remote.remote = Some("https://cache.example".to_owned());
        assert!(
            elan_remote
                .validate()
                .unwrap_err()
                .to_string()
                .contains("do not apply to the elan backend")
        );

        let mut direct = options(ToolchainBackend::Direct);
        direct.allow_unverified = true;
        assert!(direct.validate().is_ok());

        direct.remote = Some("https://cache.example".to_owned());
        assert!(
            direct
                .validate()
                .unwrap_err()
                .to_string()
                .contains("do not apply to the direct backend")
        );
    }

    #[test]
    fn elan_list_parser_removes_annotations_and_blank_rows() {
        assert_eq!(
            parse_installed_toolchains(
                "\nleanprover/lean4:v4.fixture-d (default)\nnightly\ncustom (override)\n\n"
            ),
            ["leanprover/lean4:v4.fixture-d", "nightly", "custom"]
        );
    }

    #[test]
    fn executable_path_must_have_a_bin_directory_and_root() {
        assert_eq!(
            toolchain_root_from_executable(
                "leanprover/lean4:v4.fixture-d",
                Path::new("/opt/lean/bin/lean"),
            )
            .unwrap(),
            PathBuf::from("/opt/lean")
        );
        assert!(
            toolchain_root_from_executable("broken", Path::new("lean"))
                .unwrap_err()
                .to_string()
                .contains("invalid Lean executable path")
        );
    }
}
