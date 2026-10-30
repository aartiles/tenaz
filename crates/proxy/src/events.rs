//! What the proxy reports about each request.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tenaz_core::session::{capacity_limit, is_image_refusal, is_out_of_memory};

/// Token usage of one engine response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyUsage {
    pub prompt_tokens: u64,
    /// `None` when the usage did not name its cached tokens (LM Studio 0.4
    /// leaves them out), which is not the same as 0.
    pub cached_tokens: Option<u64>,
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
    /// The engine ran out of memory on the request; it may fit again later.
    OutOfMemory,
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
        timings: Option<tenaz_core::session::ServerTimings>,
        /// What a completion request asked for, as the proxy forwarded it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent: Option<Box<tenaz_core::session::RequestSettings>>,
        first_byte_ms: Option<u64>,
        total_ms: u64,
    },
    /// A tool call's arguments came back with objects as JSON text where the
    /// tool's schema accepts only objects; they were parsed back.
    ToolArgsRepaired {
        request: u64,
        tool: String,
        fields: Vec<String>,
    },
    /// The engine returned an error status, sent an error inside a stream
    /// that started with status 200, or could not be reached.
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
    let cached_in = |details: &str| {
        usage
            .get(details)
            .and_then(|d| d.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
    };
    let cached_tokens = [
        cached_in("prompt_tokens_details"),
        usage
            .get("cached_tokens")
            .and_then(serde_json::Value::as_u64),
        cached_in("input_tokens_details"),
    ]
    .into_iter()
    .flatten()
    .max();
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

/// The output limit a completion request asks for, under any of the names
/// the chat, completions and responses APIs use.
pub fn output_limit(request: &serde_json::Value) -> Option<u64> {
    ["max_completion_tokens", "max_tokens", "max_output_tokens"]
        .iter()
        .find_map(|key| request.get(*key).and_then(serde_json::Value::as_u64))
}

/// The `finish_reason` to report: `length` when the reply reached the
/// output limit, whatever the engine said. Some engines end a reply cut
/// inside a tool call's arguments with `tool_calls`.
pub fn reported_finish_reason(
    finish_reason: Option<String>,
    usage: Option<&ProxyUsage>,
    limit: Option<u64>,
) -> Option<String> {
    match (usage, limit) {
        (Some(usage), Some(limit)) if limit > 0 && usage.completion_tokens >= limit => {
            Some("length".to_owned())
        }
        _ => finish_reason,
    }
}

/// The error message of an engine error body, and what it means.
pub fn classify_error(body: &[u8]) -> (String, EngineErrorKind) {
    let text = String::from_utf8_lossy(body);
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|j| error_message(&j))
        .unwrap_or_else(|| text.trim().to_owned());
    let kind = classify_message(&message);
    (message, kind)
}

/// The error an engine sent inside a stream that had started well (status
/// 200), as Sushi does when a generation fails part way:
/// `data: {"error":{"message":…},"choices":[{"finish_reason":"error"}]}`.
pub fn stream_error(chunk: &serde_json::Value) -> Option<(String, EngineErrorKind)> {
    let error = chunk.get("error").filter(|e| {
        e.as_object().is_some_and(|o| !o.is_empty()) || e.as_str().is_some_and(|s| !s.is_empty())
    })?;
    let message = error_message(chunk).unwrap_or_else(|| error.to_string());
    let kind = classify_message(&message);
    Some((message, kind))
}

