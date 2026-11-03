//! The engine preflight: a session that finishes, reasoning at the
//! configured level, a working prefix cache, a shell command run through
//! the harness's own tool, and (for vision models) a correctly read image.

use std::path::Path;

use crate::cache_report::CacheReport;
use crate::ledger::{CacheCheck, HarnessKind, Preflight};
use crate::metrics::{RequestRecord, engine_summary};
use crate::session::{
    AssistantTurn, OUT_OF_MEMORY_ADVICE, SessionMetrics, StopReason, is_engine_unavailable,
    is_out_of_memory,
};

use super::session::HarnessRun;
use super::{RunError, Runner};

/// The file the tool probe reads, and the word in it. The word appears
/// nowhere in the prompt, so an answer with it means the tool call worked.
pub const TOOL_PROBE_FILE: &str = "probe.txt";
const TOOL_PROBE_WORD: &str = "marmalade-4817";

/// How much of each engine error the preflight keeps.
const ENGINE_ERROR_CHARS: usize = 500;

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
        engine_errors: engine_errors(m),
        accepted_weak: false,
    };
    p.ok = checks(&p).iter().all(|c| c.found.passed());
    p
}

/// A check the preflight judges the model on, as one preflight found it.
struct Check {
    /// Its name on the preflight's line.
    key: &'static str,
    /// What it found, and whether it is on the line.
    found: Found,
    /// What failed, in words, for the error that stops the run. Called with
    /// the preflight the list was built from and the run's harness: a plain
    /// `fn` cannot hold `p`, and `read_preflight`, which sets `ok`, has no
    /// harness.
    failure: fn(&Preflight, HarnessKind) -> String,
}

/// What a check found, as the preflight's line shows it. A failed check is
/// always on the line, unless the preflight is not judged.
enum Found {
    /// Passed, and left off the line: a session that finished, the image
    /// without vision, a preflight from before the tool check.
    Hidden,
    /// On the line as `key=text`.
    Shown { text: String, passed: bool },
}

impl Found {
    /// A yes or no, shown as `true` or `false`; `None`, a check the
    /// preflight did not make, is left off.
    fn yes_no(found: Option<bool>) -> Self {
        found.map_or(Self::Hidden, |passed| Self::Shown {
            text: passed.to_string(),
            passed,
        })
    }

    fn passed(&self) -> bool {
        match self {
            Self::Hidden => true,
            Self::Shown { passed, .. } => *passed,
        }
    }
}

/// The checks of `p`, in the order of its line. The only list of them: `ok`,
/// the line and the stop's error all read it.
fn checks(p: &Preflight) -> [Check; 5] {
    let cache = p.cache_check();
    [
        Check {
            key: "settled",
            // Only a session that did not finish is on the line.
            found: if p.settled {
                Found::Hidden
            } else {
                Found::Shown {
                    text: "false".to_owned(),
                    passed: false,
                }
            },
            failure: |_, _| "the session did not finish".to_owned(),
        },
        Check {
            key: "reasoning",
            found: Found::yes_no(Some(p.reasoning)),
            failure: |_, _| "no reasoning at the configured level".to_owned(),
        },
        Check {
            key: "cache",
            found: Found::Shown {
                text: cache.to_string(),
                passed: cache.passes(),
            },
            failure: |_, _| "the engine did not reuse the cached prompt prefix".to_owned(),
        },
        Check {
            key: "tools",
            found: Found::yes_no(p.tools),
            failure: |_, harness| {
                format!(
                    "the model could not run `cat {TOOL_PROBE_FILE}` with {harness}'s shell \
                     tool, so it cannot work in the project; try another model, or another \
                     harness"
                )
            },
        },
        Check {
            key: "vision",
            found: Found::yes_no(p.vision),
            failure: |preflight, _| {
                format!(
                    "the model did not read the test image correctly (it answered {:?})",
                    preflight.vision_answer.as_deref().unwrap_or_default()
                )
            },
        },
    ]
}

