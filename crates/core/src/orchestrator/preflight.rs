//! The engine preflight: reasoning at the configured level, a working
//! prefix cache, a shell command run through the harness's own tool, and
//! (for vision models) a correctly read image.

use std::path::Path;

use crate::cache_report::CacheReport;
use crate::ledger::{CacheCheck, HarnessKind, Preflight};
use crate::metrics::{RequestRecord, engine_summary};
use crate::session::{AssistantTurn, SessionMetrics, StopReason};

use super::session::HarnessRun;
use super::{RunError, Runner};

/// The file the tool probe reads, and the word in it. The word appears
/// nowhere in the prompt, so an answer with it means the tool call worked.
pub const TOOL_PROBE_FILE: &str = "probe.txt";
const TOOL_PROBE_WORD: &str = "marmalade-4817";

impl Runner {
    pub(crate) async fn preflight(&mut self) -> Result<Preflight, RunError> {
        let model = self.ledger.config.run_model();
        let vision = model.supports_images;
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
        let prompts = preflight_prompts(&cwd, vision);
        let (m, requests) = self
            .run_harness(HarnessRun {
                label: "preflight",
                prompts: &prompts,
                cwd,
                model,
                watch: None,
                persist: false,
                own_checks: None,
            })
            .await?;
        let cache_reported = self
            .deps
            .cache_report
            .as_ref()
            .and_then(CacheReport::reported);
        Ok(Preflight {
            at: self.now(),
            ..read_preflight(&m, &requests, vision, cache_reported)
        })
    }
}

/// What a preflight session (`m`, its `requests`) shows, with `vision`
/// asked about and what the engine proxy saw of cached tokens; the caller
/// sets `at`.
fn read_preflight(
    m: &SessionMetrics,
    requests: &[RequestRecord],
    vision: bool,
    cache_reported: Option<bool>,
) -> Preflight {
    let second = second_answer(m);
    let last_text = m.texts.last().cloned().unwrap_or_default();
    let vision_ok = vision.then(|| red_before_blue(&last_text));
    let reasoning = m.reasoning_chars > 0 || m.usage.reasoning > 0;
    let cache = second.is_some_and(|a| a.usage.cache_read > 0);
    let tools = m.texts.iter().any(|t| t.contains(TOOL_PROBE_WORD));
    let mut p = Preflight {
        at: String::new(),
        ok: false,
        settled: m.settled,
        reasoning,
        cache,
        cache_reported,
        tools: Some(tools),
        vision: vision_ok,
        vision_answer: vision.then(|| last_text.trim().chars().take(80).collect()),
        reasoning_chars: m.reasoning_chars,
        second_turn_cache_read: second.map_or(0, |a| a.usage.cache_read),
        duration_ms: m.duration_ms,
        engine_metrics: requests.iter().any(|r| r.engine_matched),
        engine: engine_summary(requests),
        accepted_weak: false,
    };
    p.ok = p.settled && p.reasoning && p.cache_check().passes() && tools && p.vision != Some(false);
    p
}

/// The second prompt's answer: the first message after one that ended
/// with `stop`, else the last.
fn second_answer(m: &SessionMetrics) -> Option<&AssistantTurn> {
    m.assistant
        .iter()
        .enumerate()
        .find(|(i, _)| *i > 0 && m.assistant[i - 1].stop_reason == StopReason::Stop)
        .map(|(_, a)| a)
        .or_else(|| m.assistant.last())
}

/// The image answer names the square's colour (red), then the
/// background's (blue).
fn red_before_blue(answer: &str) -> bool {
    let lower = answer.to_lowercase();
    matches!((lower.find("red"), lower.find("blue")), (Some(r), Some(b)) if r < b)
}

/// The preflight's prompts, for a session in `cwd`: two questions sharing
/// a long prefix (reasoning, then the cache), the tool probe, and with
/// `vision` the image.
fn preflight_prompts(cwd: &Path, vision: bool) -> Vec<String> {
    let filler =
        "The orchestrator measures whether the engine reuses a cached prompt prefix. ".repeat(40);
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
    prompts
}

/// What the preflight could not check, in words, for the run's log.
pub(crate) fn warnings(preflight: &Preflight) -> Option<&'static str> {
    (preflight.cache_check() == CacheCheck::NotReported).then_some(
        "WARNING: the engine does not report cached prompt tokens, so the preflight cannot \
         check its prefix cache; it passes. Without a cache every turn processes the whole \
         prompt again. Check the engine's own log for prefix reuse.",
    )
}

