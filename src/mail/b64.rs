//! URL-safe base64 as Gmail uses it for `body.data`, attachments and `raw`.
//!
//! Gmail is inconsistent about padding, so decoding accepts both padded and unpadded input.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_PAD_INDIFFERENT;

pub use base64::DecodeError;

/// Decodes URL-safe base64, padded or not. Surrounding whitespace is ignored.
pub fn decode(data: &str) -> Result<Vec<u8>, DecodeError> {
    URL_SAFE_PAD_INDIFFERENT.decode(data.trim())
}

/// Encodes bytes as padded URL-safe base64.
#[allow(dead_code)] // consumed in M11
pub fn encode(bytes: &[u8]) -> String {
    URL_SAFE_PAD_INDIFFERENT.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_and_unpadded_input_decode() {
        assert_eq!(decode("aGk=").unwrap(), b"hi");
        assert_eq!(decode("aGk").unwrap(), b"hi");
        assert_eq!(decode("aGVsbG8gd29ybGQ").unwrap(), b"hello world");
    }

    #[test]
    fn url_safe_alphabet_is_used() {
        // 0xfb 0xff encodes to "-_8" in the URL-safe alphabet ("+/8" in standard).
        assert_eq!(decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(decode("-_8=").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(encode(&[0xfb, 0xff]), "-_8=");
    }

    #[test]
    fn invalid_input_is_an_error() {
        assert!(decode("a$b!").is_err());
        assert!(decode("+/8=").is_err());
        assert!(decode("a").is_err());
    }

    #[test]
    fn round_trip() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }
}