/// What the engine answered to each request of `m` it failed, cut to
/// [`ENGINE_ERROR_CHARS`].
fn engine_errors(m: &SessionMetrics) -> Vec<String> {
    m.engine_errors()
        .map(|message| message.chars().take(ENGINE_ERROR_CHARS).collect())
        .collect()
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
    (!unjudged(preflight) && preflight.cache_check() == CacheCheck::NotReported).then_some(
        "WARNING: the engine does not report cached prompt tokens, so the preflight cannot \
         check its prefix cache; it passes. Without a cache every turn processes the whole \
         prompt again. Check the engine's own log for prefix reuse.",
    )
}

/// What a measured preflight means for the run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Passed,
    /// It failed, and `--allow-weak-engine` runs anyway.
    AcceptedWeak,
    /// The run stops with this error.
    Stop(String),
}

/// What `preflight`, run through `harness`, means for the run, with
/// `--allow-weak-engine` when `allow_weak_engine`. A failure while the
/// engine could not take requests stops the run whatever the flag says:
/// the preflight did not measure the model, and one accepted now would
/// never be run again.
fn outcome(preflight: &Preflight, harness: HarnessKind, allow_weak_engine: bool) -> Outcome {
    if preflight.ok {
        return Outcome::Passed;
    }
    let details = details(harness);
    if unjudged(preflight) {
        let unavailable = unavailable_errors(preflight);
        let advice = if unavailable.iter().any(|e| is_out_of_memory(e)) {
            format!("it ran out of memory: {OUT_OF_MEMORY_ADVICE}")
        } else {
            "see the engine's own log".to_owned()
        };
        return Outcome::Stop(format!(
            "Engine preflight failed: the engine could not take its requests: {}. That is \
             the engine's error, not something the model lacks; {advice}, then run again. \
             Details in {details}.",
            unavailable.join(" | ")
        ));
    }
    if allow_weak_engine {
        return Outcome::AcceptedWeak;
    }
    Outcome::Stop(format!(
        "Engine preflight failed: {}. Details in {details}; pass --allow-weak-engine to run anyway.",
        failures(harness, preflight)
    ))
}

/// Decides a measured preflight: what the run keeps, and what it means for
/// the run (see [`outcome`]). A preflight `--allow-weak-engine` accepts is
/// kept as passed and accepted weak; one the engine did not let run is
/// never accepted, so it stays failed and its line shows it as not judged.
pub(crate) fn decide(
    measured: Preflight,
    harness: HarnessKind,
    allow_weak_engine: bool,
) -> (Preflight, Outcome) {
    let outcome = outcome(&measured, harness, allow_weak_engine);
    let accepted_weak = outcome == Outcome::AcceptedWeak;
    let kept = Preflight {
        ok: measured.ok || accepted_weak,
        accepted_weak,
        ..measured
    };
    (kept, outcome)
}

/// The errors, each once, of an engine that could not take the
/// preflight's requests (out of memory, busy, not reached). A refused image
/// or a bad request is left to the checks.
fn unavailable_errors(preflight: &Preflight) -> Vec<&str> {
    let mut errors: Vec<&str> = Vec::new();
    for error in preflight
        .engine_errors
        .iter()
        .filter(|e| is_engine_unavailable(e))
    {
        if !errors.contains(&error.as_str()) {
            errors.push(error);
        }
    }
    errors
}

/// The preflight failed while the engine could not take some of its
/// requests, so its checks are not held against the model, even one that
/// failed on a request the engine answered.
fn unjudged(preflight: &Preflight) -> bool {
    !preflight.ok
        && preflight
            .engine_errors
            .iter()
            .any(|e| is_engine_unavailable(e))
}

/// The preflight's line, the same in the run's log and in `tenaz status`:
/// what `preflight` found, or that it has not run.
pub(crate) fn line(preflight: Option<&Preflight>) -> String {
    format!(
        "Preflight: {}",
        preflight.map_or_else(|| "not run".to_owned(), summary)
    )
}

