//! Harness-neutral session events, usage, metrics and the classification of
//! how a session ended (engine failure, capacity limit, refused image).

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

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
    pub writes: u32,
    pub capacity_limits: Vec<u64>,
    pub big_outputs: u32,
    pub image_refusal: Option<String>,
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

    /// The session ended because of the engine or the harness, not the task.
    pub fn engine_failed(&self) -> bool {
        self.harness_exit.is_some()
            || self.last_stop_reason == Some(StopReason::Error)
            || self
                .abort_reason
                .as_deref()
                .is_some_and(|r| r.contains("provider errors"))
    }

    /// The session was stopped at its context budget.
    pub fn context_stopped(&self) -> bool {
        self.abort_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("context budget exceeded"))
    }

    /// The smallest context the engine said it could hold, if it said so.
    pub fn capacity_limit(&self) -> Option<u64> {
        self.capacity_limits.iter().copied().min()
    }
}

static CAPACITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Maximum context[^:]*:\s*(\d+) tokens").expect("static regex"));
static IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)at most \d+ images|image tokens|\bimage \d+ |images need|does not decode")
        .expect("static regex")
});

/// Sushi names the largest prompt it can hold right now:
/// "Maximum context at KV8 and prefill chunk 512: 61488 tokens."
pub fn capacity_limit(error: &str) -> Option<u64> {
    CAPACITY.captures(error).and_then(|c| c[1].parse().ok())
}

/// A vision engine refused an image the session read.
pub fn is_image_refusal(error: &str) -> bool {
    IMAGE.is_match(error)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            SessionMetrics {
                abort_reason: Some("context budget exceeded (90/60 tokens)".into()),
                ..good
            }
            .context_stopped()
        );
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
}
