//! What the proxy reports about each request.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tenaz_core::session::engine_error_text;

use crate::sse::data_json;

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
    /// that started with status 200, or could not be reached. After an
    /// error status the message starts with it (`503: <message>`); an error
    /// inside a stream is the engine's own message; an engine not reached
    /// is tenaz-core's `engine_unreachable_text`. tenaz-core reads what the
    /// message means.
    EngineError { request: u64, message: String },
    /// The harness asked the engine to summarize its conversation, and the
    /// proxy refused instead of forwarding it (see
    /// [`crate::Proxy::compaction_prompt_opening`]).
    CompactionRefused { request: u64 },
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

/// The error message of an engine error body: the message of a JSON body,
/// else of the first `error` chunk of an SSE body, else the text itself.
fn read_error_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|j| error_message(&j))
        // Unlike a JSON body, only a chunk with a non-empty `error` counts:
        // other chunks are part of a reply.
        .or_else(|| data_json(&text).find_map(|json| stream_error(&json)))
        .unwrap_or_else(|| text.trim().to_owned())
}

/// The error an engine answered with an error `status`: the message of its
/// body led by the status (`503: <message>`, or the status alone without
/// one).
pub(crate) fn status_error(status: u16, body: &[u8]) -> String {
    engine_error_text(status, &read_error_body(body))
}

/// The error an SSE chunk carries, as Sushi sends it in a stream when a
/// generation fails part way:
/// `data: {"error":{"message":…},"choices":[{"finish_reason":"error"}]}`.
/// `read_error_body` reads the same chunk in the body of an error status.
pub(crate) fn stream_error(chunk: &serde_json::Value) -> Option<String> {
    let error = chunk.get("error").filter(|e| {
        e.as_object().is_some_and(|o| !o.is_empty()) || e.as_str().is_some_and(|s| !s.is_empty())
    })?;
    Some(error_message(chunk).unwrap_or_else(|| error.to_string()))
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

    /// One case for each shape an engine's error body takes.
    #[test]
    fn error_messages_are_read() {
        for (body, expected) in [
            (r#"{"error":{"message":"from message"}}"#, "from message"),
            (
                r#"{"error":{"message":"","type":"from type"}}"#,
                "from type",
            ),
            (r#"{"error":{"code":"from code"}}"#, "from code"),
            (r#"{"error":"from string"}"#, "from string"),
            (r#"{"message":"top-level message"}"#, "top-level message"),
            (r#"{"detail":"top-level detail"}"#, "top-level detail"),
            (r#"{"other":1}"#, r#"{"other":1}"#),
            ("  plain failure\n", "plain failure"),
            (
                concat!(
                    r#"data: {"choices":[]}"#,
                    "\n\n",
                    r#"data: {"error":{"message":"after a chunk"}}"#,
                    "\n\n",
                ),
                "after a chunk",
            ),
            (
                concat!(
                    r#"data: {"error":"the first"}"#,
                    "\n\n",
                    r#"data: {"error":"a later one"}"#,
                    "\n\n",
                ),
                "the first",
            ),
            (
                concat!(r#"data: {"choices":[]}"#, "\n\n"),
                r#"data: {"choices":[]}"#,
            ),
        ] {
            assert_eq!(read_error_body(body.as_bytes()), expected, "{body}");
        }
    }

    /// After an error status, the message starts with it.
    #[test]
    fn an_error_status_leads_the_message() {
        let json = r#"{"error":{"message":"Maximum context at KV8 and prefill chunk 512: 61488 tokens."}}"#;
        for (status, body, expected) in [
            (
                400,
                json,
                "400: Maximum context at KV8 and prefill chunk 512: 61488 tokens.",
            ),
            (503, "busy", "503: busy"),
            (500, "", "500"),
            (500, "  ", "500"),
        ] {
            assert_eq!(status_error(status, body.as_bytes()), expected, "{body:?}");
        }
    }

    /// Sushi's chat-completions stream error: the message, else the type.
    #[test]
    fn an_error_inside_a_stream_is_read() {
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
            })))
            .as_deref(),
            Some("error.GenerationOutOfMemory")
        );
        assert_eq!(
            stream_error(&chunk(
                serde_json::json!({"message": "", "type": "GenerationOutOfMemory"})
            ))
            .as_deref(),
            Some("GenerationOutOfMemory")
        );
        assert_eq!(
            stream_error(&serde_json::json!({"error": "GenerationOutOfMemory"})).as_deref(),
            Some("GenerationOutOfMemory")
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
