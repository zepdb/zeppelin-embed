//! Helpers shared by byte-exact golden-fixture tests.

/// Decodes ASCII hexadecimal while rejecting malformed fixture text.
pub fn decode_hex(text: &str) -> Result<Vec<u8>, HexError> {
    let compact = text.bytes().filter(|byte| !byte.is_ascii_whitespace());
    let mut high = None;
    let mut bytes = Vec::new();
    for byte in compact {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(HexError::InvalidDigit(byte)),
        };
        if let Some(first) = high.take() {
            bytes.push((first << 4) | nibble);
        } else {
            high = Some(nibble);
        }
    }
    if high.is_some() {
        return Err(HexError::OddLength);
    }
    Ok(bytes)
}

/// Malformed textual golden fixture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HexError {
    /// The fixture contained an odd number of hexadecimal nibbles.
    OddLength,
    /// The fixture contained a non-hexadecimal byte.
    InvalidDigit(u8),
}

impl std::fmt::Display for HexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OddLength => formatter.write_str("golden hex has an odd number of nibbles"),
            Self::InvalidDigit(byte) => {
                write!(formatter, "golden hex contains invalid byte {byte}")
            }
        }
    }
}

impl std::error::Error for HexError {}