/// What `p` found, on one line. The checks of a preflight the engine did
/// not let run are left out.
fn summary(p: &Preflight) -> String {
    let errors = match p.engine_errors.len() {
        0 => String::new(),
        n => format!(" engine_errors={n}"),
    };
    if unjudged(p) {
        return format!("not judged: the engine could not take some of its requests{errors}");
    }
    let mut parts: Vec<String> = checks(p)
        .into_iter()
        .filter_map(|c| match c.found {
            Found::Shown { text, .. } => Some(format!("{}={text}", c.key)),
            Found::Hidden => None,
        })
        .collect();
    if p.accepted_weak {
        parts.push("(accepted weak)".to_owned());
    }
    format!("{}{errors}", parts.join(" "))
}

/// The first error the engine answered the preflight with, for the run's
/// log.
pub(crate) fn engine_error_note(preflight: &Preflight) -> Option<String> {
    let error = preflight.engine_errors.first()?;
    Some(format!("  preflight: the engine answered {error}"))
}

/// Where a failed preflight's details are for `harness`: `run.json`, and
/// fx's own log of the session (Pi and the scripted harness keep none).
fn details(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Fx => ".tenaz/run.json \"preflight\" and .tenaz/private/preflight.fx-acp.log",
        HarnessKind::Pi | HarnessKind::OpenCode | HarnessKind::Qwen | HarnessKind::Fake => {
            ".tenaz/run.json \"preflight\""
        }
    }
}

