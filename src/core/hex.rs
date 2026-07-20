//! Lexical checks for lowercase hexadecimal identifiers.

/// Whether `value` is lowercase hexadecimal text of exactly `length` bytes.
pub(crate) fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `value` is a canonical SHA-256 digest.
pub(crate) fn is_sha256(value: &str) -> bool {
    is_lower_hex(value, 64)
}

/// Whether `value` is a full lowercase SHA-1 or SHA-256 Git object ID.
pub(crate) fn is_git_object_id(value: &str) -> bool {
    is_lower_hex(value, 40) || is_sha256(value)
}

#[cfg(test)]
mod tests {
    use super::{is_git_object_id, is_sha256};

    #[test]
    fn accepts_only_canonical_lowercase_digests() {
        assert!(is_git_object_id(&"a".repeat(40)));
        assert!(is_git_object_id(&"0".repeat(64)));
        assert!(is_sha256(&"f".repeat(64)));
        assert!(!is_git_object_id(&"a".repeat(39)));
        assert!(!is_git_object_id(&"g".repeat(40)));
        assert!(!is_git_object_id(&"A".repeat(40)));
        assert!(!is_git_object_id("release-tag"));
    }
}
