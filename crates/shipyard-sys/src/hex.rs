//! Bytes as lowercase hexadecimal (step 12.5b1, F145: written six times).

/// `bytes` as lowercase hex, two digits a byte.
#[must_use]
pub fn lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

#[cfg(test)]
mod tests {
    /// Every byte as `{:02x}` writes it; none for none.
    #[test]
    fn every_byte_is_two_lowercase_digits() {
        let all: Vec<u8> = (0..=255).collect();
        let expected = all.iter().fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        });
        assert_eq!(super::lower(&all), expected);
        assert_eq!(super::lower(&[]), "");
    }
}
