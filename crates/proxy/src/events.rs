//! What the proxy reports about each request.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use smr_core::session::{capacity_limit, is_image_refusal};

/// Token usage of one engine response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyUsage {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
}

/// What an engine error means to the runner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineErrorKind {
    /// The prompt is larger than the engine can hold now; it named its maximum.
    Capacity {
        max_context: u64,
    },
    /// A vision engine refused an image in the conversation.
    ImageRefused,
    /// No connection to the engine.
    Unreachable,
    Other,
}

/// One proxy event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProxyEvent {
    /// The request waited for the lease to return the LLM.
    Held { request: u64, ms: u64 },
    /// The engine answered (any status). `usage` is present for completions
    /// that reported it; `first_byte_ms` is the time to the first body byte.
    Completed {
        request: u64,
        path: String,
        status: u16,
        stream: bool,
        usage: Option<ProxyUsage>,
        /// The engine's `finish_reason` (`stop`, `tool_calls`, `length`, …)
        /// when the response named one.
        finish_reason: Option<String>,
        /// The engine's own `timings` object, when the response carried one
        /// (prefill and decode time; speculative drafts when the engine
        /// reports them there).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timings: Option<smr_core::session::ServerTimings>,
        first_byte_ms: Option<u64>,
        total_ms: u64,
    },
    /// The engine returned an error status or could not be reached.
    EngineError {
        request: u64,
        status: Option<u16>,
        message: String,
        kind: EngineErrorKind,
    },
}

/// Receives proxy events (the runner feeds them to its watchdog and journal).
pub type Observer = Arc<dyn Fn(ProxyEvent) + Send + Sync>;

/// Reads OpenAI-style usage from a response or chunk object.
pub fn usage_from_json(value: &serde_json::Value) -> Option<ProxyUsage> {
    let usage = value.get("usage")?.as_object()?;
    let num = |v: Option<&serde_json::Value>| v.and_then(serde_json::Value::as_u64).unwrap_or(0);
    let prompt_tokens = num(usage.get("prompt_tokens")).max(num(usage.get("input_tokens")));
    let completion_tokens =
        num(usage.get("completion_tokens")).max(num(usage.get("output_tokens")));
    let cached_tokens = num(usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens")))
    .max(num(usage.get("cached_tokens")))
    .max(num(usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))));
    let reasoning_tokens = num(usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens")))
    .max(num(usage.get("reasoning_tokens")));
    Some(ProxyUsage {
        prompt_tokens,
        cached_tokens,
        completion_tokens,
        reasoning_tokens,
    })
}

/// The `finish_reason` of the first choice that names one, from a response
/// or a chunk object (`null` while the choice is still streaming).
pub fn finish_reason_from_json(value: &serde_json::Value) -> Option<String> {
    value.get("choices")?.as_array()?.iter().find_map(|choice| {
        choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

/// The error message of an engine error body, and what it means.
pub fn classify_error(body: &[u8]) -> (String, EngineErrorKind) {
    let text = String::from_utf8_lossy(body);
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|j| {
            j.get("error")
                .and_then(|e| {
                    e.get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| e.as_str().map(str::to_owned))
                })
                .or_else(|| {
                    j.get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .or_else(|| {
                    j.get("detail")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
        })
        .unwrap_or_else(|| text.trim().to_owned());
    let kind = if let Some(max_context) = capacity_limit(&message) {
        EngineErrorKind::Capacity { max_context }
    } else if is_image_refusal(&message) {
        EngineErrorKind::ImageRefused
    } else {
        EngineErrorKind::Other
    };
    (message, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_read_in_openai_and_responses_shapes() {
        let chat = serde_json::json!({"usage": {"prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 90}, "completion_tokens_details": {"reasoning_tokens": 3}}});
        assert_eq!(
            usage_from_json(&chat),
            Some(ProxyUsage {
                prompt_tokens: 100,
                cached_tokens: 90,
                completion_tokens: 7,
                reasoning_tokens: 3
            })
        );
        assert_eq!(
            finish_reason_from_json(
                &serde_json::json!({"choices": [{"delta": {}, "finish_reason": null}]})
            ),
            None
        );
        assert_eq!(
            finish_reason_from_json(
                &serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
            )
            .as_deref(),
            Some("tool_calls")
        );
        assert_eq!(
            finish_reason_from_json(&serde_json::json!({"usage": {}})),
            None
        );
        let responses = serde_json::json!({"usage": {"input_tokens": 5, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 4}}});
        assert_eq!(
            usage_from_json(&responses),
            Some(ProxyUsage {
                prompt_tokens: 5,
                cached_tokens: 4,
                completion_tokens: 2,
                reasoning_tokens: 0
            })
        );
        assert_eq!(usage_from_json(&serde_json::json!({"usage": null})), None);
        assert_eq!(usage_from_json(&serde_json::json!({})), None);
    }

    #[test]
    fn errors_are_classified() {
        let capacity = br#"{"error":{"message":"Prompt (98353 tokens) requires more memory. Maximum context at KV8 and prefill chunk 512: 61488 tokens."}}"#;
        assert_eq!(
            classify_error(capacity).1,
            EngineErrorKind::Capacity { max_context: 61488 }
        );
        let image = br#"{"message":"This request has at most 4 images"}"#;
        assert_eq!(
            classify_error(image),
            (
                "This request has at most 4 images".to_owned(),
                EngineErrorKind::ImageRefused
            )
        );
        assert_eq!(
            classify_error(b"plain failure"),
            ("plain failure".to_owned(), EngineErrorKind::Other)
        );
        assert_eq!(classify_error(br#"{"error":"short form"}"#).0, "short form");
    }
}
