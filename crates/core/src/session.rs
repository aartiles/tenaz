//! Harness-neutral session events, usage, metrics and the classification of
//! how a session ended (engine failure, capacity limit, refused image).

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The abort reason of a session whose context the harness compacted twice.
pub const SECOND_COMPACTION: &str = "second context compaction";

/// Token usage of one assistant message, as the harness reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl Usage {
    /// Prompt plus completion: the context the request occupied.
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.output
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.reasoning += other.reasoning;
    }
}

/// Why an assistant message ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    ToolUse,
    MaxTokens,
    Aborted,
    Error,
    Other(String),
}

impl StopReason {
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::Stop => "stop",
            StopReason::ToolUse => "toolUse",
            StopReason::MaxTokens => "maxTokens",
            StopReason::Aborted => "aborted",
            StopReason::Error => "error",
            StopReason::Other(s) => s,
        }
    }
}

/// One choice offered by a permission request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub kind: String,
    pub name: String,
}

/// What happens in a session, independent of the harness that runs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The harness refused the prompt.
    PromptRejected {
        reason: String,
    },
    TurnStart,
    /// The first token of the assistant's reply arrived.
    FirstToken,
    ToolCallStart {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolCallEnd {
        id: String,
        name: String,
        result_text: String,
        is_error: bool,
    },
    MessageEnd {
        usage: Usage,
        stop_reason: StopReason,
        error_message: Option<String>,
        thinking_chars: usize,
        text: String,
        /// What the engine reported about this request itself, when the
        /// response carried it (seen by the engine proxy).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<ServerTimings>,
        /// The sampling and limits the request asked for, as the engine
        /// proxy forwarded it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent: Option<RequestSettings>,
    },
    TurnEnd,
    CompactionStart {
        reason: String,
    },
    /// The harness asks whether a tool call may run. The session runner answers.
    PermissionRequest {
        id: String,
        tool: String,
        args: serde_json::Value,
        options: Vec<PermissionOption>,
    },
    /// The harness finished every queued prompt (or was aborted).
    Settled {
        aborted: bool,
    },
    /// The harness process ended.
    Exited {
        code: Option<i32>,
    },
}

/// The engine's own figures for one request, from the `timings` object an
/// OpenAI-compatible engine may add to its last chunk (llama.cpp's layout,
/// which Sushi follows): prefill and decode time, and speculative drafts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerTimings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Draft tokens proposed (llama.cpp `draft_n`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drafted: Option<u64>,
    /// Draft tokens accepted (llama.cpp `draft_n_accepted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<u64>,
}

/// What a completion request asked of the engine, as the engine proxy
/// forwarded it (after the proxy's own defaults). A field the request did
/// not carry is `None`: the engine used its own default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// `max_completion_tokens`, `max_tokens` or `max_output_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

impl RequestSettings {
    /// Reads them from a completion request body.
    pub fn from_request(request: &serde_json::Value) -> Self {
        let float = |key: &str| request.get(key).and_then(serde_json::Value::as_f64);
        let int = |key: &str| request.get(key).and_then(serde_json::Value::as_u64);
        Self {
            temperature: float("temperature"),
            top_p: float("top_p"),
            top_k: int("top_k"),
            reasoning_effort: request
                .get("reasoning_effort")
                .or_else(|| request.get("reasoning").and_then(|r| r.get("effort")))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            max_tokens: ["max_completion_tokens", "max_tokens", "max_output_tokens"]
                .into_iter()
                .find_map(int),
        }
    }
}

impl ServerTimings {
    /// Reads a `timings` object; `None` when it holds none of the known fields.
    pub fn from_json(timings: &serde_json::Value) -> Option<Self> {
        let object = timings.as_object()?;
        let float = |key: &str| object.get(key).and_then(serde_json::Value::as_f64);
        let int = |key: &str| object.get(key).and_then(serde_json::Value::as_u64);
        let parsed = Self {
            prompt_ms: float("prompt_ms"),
            decode_ms: float("predicted_ms"),
            cached_tokens: int("cached_n"),
            drafted: int("draft_n"),
            accepted: int("draft_n_accepted"),
        };
        (parsed != Self::default()).then_some(parsed)
    }
}

/// Tool names that change files, for the idle rule's quick count.
pub fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "write" | "edit" | "Write" | "Edit" | "fs_write" | "edit_file" | "write_file"
    )
}

/// Tool names that read files: a read of something new counts as progress
/// for the idle rule.
pub fn is_read_tool(name: &str) -> bool {
    matches!(name, "read" | "Read" | "read_file" | "fs_read")
}