/// The message of an error object, in the shapes engines use: an `error`
/// object (its `message`, else its `type` or `code`) or string, or a
/// top-level `message` or `detail`.
fn error_message(json: &serde_json::Value) -> Option<String> {
    let text = |v: Option<&serde_json::Value>| {
        v.and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    json.get("error")
        .and_then(|e| {
            text(e.get("message"))
                .or_else(|| text(e.get("type")))
                .or_else(|| text(e.get("code")))
                .or_else(|| text(Some(e)))
        })
        .or_else(|| text(json.get("message")))
        .or_else(|| text(json.get("detail")))
}

/// What an engine error message means to the runner.
fn classify_message(message: &str) -> EngineErrorKind {
    if let Some(max_context) = capacity_limit(message) {
        EngineErrorKind::Capacity { max_context }
    } else if is_image_refusal(message) {
        EngineErrorKind::ImageRefused
    } else if is_out_of_memory(message) {
        EngineErrorKind::OutOfMemory
    } else {
        EngineErrorKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_at_the_output_limit_is_reported_as_cut() {
        let usage = |completion_tokens| ProxyUsage {
            completion_tokens,
            ..Default::default()
        };
        let tool = || Some("tool_calls".to_owned());
        assert_eq!(
            reported_finish_reason(tool(), Some(&usage(16384)), Some(16384)).as_deref(),
            Some("length")
        );
        assert_eq!(
            reported_finish_reason(tool(), Some(&usage(16383)), Some(16384)).as_deref(),
            Some("tool_calls")
        );
        assert_eq!(
            reported_finish_reason(tool(), Some(&usage(16384)), None).as_deref(),
            Some("tool_calls")
        );
        assert_eq!(
            output_limit(&serde_json::json!({"max_tokens": 16384, "model": "m"})),
            Some(16384)
        );
        assert_eq!(
            output_limit(&serde_json::json!({"max_completion_tokens": 900})),
            Some(900)
        );
        assert_eq!(output_limit(&serde_json::json!({"model": "m"})), None);
    }

    #[test]
    fn usage_is_read_in_openai_and_responses_shapes() {
        let chat = serde_json::json!({"usage": {"prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 90}, "completion_tokens_details": {"reasoning_tokens": 3}}});
        assert_eq!(
            usage_from_json(&chat),
            Some(ProxyUsage {
                prompt_tokens: 100,
                cached_tokens: Some(90),
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
                cached_tokens: Some(4),
                completion_tokens: 2,
                reasoning_tokens: 0
            })
        );
        let lm_studio = serde_json::json!({"usage": {"prompt_tokens": 2796, "completion_tokens": 9, "total_tokens": 2805}});
        assert_eq!(usage_from_json(&lm_studio).unwrap().cached_tokens, None);
        let zero = serde_json::json!({"usage": {"prompt_tokens": 9, "prompt_tokens_details": {"cached_tokens": 0}}});
        assert_eq!(usage_from_json(&zero).unwrap().cached_tokens, Some(0));
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
        assert_eq!(
            classify_error(br#"{"error":{"message":"GenerationOutOfMemory"}}"#).1,
            EngineErrorKind::OutOfMemory
        );
    }

    /// mlx-serve 26.10.3 refuses a prompt whose prefill does not fit in GPU
    /// memory. The message is recorded (Digger 3D run, 2026-10-29); the
    /// `error.message` body around it is assumed, not captured. The text is
    /// `MLX_SERVE_REFUSAL` in tenaz-core's `session` tests.
    #[test]
    fn an_mlx_serve_memory_refusal_is_out_of_memory() {
        let body = br#"{"error":{"message":"This prompt (30245 tokens) does not fit in GPU memory even after freeing the prefix cache: it requires ~7720MB GPU memory but only ~6960MB is available. It was refused before any work started. Reduce the prompt length, lower --ctx-size, or free memory on the machine."}}"#;
        assert_eq!(classify_error(body).1, EngineErrorKind::OutOfMemory);
    }

    /// Sushi's chat-completions stream error: the message, else the type.
    #[test]
    fn an_error_inside_a_stream_is_read_and_classified() {
        let chunk = |error: serde_json::Value| {
            serde_json::json!({
                "error": error,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "error"}]
            })
        };
        assert_eq!(
            stream_error(&chunk(serde_json::json!({
                "message": "error.GenerationOutOfMemory", "type": "server_error",
                "param": null, "code": null
            }))),
            Some((
                "error.GenerationOutOfMemory".to_owned(),
                EngineErrorKind::OutOfMemory
            ))
        );
        assert_eq!(
            stream_error(&chunk(
                serde_json::json!({"message": "", "type": "GenerationOutOfMemory"})
            )),
            Some((
                "GenerationOutOfMemory".to_owned(),
                EngineErrorKind::OutOfMemory
            ))
        );
        assert_eq!(
            stream_error(&serde_json::json!({"error": "GenerationOutOfMemory"})),
            Some((
                "GenerationOutOfMemory".to_owned(),
                EngineErrorKind::OutOfMemory
            ))
        );
        assert_eq!(
            stream_error(&serde_json::json!({"choices": [{"delta": {"content": "hi"}}]})),
            None
        );
        for empty in [
            serde_json::Value::Null,
            serde_json::json!(false),
            serde_json::json!(""),
            serde_json::json!({}),
        ] {
            assert_eq!(stream_error(&serde_json::json!({"error": empty})), None);
        }
    }
}
