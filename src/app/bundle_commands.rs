//! User-facing deterministic project bundle creation.

use anyhow::{Context, Result};

use crate::cli::BundleArgs;
use crate::project;
use crate::project::bundle as project_bundle;

use super::AppContext;

pub(super) fn bundle(context: &AppContext, args: BundleArgs) -> Result<i32> {
    let project = context.project()?;
    let destination = match args.output {
        Some(path) => project::absolute(&path)?,
        None => {
            let name = project
                .root
                .file_name()
                .and_then(|name| name.to_str())
                .context("project root has no portable directory name")?;
            std::env::current_dir()
                .context("failed to determine current directory")?
                .join(format!("{name}.lev.tar.zst"))
        }
    };
    let stats = project_bundle::create(&project, &destination, args.force)?;
    context.info(format!(
        "bundled {} files ({} source bytes) into {}",
        stats.files,
        stats.source_bytes,
        destination.display()
    ));
    Ok(0)
}