/// A watchdog event recorded in the session's journal record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogEvent {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub turn: u32,
    pub ms: u64,
}

/// One assistant message, kept for the preflight's checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantTurn {
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub thinking: usize,
}

/// Everything the watchdog and the session runner measured.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub turns: u32,
    pub usage: Usage,
    pub reasoning_chars: usize,
    pub context_peak: u64,
    pub compactions: u32,
    pub provider_errors: u32,
    pub tool_calls: u32,
    /// Tool calls the harness reported as failed, and the first of them.
    #[serde(default)]
    pub tool_errors: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_tool_error: Option<String>,
    pub writes: u32,
    /// Files the write tools named outside the project, the temporary
    /// directories and `sandboxAllowWrite`, resolved, each once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outside_writes: Vec<String>,
    pub capacity_limits: Vec<u64>,
    pub big_outputs: u32,
    pub image_refusal: Option<String>,
    /// The session's last engine error said the engine ran out of memory
    /// ([`is_out_of_memory`]), and no reply came after it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub engine_out_of_memory: bool,
    pub events: Vec<WatchdogEvent>,
    pub texts: Vec<String>,
    pub assistant: Vec<AssistantTurn>,
    pub settled: bool,
    pub aborted: bool,
    pub abort_reason: Option<String>,
    pub exit_code: Option<i32>,
    pub last_stop_reason: Option<StopReason>,
    pub duration_ms: u64,
    /// The harness ended before the turn settled, without the runner
    /// stopping it: what happened, with the tail of its stderr.
    pub harness_exit: Option<String>,
    /// A tool call that had started and not ended when the harness exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_in_flight: Option<String>,
}

impl SessionMetrics {
    /// A session counts as settled cleanly when every prompt was answered,
    /// nothing aborted it, the harness exited 0 and the last message was not
    /// an error.
    pub fn ok(&self) -> bool {
        self.settled
            && !self.aborted
            && self.abort_reason.is_none()
            && self.exit_code == Some(0)
            && self.last_stop_reason != Some(StopReason::Error)
    }

    /// The harness ended before its turn settled and nobody stopped it: a
    /// crash of the harness process, or a connection lost to it.
    pub fn ended_early(&self) -> bool {
        !self.settled && !self.aborted && self.abort_reason.is_none()
    }

    /// What ended the session as far as it says: its abort reason, else
    /// the last reply's stop reason.
    pub fn stop_cause(&self) -> Option<String> {
        self.abort_reason.clone().or_else(|| {
            self.last_stop_reason
                .as_ref()
                .map(|r| r.as_str().to_owned())
        })
    }

    /// Why a session that did not settle cleanly stopped; empty when it
    /// says nothing (a harness that died before its first reply). `None`
    /// for a clean session.
    pub fn stopped(&self) -> Option<String> {
        (!self.ok()).then(|| self.stop_cause().unwrap_or_default())
    }

    /// The last reply stopped at the output limit (`length`), usually while
    /// the model was still reasoning.
    pub fn output_truncated(&self) -> bool {
        self.last_stop_reason == Some(StopReason::MaxTokens)
    }

    /// The session ended because of the engine or the harness, not the task.
    /// A harness that ended while one of the session's tool calls ran, or
    /// after a reply cut at the output limit, is not counted: the session
    /// caused it, and an unchanged rerun would do the same.
    pub fn engine_failed(&self) -> bool {
        (self.harness_exit.is_some() && self.tool_in_flight.is_none() && !self.output_truncated())
            || self.last_stop_reason == Some(StopReason::Error)
            || self
                .abort_reason
                .as_deref()
                .is_some_and(|r| r.contains("provider errors"))
    }

    /// The model called tools and every call failed: it is not using the
    /// harness's tools the way they work, whatever the task asked.
    pub fn every_tool_call_failed(&self) -> bool {
        self.tool_calls > 0 && self.tool_errors >= self.tool_calls
    }

    /// The session ran out of context: stopped at its context budget, or
    /// at its second compaction.
    pub fn context_stopped(&self) -> bool {
        self.compaction_stopped()
            || self
                .abort_reason
                .as_deref()
                .is_some_and(|r| r.starts_with("context budget exceeded"))
    }

    /// The session was stopped when the harness compacted its context a
    /// second time.
    pub fn compaction_stopped(&self) -> bool {
        self.abort_reason.as_deref() == Some(SECOND_COMPACTION)
    }

