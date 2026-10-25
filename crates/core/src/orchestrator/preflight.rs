//! The engine preflight: reasoning at the configured level, a working
//! prefix cache, a shell command run through the harness's own tool, and
//! (for vision models) a correctly read image.

use std::path::Path;

use crate::ledger::Preflight;
use crate::metrics::engine_summary;
use crate::session::StopReason;

use super::{RunError, Runner};

/// The file the tool probe reads, and the word in it. The word appears
/// nowhere in the prompt, so an answer with it means the tool call worked.
pub const TOOL_PROBE_FILE: &str = "probe.txt";
const TOOL_PROBE_WORD: &str = "marmalade-4817";

impl Runner {
    pub(crate) async fn preflight(&mut self) -> Result<Preflight, RunError> {
        let filler = "The orchestrator measures whether the engine reuses a cached prompt prefix. "
            .repeat(40);
        let vision = self.ledger.config.supports_images();
        let dir = tempfile::Builder::new()
            .prefix("tenaz-preflight-")
            .tempdir()?;
        std::fs::write(
            dir.path().join(TOOL_PROBE_FILE),
            format!("{TOOL_PROBE_WORD}\n"),
        )?;
        if vision {
            write_test_image(&dir.path().join("probe.png"))?;
        }
        let cwd = dir.path().to_path_buf();
        // Absolute paths: a harness may run its tools in another folder than
        // the session's (fx uses the folder it was started in).
        let probe = cwd.join(TOOL_PROBE_FILE);
        let image = cwd.join("probe.png");
        let mut prompts = vec![
            format!("{filler}\nThink briefly, then answer with only the number: what is 17 * 23?"),
            "Now answer with only the number: what is 19 * 21?".to_owned(),
            format!(
                "Use your shell tool to run `cat {}`, then answer with only the word it printed.",
                probe.display()
            ),
        ];
        if vision {
            prompts.push(format!("Use the read tool to open {} and look at it. Answer with two words only: the colour of the square, then the colour of the background.", image.display()));
        }
        let (m, requests) = self
            .run_harness("preflight", &prompts, cwd, None, false, None)
            .await?;
        // The first message after a message that ended with `stop` is the second prompt's answer.
        let second = m
            .assistant
            .iter()
            .enumerate()
            .find(|(i, _)| *i > 0 && m.assistant[i - 1].stop_reason == StopReason::Stop)
            .map(|(_, a)| a)
            .or_else(|| m.assistant.last());
        let last_text = m.texts.last().cloned().unwrap_or_default();
        let vision_ok = vision.then(|| {
            let lower = last_text.to_lowercase();
            matches!((lower.find("red"), lower.find("blue")), (Some(r), Some(b)) if r < b)
        });
        let reasoning = m.reasoning_chars > 0 || m.usage.reasoning > 0;
        let cache = second.is_some_and(|a| a.usage.cache_read > 0);
        let tools = m.texts.iter().any(|t| t.contains(TOOL_PROBE_WORD));
        Ok(Preflight {
            at: self.now(),
            ok: m.settled && reasoning && cache && tools && vision_ok != Some(false),
            settled: m.settled,
            reasoning,
            cache,
            tools: Some(tools),
            vision: vision_ok,
            vision_answer: vision.then(|| last_text.trim().chars().take(80).collect()),
            reasoning_chars: m.reasoning_chars,
            second_turn_cache_read: second.map_or(0, |a| a.usage.cache_read),
            duration_ms: m.duration_ms,
            engine_metrics: requests.iter().any(|r| r.engine_matched),
            engine: engine_summary(&requests),
            accepted_weak: false,
        })
    }
}

/// What failed in `preflight`, in words, for the error that stops the run.
pub(crate) fn failures(harness: &str, preflight: Option<&Preflight>) -> String {
    let Some(p) = preflight else {
        return "no result".to_owned();
    };
    let mut failed = Vec::new();
    if !p.settled {
        failed.push("the session did not finish".to_owned());
    }
    if !p.reasoning {
        failed.push("no reasoning at the configured level".to_owned());
    }
    if !p.cache {
        failed.push("the engine did not reuse the cached prompt prefix".to_owned());
    }
    if p.tools == Some(false) {
        failed.push(format!(
            "the model could not run `cat {TOOL_PROBE_FILE}` with {harness}'s shell tool, so it \
             cannot work in the project; try another model, or the other harness"
        ));
    }
    if p.vision == Some(false) {
        failed.push(format!(
            "the model did not read the test image correctly (it answered {:?})",
            p.vision_answer.as_deref().unwrap_or_default()
        ));
    }
    if failed.is_empty() {
        "no check failed".to_owned()
    } else {
        failed.join("; ")
    }
}

/// A 64x64 PNG: a red square on a blue background, written without a
/// compression library (stored deflate blocks).
pub fn write_test_image(path: &Path) -> std::io::Result<()> {
    let size = 64u32;
    let mut raw = Vec::with_capacity((size * (size * 3 + 1)) as usize);
    for y in 0..size {
        raw.push(0u8);
        for x in 0..size {
            let inside = (16..48).contains(&x) && (16..48).contains(&y);
            raw.extend_from_slice(if inside {
                &[220, 30, 30]
            } else {
                &[30, 60, 220]
            });
        }
    }
    let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut header = Vec::new();
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, *b"IHDR", &header);
    chunk(&mut png, *b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, *b"IEND", &[]);
    std::fs::write(path, png)
}

fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in bytes {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    c ^ 0xffff_ffff
}

/// A zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = data.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        let last = u8::from(i + 1 == blocks.len());
        out.push(last);
        let len = u16::try_from(block.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_tool_probe_is_named_with_the_harness() {
        let p = Preflight {
            at: String::new(),
            ok: false,
            settled: true,
            reasoning: true,
            cache: true,
            tools: Some(false),
            vision: None,
            vision_answer: None,
            reasoning_chars: 10,
            second_turn_cache_read: 100,
            duration_ms: 0,
            engine_metrics: false,
            engine: None,
            accepted_weak: false,
        };
        let text = failures("fx", Some(&p));
        assert!(text.contains("with fx's shell tool"), "{text}");
        assert!(!text.contains("cache"), "{text}");
        let old = Preflight { tools: None, ..p };
        assert_eq!(failures("fx", Some(&old)), "no check failed");
    }

    #[test]
    fn test_image_is_a_well_formed_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.png");
        write_test_image(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        assert_eq!(&bytes[12..16], b"IHDR");
        assert_eq!(&bytes[bytes.len() - 8..bytes.len() - 4], b"IEND");
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let z = zlib_stored(b"abc");
        assert_eq!(
            z,
            vec![
                0x78, 0x01, 1, 3, 0, 0xfc, 0xff, b'a', b'b', b'c', 0x02, 0x4d, 0x01, 0x27
            ]
        );
    }
}