/// What failed in `p`, in words, for the error that stops the run.
fn failures(harness: HarnessKind, p: &Preflight) -> String {
    let failed: Vec<String> = checks(p)
        .into_iter()
        .filter(|c| !c.found.passed())
        .map(|c| (c.failure)(p, harness))
        .collect();
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
            engine_errors: Vec::new(),
            accepted_weak: false,
        };
        let text = failures(HarnessKind::Fx, &p);
        assert!(text.contains("with fx's shell tool"), "{text}");
        assert!(!text.contains("cache"), "{text}");
        let old = Preflight { tools: None, ..p };
        assert_eq!(failures(HarnessKind::Fx, &old), "no check failed");
    }

    /// A session that failed every check: each is named, in the order of
    /// the line, in the stop's error and on the line. A new check must fail
    /// here too, so its text is pinned.
    #[test]
    fn every_failed_check_is_named_in_order() {
        let m = SessionMetrics {
            settled: false,
            // No reasoning; no probe word and no red, so tools and vision fail.
            texts: vec!["blue".to_owned()],
            // The second answer read nothing from the cache, which the
            // engine reports (`Some(true)` below), so the cache fails.
            assistant: vec![turn(StopReason::Stop, 0), turn(StopReason::Stop, 0)],
            ..Default::default()
        };
        let p = read_preflight(&m, &[], true, Some(true));
        for c in checks(&p) {
            assert!(!c.found.passed(), "{} passed", c.key);
        }
        assert!(!p.ok);
        let failed = [
            "the session did not finish",
            "no reasoning at the configured level",
            "the engine did not reuse the cached prompt prefix",
            "the model could not run `cat probe.txt` with pi's shell tool, so it cannot work in \
             the project; try another model, or another harness",
            "the model did not read the test image correctly (it answered \"blue\")",
        ];
        assert_eq!(failures(HarnessKind::Pi, &p), failed.join("; "));
        let line = "settled=false reasoning=false cache=false tools=false vision=false";
        assert_eq!(summary(&p), line);
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

    /// A session that did not finish fails the preflight on its own, as a
    /// judged check that `--allow-weak-engine` can accept.
    #[test]
    fn an_unfinished_session_fails_the_preflight() {
        let mut m = with_image_answer("");
        m.settled = false;
        let p = read_preflight(&m, &[], false, None);
        assert!(!p.ok);
        assert!(stop(&p).contains("the session did not finish"));
        assert_eq!(outcome(&p, HarnessKind::Pi, true), Outcome::AcceptedWeak);
    }

    #[test]
    fn without_vision_the_image_answer_is_not_checked() {
        let m = with_image_answer("blue, red");
        let p = read_preflight(&m, &[], false, None);
        assert_eq!(p.vision, None);
        assert!(p.ok);
    }

    /// A preflight session whose second answer read nothing from the cache.
    fn second_answer_uncached_metrics() -> SessionMetrics {
        let mut m = with_image_answer("");
        // The second answer, the one after the first that stopped.
        m.assistant[1] = turn(StopReason::Stop, 0);
        m
    }

    /// That preflight, with `cache_reported` from the engine proxy.
    fn second_answer_uncached(cache_reported: Option<bool>) -> Preflight {
        read_preflight(
            &second_answer_uncached_metrics(),
            &[],
            false,
            cache_reported,
        )
    }

    #[test]
    fn an_engine_that_reports_zero_cached_tokens_fails_the_cache_check() {
        let p = second_answer_uncached(Some(true));
        assert!(!p.ok);
        assert_eq!(p.cache_check(), CacheCheck::NotReused);
        let text = failures(HarnessKind::Pi, &p);
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
        assert_eq!(failures(HarnessKind::Pi, &p), "no check failed");
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

    /// mlx-serve 26.10.3's answer to each preflight request on 2026-11-02,
    /// with no memory left for them (HTTP 503 and this body), in the form
    /// Pi is assumed to record it: the status, then the raw body.
    const OUT_OF_MEMORY: &str = "503: {\"error\":{\"message\":\"not enough memory: available=0 MB\",\"type\":\"out_of_memory\"}}";

    /// The event the watchdog records for a request the engine failed.
    fn provider_error(message: &str) -> crate::session::WatchdogEvent {
        crate::session::WatchdogEvent {
            kind: crate::session::PROVIDER_ERROR.to_owned(),
            detail: Some(message.to_owned()),
            turn: 0,
            ms: 0,
        }
    }

    /// A preflight session in which the engine failed every one of the
    /// three requests with `message`.
    fn refused(message: &str) -> SessionMetrics {
        SessionMetrics {
            settled: true,
            events: vec![provider_error(message); 3],
            assistant: vec![turn(StopReason::Error, 0); 3],
            ..Default::default()
        }
    }

    /// The error that stops the run after preflight `p` without
    /// `--allow-weak-engine`; a panic when it does not stop.
    fn stop(p: &Preflight) -> String {
        match outcome(p, HarnessKind::Pi, false) {
            Outcome::Stop(error) => error,
            other => panic!("{other:?}"),
        }
    }

    /// In both forms the error arrives in: Pi's (the raw body) and the
    /// engine proxy's for fx (the message).
    #[test]
    fn an_engine_out_of_memory_is_named_not_the_model() {
        let from_proxy =
            crate::session::engine_error_text(503, "not enough memory: available=0 MB");
        for error in [OUT_OF_MEMORY, from_proxy.as_str()] {
            let p = read_preflight(&refused(error), &[], false, Some(true));
            assert_eq!(p.engine_errors, vec![error.to_owned(); 3]);
            let text = stop(&p);
            assert_eq!(text.matches("available=0 MB").count(), 1, "{text}");
            assert!(
                text.contains("could not take its requests: 503: "),
                "{text}"
            );
            assert!(text.contains(OUT_OF_MEMORY_ADVICE), "{text}");
            for blame in ["no reasoning", "cached prompt prefix", "shell tool"] {
                assert!(!text.contains(blame), "{blame} in {text}");
            }
            assert!(!text.contains("allow-weak-engine"), "{text}");
        }
    }

    #[test]
    fn allow_weak_engine_does_not_accept_an_engine_out_of_memory() {
        let p = read_preflight(&refused(OUT_OF_MEMORY), &[], false, Some(true));
        let accepted = outcome(&p, HarnessKind::Pi, true);
        assert!(matches!(accepted, Outcome::Stop(_)), "{accepted:?}");
    }

    /// One server error is enough, even beside a check that failed on its
    /// own; different errors are each named, and only memory is blamed on
    /// memory.
    #[test]
    fn one_server_error_stops_the_run_and_each_error_is_named() {
        let mut m = second_answer_uncached_metrics();
        m.events = vec![provider_error("503: busy")];
        let p = read_preflight(&m, &[], false, Some(true));
        let one = stop(&p);
        assert!(one.contains("requests: 503: busy."), "{one}");
        assert!(one.contains("see the engine's own log"), "{one}");
        assert!(!one.contains(OUT_OF_MEMORY_ADVICE), "{one}");
        assert!(!one.contains("cached prompt prefix"), "{one}");
        let accepted = outcome(&p, HarnessKind::Pi, true);
        assert!(matches!(accepted, Outcome::Stop(_)), "{accepted:?}");
        m.events.push(provider_error("engine unreachable: refused"));
        let two = stop(&read_preflight(&m, &[], false, Some(true)));
        assert!(
            two.contains("503: busy | engine unreachable: refused"),
            "{two}"
        );
    }

    /// A refused image is the model's or the request's: the image check
    /// fails, and `--allow-weak-engine` may accept it.
    #[test]
    fn a_refused_image_is_left_to_the_checks() {
        let mut m = with_image_answer("");
        m.events = vec![provider_error("400: at most 0 images per request")];
        let p = read_preflight(&m, &[], true, None);
        assert_eq!(p.vision, Some(false));
        assert!(stop(&p).contains("did not read the test image"));
        assert_eq!(outcome(&p, HarnessKind::Pi, true), Outcome::AcceptedWeak);
    }

    #[test]
    fn only_a_weak_engine_the_flag_accepts_is_kept_as_passed() {
        let p = second_answer_uncached(Some(true));
        let (kept, accepted) = decide(p.clone(), HarnessKind::Pi, true);
        assert_eq!(accepted, Outcome::AcceptedWeak);
        assert!(kept.ok && kept.accepted_weak);
        assert!(summary(&kept).ends_with(" (accepted weak)"));
        let (kept, stopped) = decide(p, HarnessKind::Pi, false);
        assert!(matches!(stopped, Outcome::Stop(_)));
        assert!(!kept.ok && !kept.accepted_weak);
        let unjudged = read_preflight(&refused(OUT_OF_MEMORY), &[], false, Some(true));
        let (kept, stopped) = decide(unjudged, HarnessKind::Pi, true);
        assert!(matches!(stopped, Outcome::Stop(_)));
        assert!(!kept.ok && summary(&kept).starts_with("not judged:"));
    }

    #[test]
    fn an_engine_error_with_every_check_passing_passes() {
        let mut m = with_image_answer("");
        m.events = vec![provider_error("503: busy")];
        let p = read_preflight(&m, &[], false, None);
        assert_eq!(p.engine_errors, ["503: busy"]);
        assert_eq!(outcome(&p, HarnessKind::Pi, false), Outcome::Passed);
    }

    #[test]
    fn a_weak_engine_is_accepted_only_when_asked() {
        let p = second_answer_uncached(Some(true));
        assert!(stop(&p).ends_with("pass --allow-weak-engine to run anyway."));
        assert_eq!(outcome(&p, HarnessKind::Pi, true), Outcome::AcceptedWeak);
    }

    #[test]
    fn a_preflight_the_engine_did_not_let_run_is_not_judged() {
        let p = read_preflight(&refused(OUT_OF_MEMORY), &[], false, Some(true));
        assert_eq!(
            summary(&p),
            "not judged: the engine could not take some of its requests engine_errors=3"
        );
    }

    #[test]
    fn a_preflight_not_judged_logs_no_cache_warning() {
        let unreported = read_preflight(&refused(OUT_OF_MEMORY), &[], false, Some(false));
        assert_eq!(warnings(&unreported), None);
    }

    /// One busy answer beside a check that failed on its own: the run
    /// stops with the engine's error, so the summary does not judge the
    /// model either.
    #[test]
    fn a_failed_check_beside_a_busy_engine_is_not_judged() {
        let mut m = second_answer_uncached_metrics();
        m.events = vec![provider_error("503: busy")];
        let p = read_preflight(&m, &[], false, Some(true));
        assert!(summary(&p).starts_with("not judged:"), "{}", summary(&p));
    }

    /// The checks are shown when they were judged: a weak engine (also
    /// accepted), engine errors beside passing checks, a refused image, a
    /// vision model's image answer (right, wrong, and wrong but accepted
    /// weak, so a preflight that failed only on the image does not read
    /// like a pass), a session that did not finish (for the same reason),
    /// and a preflight from before the tool check.
    #[test]
    fn judged_checks_are_shown_with_the_engine_errors() {
        let weak = second_answer_uncached(Some(true));
        let accepted = decide(weak.clone(), HarnessKind::Pi, true).0;
        let mut busy = with_image_answer("");
        busy.events = vec![provider_error("503: busy")];
        let mut image = with_image_answer("");
        image.events = vec![provider_error("400: at most 0 images per request")];
        let image_answer = |answer| read_preflight(&with_image_answer(answer), &[], true, None);
        let mut unfinished = with_image_answer("");
        unfinished.settled = false;
        let old = Preflight {
            tools: None,
            ..weak.clone()
        };
        for (p, expected) in [
            (weak, "reasoning=true cache=false tools=true"),
            (
                accepted,
                "reasoning=true cache=false tools=true (accepted weak)",
            ),
            (
                read_preflight(&busy, &[], false, None),
                "reasoning=true cache=true tools=true engine_errors=1",
            ),
            (
                read_preflight(&image, &[], true, None),
                "reasoning=true cache=true tools=true vision=false engine_errors=1",
            ),
            (
                image_answer("red, blue"),
                "reasoning=true cache=true tools=true vision=true",
            ),
            (
                image_answer("blue, red"),
                "reasoning=true cache=true tools=true vision=false",
            ),
            (
                decide(image_answer("blue, red"), HarnessKind::Pi, true).0,
                "reasoning=true cache=true tools=true vision=false (accepted weak)",
            ),
            (
                read_preflight(&unfinished, &[], false, None),
                "settled=false reasoning=true cache=true tools=true",
            ),
            (old, "reasoning=true cache=false"),
        ] {
            assert_eq!(summary(&p), expected);
        }
    }

    #[test]
    fn the_line_says_whether_the_preflight_ran() {
        assert_eq!(line(None), "Preflight: not run");
        let p = second_answer_uncached(Some(true));
        assert_eq!(
            line(Some(&p)),
            "Preflight: reasoning=true cache=false tools=true"
        );
    }

    #[test]
    fn a_long_engine_error_is_cut_by_characters() {
        let long = "é".repeat(ENGINE_ERROR_CHARS * 2);
        let p = read_preflight(&refused(&long), &[], false, None);
        let kept = &p.engine_errors[0];
        assert_eq!(kept.chars().count(), ENGINE_ERROR_CHARS);
        assert!(long.starts_with(kept.as_str()));
    }

    #[test]
    fn only_fx_is_pointed_to_a_preflight_log() {
        let p = second_answer_uncached(Some(true));
        let stop_with = |harness| match outcome(&p, harness, false) {
            Outcome::Stop(error) => error,
            other => panic!("{other:?}"),
        };
        let fx = stop_with(HarnessKind::Fx);
        assert!(
            fx.contains("and .tenaz/private/preflight.fx-acp.log;"),
            "{fx}"
        );
        for harness in [
            HarnessKind::Pi,
            HarnessKind::OpenCode,
            HarnessKind::Qwen,
            HarnessKind::Fake,
        ] {
            let text = stop_with(harness);
            assert!(
                text.contains("Details in .tenaz/run.json \"preflight\";"),
                "{text}"
            );
        }
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