    /// The session was stopped at one and a half times its turn budget.
    pub fn turns_stopped(&self) -> bool {
        self.abort_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("turn budget exceeded"))
    }

    /// The smallest context the engine said it could hold, if it said so.
    pub fn capacity_limit(&self) -> Option<u64> {
        self.capacity_limits.iter().copied().min()
    }

    /// The harness refused the prompt as more than the model's window can
    /// hold: fx ends with `ContextCapacityExceeded` before any reply. The
    /// same prompt is refused again (ADR 0012). After a reply, fx failed to
    /// compact what the session read since, and a rerun may pass.
    pub fn prompt_refused(&self) -> bool {
        self.assistant.is_empty()
            && self
                .harness_exit
                .as_deref()
                .is_some_and(|exit| exit.contains(CONTEXT_EXCEEDED))
    }
}

/// What fx says when a prompt does not fit the model's window.
const CONTEXT_EXCEEDED: &str = "ContextCapacityExceeded";
static CAPACITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Maximum context[^:]*:\s*(\d+) tokens").expect("static regex"));
static OUT_OF_MEMORY: LazyLock<Regex> = LazyLock::new(|| {
    // `OutOfMemory` in CamelCase, as Sushi and Metal name it, is matched
    // case-sensitively: without case it would match `layoutOfMemory`.
    Regex::new(
        r"(?i:\bout[ _-]?of[ _-]?memory|\binsufficient memory\b|\bnot enough (?:\w+ )?memory\b|\bdoes not fit in (?:\w+ ){0,2}memory\b)|OutOfMemory",
    )
    .expect("static regex")
});
static IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)at most \d+ images|image tokens|\bimage \d+ |images need|does not decode")
        .expect("static regex")
});

/// Sushi names the largest prompt it can hold right now:
/// "Maximum context at KV8 and prefill chunk 512: 61488 tokens."
pub fn capacity_limit(error: &str) -> Option<u64> {
    CAPACITY.captures(error).and_then(|c| c[1].parse().ok())
}

/// The engine ran out of memory while it worked on a request. Sushi sends
/// `GenerationOutOfMemory` when Metal cannot get the memory, typically
/// because another program is using the GPU at the same time; mlx-serve
/// refuses a prompt whose prefill "does not fit in GPU memory" (ADR 0012).
/// This is not a prompt too large for the engine (`capacity_limit`): the
/// same prompt fits again once the memory is back.
pub fn is_out_of_memory(error: &str) -> bool {
    OUT_OF_MEMORY.is_match(error)
}

