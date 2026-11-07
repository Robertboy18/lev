//! Versioned envelopes for machine-readable command output.
//!
//! Every `--json` command prints `{schema, data}` through this module.

use std::io::{self, Write};

use anyhow::{Context, Result};
use serde::Serialize;

/// Schema identifiers, one per JSON-producing command.
pub(crate) mod schema {
    pub const INSPECT_BUILD: &str = "lev.cli.inspect.build/v2";
    pub const CHECK: &str = "lev.cli.check/v2";
    pub const DOCTOR: &str = "lev.cli.doctor/v2";
    pub const CACHE_STATUS: &str = "lev.cli.cache.status/v2";
    pub const CACHE_GC: &str = "lev.cli.cache.gc/v2";

    /// Every identifier, checked against the published envelope schema.
    #[cfg(test)]
    pub const ALL: &[&str] = &[INSPECT_BUILD, CHECK, DOCTOR, CACHE_STATUS, CACHE_GC];
}

#[derive(Debug, Serialize)]
struct Envelope<'a, T: ?Sized> {
    schema: &'static str,
    data: &'a T,
}

/// Serialize the whole envelope before writing so stdout never gets partial JSON.
pub(crate) fn print<T>(schema: &'static str, data: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let mut rendered = serde_json::to_vec_pretty(&Envelope { schema, data })
        .context("failed to serialize JSON output")?;
    rendered.push(b'\n');
    io::stdout()
        .lock()
        .write_all(&rendered)
        .context("failed to write JSON output")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::schema;

    #[test]
    fn schema_identifiers_are_unique_and_versioned() {
        let unique = schema::ALL.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), schema::ALL.len());
        for identifier in schema::ALL {
            assert!(identifier.starts_with("lev.cli."), "{identifier}");
            assert!(identifier.ends_with("/v2"), "{identifier}");
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
