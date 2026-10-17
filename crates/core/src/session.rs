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

    /// The session ended because of the engine, not the task.
    pub fn engine_failed(&self) -> bool {
        self.last_stop_reason == Some(StopReason::Error)
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