/// A vision engine refused an image the session read.
pub fn is_image_refusal(error: &str) -> bool {
    IMAGE.is_match(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mlx-serve 26.10.3, in the Digger 3D run of 2026-10-29. The proxy's
    /// `an_mlx_serve_memory_refusal_is_out_of_memory` sends the same text.
    const MLX_SERVE_REFUSAL: &str = "This prompt (30245 tokens) does not fit in GPU memory even after freeing the prefix cache: it requires ~7720MB GPU memory but only ~6960MB is available. It was refused before any work started. Reduce the prompt length, lower --ctx-size, or free memory on the machine.";

    #[test]
    fn an_engine_out_of_memory_is_told_from_other_errors() {
        for message in [
            "error.GenerationOutOfMemory",
            "CUDA out of memory",
            "Insufficient Memory (kIOGPUCommandBufferCallbackErrorOutOfMemory)",
            "kIOGPUCommandBufferCallbackErrorOutOfMemory",
            "out_of_memory",
            "400: not enough GPU memory",
            "not enough memory",
            MLX_SERVE_REFUSAL,
            "does not fit in memory",
            "does not fit in free GPU memory",
        ] {
            assert!(is_out_of_memory(message), "{message}");
        }
        for message in [
            "timeout of memory check",
            "layout of memory map",
            "layoutOfMemory",
            "not enough tokens",
            "does not fit in the context window",
            "boom",
        ] {
            assert!(!is_out_of_memory(message), "{message}");
        }
    }

    #[test]
    fn request_settings_read_what_the_request_carries() {
        let sent = RequestSettings::from_request(&serde_json::json!({
            "model": "m", "temperature": 0.8, "top_k": 20,
            "reasoning_effort": "low", "max_tokens": 16384
        }));
        assert_eq!(
            sent,
            RequestSettings {
                temperature: Some(0.8),
                top_p: None,
                top_k: Some(20),
                reasoning_effort: Some("low".into()),
                max_tokens: Some(16384),
            }
        );
        assert_eq!(
            serde_json::to_value(&sent).unwrap(),
            serde_json::json!({"temperature": 0.8, "top_k": 20, "reasoning_effort": "low", "max_tokens": 16384})
        );
    }

    #[test]
    fn a_session_whose_every_tool_call_failed_is_told_apart() {
        let mut m = SessionMetrics::default();
        assert!(
            !m.every_tool_call_failed(),
            "no tool calls is not a tool failure"
        );
        m.tool_calls = 3;
        m.tool_errors = 2;
        assert!(!m.every_tool_call_failed());
        m.tool_errors = 3;
        assert!(m.every_tool_call_failed());
    }

    #[test]
    fn server_timings_follow_the_llama_cpp_layout() {
        let sushi = serde_json::json!({"prompt_n": 24, "cached_n": 0, "prompt_ms": 192.964, "predicted_n": 69, "predicted_ms": 866.124});
        assert_eq!(
            ServerTimings::from_json(&sushi),
            Some(ServerTimings {
                prompt_ms: Some(192.964),
                decode_ms: Some(866.124),
                cached_tokens: Some(0),
                drafted: None,
                accepted: None
            })
        );
        let llama = serde_json::json!({"prompt_ms": 1.0, "predicted_ms": 2.0, "draft_n": 30, "draft_n_accepted": 21});
        let parsed = ServerTimings::from_json(&llama).unwrap();
        assert_eq!((parsed.drafted, parsed.accepted), (Some(30), Some(21)));
        assert_eq!(
            ServerTimings::from_json(&serde_json::json!({"other": 1})),
            None
        );
        assert_eq!(ServerTimings::from_json(&serde_json::json!(null)), None);
        let json = serde_json::to_value(parsed).unwrap();
        assert!(
            json.get("cached_tokens").is_none(),
            "absent figures are not written: {json}"
        );
    }

    #[test]
    fn a_harness_that_ended_early_is_an_engine_side_failure() {
        let early = SessionMetrics {
            exit_code: Some(1),
            ..Default::default()
        };
        assert!(early.ended_early());
        assert!(
            !early.engine_failed(),
            "only the runner's note makes it one"
        );
        let noted = SessionMetrics {
            harness_exit: Some("the harness ended".into()),
            ..early
        };
        assert!(noted.engine_failed());
        let in_tool = SessionMetrics {
            tool_in_flight: Some(r#"shell {"command":"head -c 400 x.json"}"#.into()),
            ..noted.clone()
        };
        assert!(
            !in_tool.engine_failed(),
            "a crash during a tool call is the call's, not the engine's"
        );
        let truncated = SessionMetrics {
            last_stop_reason: Some(StopReason::MaxTokens),
            ..noted.clone()
        };
        assert!(truncated.output_truncated());
        assert!(
            !truncated.engine_failed(),
            "a reply cut at the output limit is the session's, not the engine's"
        );
        let aborted = SessionMetrics {
            abort_reason: Some("same bash call 5 times in a row".into()),
            ..Default::default()
        };
        assert!(
            !aborted.ended_early(),
            "a session the runner stopped did not end early"
        );
        let settled = SessionMetrics {
            settled: true,
            ..Default::default()
        };
        assert!(!settled.ended_early());
    }

    fn exited(note: &str) -> SessionMetrics {
        SessionMetrics {
            harness_exit: Some(note.to_owned()),
            ..SessionMetrics::default()
        }
    }

    /// fx 0.0.15 on a reviewer prompt of 281465 characters, in the
    /// Digger 3D run of 2026-10-29.
    #[test]
    fn a_prompt_the_harness_refused_is_told_from_other_exits() {
        assert!(exited(&crate::testing::fx_context_exceeded_exit()).prompt_refused());
        for other in [
            "the harness ended before the turn settled (exit code 1); its stderr was empty",
            MLX_SERVE_REFUSAL,
        ] {
            assert!(!exited(other).prompt_refused(), "{other}");
        }
        assert!(!SessionMetrics::default().prompt_refused());
    }

    /// After a reply the prompt fitted: fx failed to compact what came
    /// later, which a rerun may not meet again.
    #[test]
    fn a_context_exceeded_after_a_reply_is_not_a_refused_prompt() {
        let mut metrics = exited(&crate::testing::fx_context_exceeded_exit());
        metrics.assistant.push(AssistantTurn {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            thinking: 0,
        });
        assert!(!metrics.prompt_refused());
    }

    #[test]
    fn capacity_limit_is_read_from_the_engine_message() {
        let msg = "400: {\"message\":\"Prompt (98353 tokens) requires ~4406MB GPU memory but only ~3494MB is available. Maximum context at KV8 and prefill chunk 512: 61488 tokens.\"}";
        assert_eq!(capacity_limit(msg), Some(61488));
        assert_eq!(capacity_limit("400: not enough GPU memory"), None);
    }

    #[test]
    fn image_refusals_are_recognised() {
        assert!(is_image_refusal(
            "This model accepts at most 4 images per request"
        ));
        assert!(is_image_refusal("decoder does not decode webp"));
        assert!(!is_image_refusal("connection reset"));
    }

    #[test]
    fn ok_requires_a_clean_end() {
        let good = SessionMetrics {
            settled: true,
            exit_code: Some(0),
            last_stop_reason: Some(StopReason::Stop),
            ..Default::default()
        };
        assert!(good.ok());
        assert!(
            !SessionMetrics {
                aborted: true,
                ..good.clone()
            }
            .ok()
        );
        assert!(
            !SessionMetrics {
                exit_code: Some(1),
                ..good.clone()
            }
            .ok()
        );
        assert!(
            !SessionMetrics {
                last_stop_reason: Some(StopReason::Error),
                ..good.clone()
            }
            .ok()
        );
        assert!(
            SessionMetrics {
                last_stop_reason: Some(StopReason::Error),
                ..good.clone()
            }
            .engine_failed()
        );
        assert!(
            SessionMetrics {
                abort_reason: Some("3 provider errors".into()),
                ..good.clone()
            }
            .engine_failed()
        );
        let budget = SessionMetrics {
            abort_reason: Some("context budget exceeded (90/60 tokens)".into()),
            ..good.clone()
        };
        assert!(budget.context_stopped() && !budget.compaction_stopped());
        let compacted = SessionMetrics {
            abort_reason: Some(SECOND_COMPACTION.into()),
            ..good
        };
        assert!(compacted.context_stopped() && compacted.compaction_stopped());
    }

    #[test]
    fn usage_serializes_with_camel_case_like_the_journal() {
        let json = serde_json::to_value(Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            reasoning: 5,
        })
        .unwrap();
        assert_eq!(json["cacheRead"], 3);
        assert_eq!(
            Usage {
                input: 10,
                output: 5,
                cache_read: 20,
                ..Default::default()
            }
            .context(),
            35
        );
    }

    fn clean() -> SessionMetrics {
        SessionMetrics {
            settled: true,
            exit_code: Some(0),
            last_stop_reason: Some(StopReason::Stop),
            ..SessionMetrics::default()
        }
    }

    #[test]
    fn a_clean_session_was_not_stopped() {
        assert_eq!(clean().stopped(), None);
    }

    /// As the watchdog leaves a session it aborted.
    #[test]
    fn an_aborted_session_gives_its_abort_reason() {
        let metrics = SessionMetrics {
            aborted: true,
            abort_reason: Some(SECOND_COMPACTION.to_owned()),
            last_stop_reason: Some(StopReason::Aborted),
            exit_code: Some(0),
            ..SessionMetrics::default()
        };
        assert_eq!(metrics.stopped().as_deref(), Some(SECOND_COMPACTION));
    }

    #[test]
    fn a_provider_error_gives_the_error_stop_reason() {
        let metrics = SessionMetrics {
            last_stop_reason: Some(StopReason::Error),
            ..clean()
        };
        assert_eq!(metrics.stopped().as_deref(), Some("error"));
    }

    /// As the runner leaves a session whose harness process exited.
    fn crashed() -> SessionMetrics {
        SessionMetrics {
            exit_code: Some(1),
            harness_exit: Some(
                "the harness ended before the turn settled (exit code 1); its stderr was empty"
                    .to_owned(),
            ),
            ..SessionMetrics::default()
        }
    }

    #[test]
    fn a_session_that_says_nothing_has_no_stop_cause() {
        assert_eq!(crashed().stop_cause(), None);
    }

    #[test]
    fn a_harness_that_ended_early_gives_the_last_replys_stop_reason() {
        let metrics = SessionMetrics {
            last_stop_reason: Some(StopReason::ToolUse),
            ..crashed()
        };
        assert_eq!(metrics.stopped().as_deref(), Some("toolUse"));
    }

    #[test]
    fn a_harness_that_died_before_replying_stopped_with_an_empty_reason() {
        assert_eq!(crashed().stopped().as_deref(), Some(""));
    }
}
