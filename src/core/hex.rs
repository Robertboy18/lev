//! Lowercase hexadecimal encoding and checks.

/// Encode bytes as lowercase hexadecimal text.
pub(crate) fn lowercase(value: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Whether `value` is lowercase hexadecimal text of exactly `length` bytes.
pub(crate) fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{is_lower_hex, lowercase};

    #[test]
    fn encodes_and_recognizes_lowercase_hex() {
        assert_eq!(lowercase(&[]), "");
        assert_eq!(lowercase(&[0x00, 0x09, 0xaf, 0xff]), "0009afff");
        assert!(is_lower_hex("0123456789abcdef", 16));
        assert!(!is_lower_hex("0123456789abcdeF", 16));
        assert!(!is_lower_hex("0123456789abcde", 16));
        assert!(!is_lower_hex("0123456789abcdeg", 16));
    }
}
