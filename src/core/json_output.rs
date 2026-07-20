//! Stable envelopes for machine-readable command output.
//!
//! Every `--json` command emits `{schema, data}` through this module.

use std::io::{self, Write};

use anyhow::{Context, Result};
use serde::Serialize;

/// Schema identifiers for every JSON-producing command.
pub(crate) mod schema {
    pub const OUTDATED: &str = "lev.cli.outdated/v1";
    pub const AUDIT: &str = "lev.cli.audit/v1";
    pub const DEPS: &str = "lev.cli.deps/v1";
    pub const TREE: &str = "lev.cli.tree/v1";
    pub const WHY: &str = "lev.cli.why/v1";
    pub const WORKSPACE_LIST: &str = "lev.cli.workspace.list/v1";
    pub const CACHE_ARTIFACTS_STATUS: &str = "lev.cli.cache.artifacts.status/v1";
    pub const CACHE_ARTIFACTS_VERIFY: &str = "lev.cli.cache.artifacts.verify/v1";
    pub const CACHE_ARTIFACTS_GC: &str = "lev.cli.cache.artifacts.gc/v1";
    pub const CACHE_REMOTE_PUSH: &str = "lev.cli.cache.remote.push/v1";
    pub const CACHE_REMOTE_PULL: &str = "lev.cli.cache.remote.pull/v1";
    pub const TOOLCHAIN_CHUNKS_PUBLISH: &str = "lev.cli.toolchain.chunks.publish/v1";
    pub const TOOLCHAIN_CHUNKS_INSTALL: &str = "lev.cli.toolchain.chunks.install/v1";
    pub const TOOLCHAIN_INDEX_BUILD: &str = "lev.cli.toolchain.index.build/v1";
    pub const TOOLCHAIN_INDEX_LIST: &str = "lev.cli.toolchain.index.list/v1";
    pub const TOOLCHAIN_INDEX_VERIFY: &str = "lev.cli.toolchain.index.verify/v1";
    pub const TOOLCHAIN_GC: &str = "lev.cli.toolchain.gc/v1";
    pub const TOOL_LIST: &str = "lev.cli.tool.list/v1";
    pub const TOOL_GC: &str = "lev.cli.tool.gc/v1";

    /// Complete schema inventory used to enforce identifier invariants.
    #[cfg(test)]
    pub const ALL: &[&str] = &[
        OUTDATED,
        AUDIT,
        DEPS,
        TREE,
        WHY,
        WORKSPACE_LIST,
        CACHE_ARTIFACTS_STATUS,
        CACHE_ARTIFACTS_VERIFY,
        CACHE_ARTIFACTS_GC,
        CACHE_REMOTE_PUSH,
        CACHE_REMOTE_PULL,
        TOOLCHAIN_CHUNKS_PUBLISH,
        TOOLCHAIN_CHUNKS_INSTALL,
        TOOLCHAIN_INDEX_BUILD,
        TOOLCHAIN_INDEX_LIST,
        TOOLCHAIN_INDEX_VERIFY,
        TOOLCHAIN_GC,
        TOOL_LIST,
        TOOL_GC,
    ];
}

/// Versioned outer shape shared by all `--json` command reports.
#[derive(Debug, Serialize)]
struct Envelope<'a, T: ?Sized> {
    schema: &'static str,
    data: &'a T,
}

/// Serialize before writing so stdout never receives partial JSON.
pub(crate) fn print<T>(schema: &'static str, data: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let rendered = render(schema, data)?;
    io::stdout()
        .lock()
        .write_all(&rendered)
        .context("failed to write JSON output")
}

fn render<T>(schema: &'static str, data: &T) -> Result<Vec<u8>>
where
    T: Serialize + ?Sized,
{
    let envelope = Envelope { schema, data };
    let mut rendered =
        serde_json::to_vec_pretty(&envelope).context("failed to serialize JSON output")?;
    rendered.push(b'\n');
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::{render, schema};

    #[test]
    fn envelope_keeps_payload_under_data_and_ends_with_one_newline() {
        let rendered = render(schema::DEPS, &json!({"packages": [1, 2]})).unwrap();
        assert_eq!(rendered.last(), Some(&b'\n'));
        assert_ne!(rendered.get(rendered.len().saturating_sub(2)), Some(&b'\n'));

        let value: serde_json::Value = serde_json::from_slice(&rendered).unwrap();
        assert_eq!(value["schema"], schema::DEPS);
        assert_eq!(value["data"]["packages"], json!([1, 2]));
        assert!(value.get("packages").is_none());
    }

    #[test]
    fn command_schema_identifiers_are_unique_and_versioned() {
        let unique = schema::ALL.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), schema::ALL.len());
        for identifier in schema::ALL {
            assert!(identifier.starts_with("lev.cli."), "{identifier}");
            assert!(identifier.ends_with("/v1"), "{identifier}");
            assert!(
                identifier.bytes().all(|byte| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'/')),
                "{identifier}"
            );
        }
    }

    #[test]
    fn published_envelope_schema_lists_every_command_contract() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../../schemas/cli-envelope-v1.schema.json"))
                .unwrap();
        let published = document["properties"]["schema"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let implemented = schema::ALL.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(published, implemented);
        assert_eq!(document["additionalProperties"], false);
    }
}