/// What failed in `preflight`, in words, for the error that stops the run.
pub(crate) fn failures(harness: HarnessKind, preflight: Option<&Preflight>) -> String {
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
    if p.cache_check() == CacheCheck::NotReused {
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
            cache_reported: Some(true),
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
        let text = failures(HarnessKind::Fx, Some(&p));
        assert!(text.contains("with fx's shell tool"), "{text}");
        assert!(!text.contains("cache"), "{text}");
        let old = Preflight { tools: None, ..p };
        assert_eq!(failures(HarnessKind::Fx, Some(&old)), "no check failed");
    }

    fn turn(stop_reason: StopReason, cache_read: u64) -> AssistantTurn {
        AssistantTurn {
            stop_reason,
            usage: crate::session::Usage {
                cache_read,
                ..Default::default()
            },
            thinking: 0,
        }
    }

    #[test]
    fn the_second_answer_follows_the_first_that_stopped() {
        // As in a preflight: the first answer, the second, then the tool probe.
        let m = SessionMetrics {
            assistant: vec![
                turn(StopReason::Stop, 0),
                turn(StopReason::Stop, 512),
                turn(StopReason::ToolUse, 9),
                turn(StopReason::Stop, 10),
            ],
            ..Default::default()
        };
        assert_eq!(second_answer(&m).map(|a| a.usage.cache_read), Some(512));
    }

    #[test]
    fn without_a_stopped_answer_the_last_one_is_taken() {
        let m = SessionMetrics {
            assistant: vec![turn(StopReason::ToolUse, 1), turn(StopReason::ToolUse, 2)],
            ..Default::default()
        };
        assert_eq!(second_answer(&m).map(|a| a.usage.cache_read), Some(2));
    }

    #[test]
    fn the_image_answer_names_red_then_blue() {
        assert!(red_before_blue("Red, Blue"));
        assert!(!red_before_blue("blue, red"));
        assert!(!red_before_blue("red"));
    }

    /// A preflight session whose last answer to the image is `answer`;
    /// everything else passes.
    fn with_image_answer(answer: &str) -> SessionMetrics {
        SessionMetrics {
            settled: true,
            reasoning_chars: 10,
            texts: vec![TOOL_PROBE_WORD.to_owned(), answer.to_owned()],
            assistant: vec![
                turn(StopReason::Stop, 0),
                turn(StopReason::Stop, 512),
                turn(StopReason::Stop, 0),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_wrong_image_answer_fails_the_preflight() {
        let m = with_image_answer("blue, red");
        let p = read_preflight(&m, &[], true, None);
        assert_eq!(p.vision, Some(false));
        assert!(!p.ok);
    }

    #[test]
    fn without_vision_the_image_answer_is_not_checked() {
        let m = with_image_answer("blue, red");
        let p = read_preflight(&m, &[], false, None);
        assert_eq!(p.vision, None);
        assert!(p.ok);
    }

    /// A preflight whose second answer read nothing from the cache, with
    /// `cache_reported` from the engine proxy.
    fn second_answer_uncached(cache_reported: Option<bool>) -> Preflight {
        let mut m = with_image_answer("");
        // The second answer, the one after the first that stopped.
        m.assistant[1] = turn(StopReason::Stop, 0);
        read_preflight(&m, &[], false, cache_reported)
    }

    #[test]
    fn an_engine_that_reports_zero_cached_tokens_fails_the_cache_check() {
        let p = second_answer_uncached(Some(true));
        assert!(!p.ok);
        assert_eq!(p.cache_check(), CacheCheck::NotReused);
        let text = failures(HarnessKind::Pi, Some(&p));
        assert!(
            text.contains("did not reuse the cached prompt prefix"),
            "{text}"
        );
        assert_eq!(warnings(&p), None);
    }

    #[test]
    fn an_engine_that_does_not_report_cached_tokens_passes_with_a_warning() {
        let p = second_answer_uncached(Some(false));
        assert!(p.ok);
        assert!(!p.cache);
        assert_eq!(p.cache_check().to_string(), "not reported");
        assert_eq!(failures(HarnessKind::Pi, Some(&p)), "no check failed");
        assert!(warnings(&p).is_some_and(|w| w.contains("does not report cached prompt tokens")));
    }

    #[test]
    fn without_the_engine_proxy_no_cached_tokens_is_still_a_failure() {
        let p = second_answer_uncached(None);
        assert!(!p.ok);
        assert_eq!(p.cache_check().to_string(), "false");
    }

    #[test]
    fn a_preflight_saved_before_the_report_loads_without_it() {
        let mut json = serde_json::to_value(second_answer_uncached(None)).unwrap();
        assert!(json.get("cache_reported").is_none(), "{json}");
        json["cache"] = true.into();
        let old: Preflight = serde_json::from_value(json).unwrap();
        assert_eq!(old.cache_reported, None);
        assert_eq!(old.cache_check(), CacheCheck::Reused);
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
