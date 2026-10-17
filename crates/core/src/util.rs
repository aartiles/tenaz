//! Small helpers shared by the core modules.

use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of `bytes`.
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

/// Cuts `text` to at most `limit` bytes (on a character boundary) and says how
/// much was removed, so a model sees that the text is incomplete.
pub fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let cut = text.floor_char_boundary(limit);
    format!("{}\n[... clipped {} bytes]", &text[..cut], text.len() - cut)
}

/// The rough token count of a text: four characters per token.
pub fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_keeps_short_text_and_marks_long_text() {
        assert_eq!(clip("abc", 10), "abc");
        assert_eq!(clip("abcdefghij", 4), "abcd\n[... clipped 6 bytes]");
    }

    #[test]
    fn clip_never_splits_a_character() {
        let clipped = clip("ééé", 3);
        assert!(clipped.starts_with("é\n"));
    }

    #[test]
    fn sha256_is_hex_of_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn approx_tokens_rounds_up() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcde"), 2);
    }
}
