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

/// The last `bytes` of `text` at most, from a character boundary.
pub fn tail(text: &str, bytes: usize) -> String {
    if text.len() <= bytes {
        return text.to_owned();
    }
    let start = text.ceil_char_boundary(text.len() - bytes);
    text[start..].to_owned()
}

/// `program args...` started through `wrapper` (a program and its leading
/// arguments, such as the sandbox launcher); unchanged when `wrapper` is empty.
pub fn wrap_command(
    wrapper: &[String],
    program: &std::path::Path,
    args: Vec<String>,
) -> (std::path::PathBuf, Vec<String>) {
    match wrapper.split_first() {
        None => (program.to_path_buf(), args),
        Some((launcher, leading)) => {
            let mut wrapped = leading.to_vec();
            wrapped.push(program.display().to_string());
            wrapped.extend(args);
            (std::path::PathBuf::from(launcher), wrapped)
        }
    }
}

/// The rough token count of a text: four characters per token.
pub fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Bytes per token of code and diffs: about 3 in the Digger 3D run, where
/// prose was close to 4 (ADR 0012).
pub const CODE_BYTES_PER_TOKEN: usize = 3;

/// The token count of a text taken as code: [`CODE_BYTES_PER_TOKEN`].
/// Prose is overcounted by about a quarter, which errs on the side of
/// fitting.
pub fn approx_code_tokens(text: &str) -> usize {
    text.len().div_ceil(CODE_BYTES_PER_TOKEN)
}

/// `text` without terminal escape sequences: colours and other CSI
/// sequences (`ESC [ … final`), OSC sequences (`ESC ] … BEL` or `ESC \\`)
/// and two-character escapes.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                let _ = chars.find(|c| ('\u{40}'..='\u{7e}').contains(c));
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_leaves_the_text() {
        assert_eq!(
            strip_ansi("\u{1b}[2m  \u{1b}[32m➜\u{1b}[39m  \u{1b}[1mLocal\u{1b}[22m: http://x"),
            "  ➜  Local: http://x"
        );
        assert_eq!(
            strip_ansi("\u{1b}]8;;http://a\u{7}link\u{1b}]8;;\u{1b}\\ done\u{1b}7"),
            "link done"
        );
        assert_eq!(strip_ansi("plain ✓"), "plain ✓");
    }

    /// A progress line redrawn with `\r` has no newline to cut at, and a
    /// cut may fall inside a character.
    #[test]
    fn tail_keeps_the_end_on_a_character_boundary() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("frame 1\rframe 2\rframe 3", 7), "frame 3");
        assert_eq!(tail("añb", 2), "b", "ñ is two bytes, and the cut is in it");
    }

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
        assert_eq!(approx_code_tokens(""), 0);
        assert_eq!(approx_code_tokens("abcd"), 2);
    }
}
