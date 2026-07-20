//! Lean toolchain selector normalization.
//!
//! Shorthand expands syntactically; complete elan identifiers pass through.

pub(crate) mod chunks;
pub(crate) mod download;
pub(crate) mod index;
pub(crate) mod store;

use anyhow::{Result, bail};

pub fn normalize(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("Lean toolchain cannot be empty");
    }
    if value.chars().any(char::is_whitespace) {
        bail!("Lean toolchain cannot contain whitespace: {value:?}");
    }

    if value.contains('/') || value.contains(':') {
        return Ok(value.to_owned());
    }

    if value == "stable" || value == "beta" {
        return Ok(format!("leanprover/lean4:{value}"));
    }

    if value == "nightly" || value.starts_with("nightly-") {
        return Ok(format!("leanprover/lean4-nightly:{value}"));
    }

    let version = value.strip_prefix('v').unwrap_or(value);
    if version
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        return Ok(format!("leanprover/lean4:v{version}"));
    }

    Ok(value.to_owned())
}

/// Return the channel or release portion of a canonical toolchain identifier.
pub(crate) fn short_name(toolchain: &str) -> &str {
    toolchain
        .rsplit_once(':')
        .map_or(toolchain, |(_, channel)| channel)
}

#[cfg(test)]
mod tests {
    use super::{normalize, short_name};

    #[test]
    fn normalizes_common_version_forms() {
        assert_eq!(
            normalize("4.fixture-b").unwrap(),
            "leanprover/lean4:v4.fixture-b"
        );
        assert_eq!(
            normalize("v4.fixture-b").unwrap(),
            "leanprover/lean4:v4.fixture-b"
        );
        assert_eq!(
            normalize("nightly").unwrap(),
            "leanprover/lean4-nightly:nightly"
        );
        assert_eq!(
            normalize("nightly-fixture").unwrap(),
            "leanprover/lean4-nightly:nightly-fixture"
        );
        assert_eq!(
            normalize("leanprover/lean4:v4.fixture-b").unwrap(),
            "leanprover/lean4:v4.fixture-b"
        );
        assert_eq!(
            normalize("99.123.456").unwrap(),
            "leanprover/lean4:v99.123.456"
        );
        assert_eq!(normalize("7-rc").unwrap(), "leanprover/lean4:v7-rc");
        assert_eq!(
            normalize("vendor/lean:channel").unwrap(),
            "vendor/lean:channel"
        );
        assert_eq!(short_name("vendor/lean:channel"), "channel");
        assert_eq!(short_name("local-alias"), "local-alias");
    }

    #[test]
    fn rejects_invalid_names() {
        assert!(normalize(" ").is_err());
        assert!(normalize("v4.fixture-b bad").is_err());
    }
}
