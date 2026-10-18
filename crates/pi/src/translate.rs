//! Pure translation of Pi's RPC event records into the runner's session
//! events. One parsed JSON line in, zero or more `SessionEvent`s out; the
//! only state is whether the current turn has produced its first token.

use serde::Deserialize;
use serde_json::Value;
use smr_core::session::{SessionEvent, StopReason, Usage};

/// Per-session state the translation depends on.
#[derive(Debug, Default)]
pub struct Translator {
    first_token_sent: bool,
}

/// `message_end.message` as far as the runner reads it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AssistantMessage {
    role: String,
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    error_message: Option<String>,
    usage: Usage,
}

/// A content block of an assistant message or a tool result.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// The events for one record of Pi's stdout.
    pub fn event(&mut self, record: &Value) -> Vec<SessionEvent> {
        let Some(kind) = record.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        match kind {
            "response" => prompt_response(record),
            "turn_start" => {
                self.first_token_sent = false;
                vec![SessionEvent::TurnStart]
            }
            "message_update" => {
                if self.first_token_sent {
                    Vec::new()
                } else {
                    self.first_token_sent = true;
                    vec![SessionEvent::FirstToken]
                }
            }
            "tool_execution_start" => vec![SessionEvent::ToolCallStart {
                id: str_field(record, "toolCallId"),
                name: str_field(record, "toolName"),
                args: record.get("args").cloned().unwrap_or(Value::Null),
            }],
            "tool_execution_end" => vec![SessionEvent::ToolCallEnd {
                id: str_field(record, "toolCallId"),
                name: str_field(record, "toolName"),
                result_text: result_text(record.get("result")),
                is_error: record
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }],
            "message_end" => message_end(record),
            "turn_end" => vec![SessionEvent::TurnEnd],
            "compaction_start" => vec![SessionEvent::CompactionStart {
                reason: record
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
            }],
            "agent_settled" => vec![SessionEvent::Settled {
                aborted: record
                    .get("aborted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }],
            _ => Vec::new(),
        }
    }
}

/// A `response` to a `prompt` that did not start a run ends the session:
/// no `agent_settled` will follow it.
fn prompt_response(record: &Value) -> Vec<SessionEvent> {
    if record.get("command").and_then(Value::as_str) != Some("prompt") {
        return Vec::new();
    }
    let success = record
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let disposition = record
        .get("data")
        .and_then(|d| d.get("disposition"))
        .and_then(Value::as_str);
    if success && disposition != Some("handled") {
        return Vec::new();
    }
    let reason = record
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| disposition.map(str::to_owned))
        .unwrap_or_else(|| "rejected".to_owned());
    vec![
        SessionEvent::PromptRejected { reason },
        SessionEvent::Settled { aborted: true },
    ]
}

fn message_end(record: &Value) -> Vec<SessionEvent> {
    let Some(message) = record.get("message") else {
        return Vec::new();
    };
    let Ok(message) = serde_json::from_value::<AssistantMessage>(message.clone()) else {
        return Vec::new();
    };
    if message.role != "assistant" {
        return Vec::new();
    }
    let text = message
        .content
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text.as_deref())
        .collect::<String>();
    let thinking_chars = message
        .content
        .iter()
        .filter(|b| b.kind == "thinking")
        .filter_map(|b| b.thinking.as_deref())
        .map(|t| t.chars().count())
        .sum();
    vec![SessionEvent::MessageEnd {
        usage: message.usage,
        stop_reason: stop_reason(message.stop_reason.as_deref()),
        error_message: message.error_message,
        thinking_chars,
        text,
    }]
}

/// The text of a tool result: its text blocks joined, or the whole result
/// as JSON when it has no content array. Images cost few tokens and are
/// not counted, as in the task loop.
pub fn result_text(result: Option<&Value>) -> String {
    match result {
        None | Some(Value::Null) => String::new(),
        Some(result) => match result.get("content").and_then(Value::as_array) {
            Some(blocks) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect(),
            None => result.to_string(),
        },
    }
}

/// Pi's stop reasons as the runner names them.
pub fn stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") => StopReason::Stop,
        Some("toolUse") => StopReason::ToolUse,
        Some("maxTokens" | "length") => StopReason::MaxTokens,
        Some("aborted") => StopReason::Aborted,
        Some("error") => StopReason::Error,
        Some(other) => StopReason::Other(other.to_owned()),
        None => StopReason::Other("unknown".to_owned()),
    }
}

fn str_field(record: &Value, name: &str) -> String {
    record
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use smr_core::session::capacity_limit;

    use super::*;

    fn events(t: &mut Translator, lines: &[Value]) -> Vec<SessionEvent> {
        lines.iter().flat_map(|l| t.event(l)).collect()
    }

    #[test]
    fn a_tool_turn_as_fake_pi_plays_it() {
        let mut t = Translator::new();
        let message = json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "plan"},
                {"type": "toolCall", "id": "call_1", "name": "write", "arguments": {"path": "a.txt"}}
            ],
            "stopReason": "toolUse",
            "usage": {"input": 1010, "output": 20, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 1030, "cost": {"total": 0}}
        });
        let got = events(
            &mut t,
            &[
                json!({"id": "prompt-0", "type": "response", "command": "prompt", "success": true, "data": {"disposition": "started"}}),
                json!({"type": "agent_start"}),
                json!({"type": "turn_start"}),
                json!({"type": "message_update", "usage": {}, "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "pl"}}),
                json!({"type": "message_update", "usage": {}, "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "an"}}),
                json!({"type": "tool_execution_start", "toolCallId": "call_1", "toolName": "write", "args": {"path": "a.txt"}}),
                json!({"type": "tool_execution_end", "toolCallId": "call_1", "toolName": "write", "result": {"content": [{"type": "text", "text": "ok"}]}, "isError": false, "durationMs": 3}),
                json!({"type": "message_end", "message": message}),
                json!({"type": "turn_end", "message": message, "toolResults": []}),
            ],
        );
        assert_eq!(
            got,
            vec![
                SessionEvent::TurnStart,
                SessionEvent::FirstToken,
                SessionEvent::ToolCallStart {
                    id: "call_1".into(),
                    name: "write".into(),
                    args: json!({"path": "a.txt"})
                },
                SessionEvent::ToolCallEnd {
                    id: "call_1".into(),
                    name: "write".into(),
                    result_text: "ok".into(),
                    is_error: false
                },
                SessionEvent::MessageEnd {
                    usage: Usage {
                        input: 1010,
                        output: 20,
                        ..Default::default()
                    },
                    stop_reason: StopReason::ToolUse,
                    error_message: None,
                    thinking_chars: 4,
                    text: String::new()
                },
                SessionEvent::TurnEnd,
            ]
        );
    }

    #[test]
    fn the_final_message_and_settled_end_the_prompt() {
        let mut t = Translator::new();
        let message = json!({
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "wrap up"}, {"type": "text", "text": "Done."}],
            "stopReason": "stop",
            "usage": {"input": 1200, "output": 10, "cacheRead": 900, "cacheWrite": 0}
        });
        let got = events(
            &mut t,
            &[
                json!({"type": "message_end", "message": message}),
                json!({"type": "turn_end", "message": message, "toolResults": []}),
                json!({"type": "agent_end", "messages": [], "willRetry": false}),
                json!({"type": "agent_settled", "aborted": false}),
            ],
        );
        assert_eq!(got.len(), 3);
        assert!(matches!(
            &got[0],
            SessionEvent::MessageEnd { usage, stop_reason: StopReason::Stop, thinking_chars: 7, text, .. }
                if text == "Done." && usage.cache_read == 900 && usage.context() == 2110
        ));
        assert_eq!(got[1], SessionEvent::TurnEnd);
        assert_eq!(got[2], SessionEvent::Settled { aborted: false });
        assert_eq!(
            t.event(&json!({"type": "agent_settled", "aborted": true})),
            vec![SessionEvent::Settled { aborted: true }]
        );
    }

    /// What Pi 1.1.0 sent after `clear_queue` + `abort` during a long
    /// thinking stream: the cut-off message (no `reasoning` in its usage),
    /// then the settle, then the `abort` response last.
    #[test]
    fn an_aborted_stream_as_pi_1_1_0_ends_it() {
        let mut t = Translator::new();
        let message = json!({
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "`proj` is", "thinkingSignature": "reasoning_content"}],
            "api": "openai-completions", "provider": "sushi", "model": "Qwen3.8-Flash-Next-Sushi-2.6bpw",
            "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "stopReason": "aborted", "timestamp": 1_792_277_264_208_u64, "responseId": "chatcmpl-1792277264213",
            "errorMessage": "Request was aborted", "durationMs": 203_641, "thinkingLevel": "medium"
        });
        let got = events(
            &mut t,
            &[
                json!({"type": "response", "command": "clear_queue", "success": true, "data": {"steering": [], "followUp": []}}),
                json!({"type": "message_end", "message": message}),
                json!({"type": "turn_end", "message": message, "toolResults": []}),
                json!({"type": "agent_end", "messages": []}),
                json!({"type": "agent_settled", "aborted": true}),
                json!({"type": "response", "command": "abort", "success": true}),
            ],
        );
        assert_eq!(
            got,
            vec![
                SessionEvent::MessageEnd {
                    usage: Usage::default(),
                    stop_reason: StopReason::Aborted,
                    error_message: Some("Request was aborted".into()),
                    thinking_chars: 9,
                    text: String::new()
                },
                SessionEvent::TurnEnd,
                SessionEvent::Settled { aborted: true },
            ]
        );
    }

    #[test]
    fn user_messages_and_unknown_records_are_ignored() {
        let mut t = Translator::new();
        assert!(
            t.event(&json!({"type": "message_end", "message": {"role": "user", "content": "hi"}}))
                .is_empty()
        );
        assert!(
            t.event(&json!({"type": "message_start", "message": {"role": "assistant"}}))
                .is_empty()
        );
        assert!(
            t.event(&json!({"type": "queue_update", "steering": [], "followUp": []}))
                .is_empty()
        );
        assert!(t.event(&json!({"no": "type"})).is_empty());
        assert!(t.event(&json!("a string")).is_empty());
        assert!(
            t.event(&json!({"type": "response", "command": "steer", "success": true, "data": {"disposition": "queued"}}))
                .is_empty()
        );
    }

    #[test]
    fn engine_errors_keep_their_message_and_capacity_limit() {
        let mut t = Translator::new();
        let error = "400: {\"message\":\"Prompt (98353 tokens) requires ~4406MB GPU memory but only ~3494MB is available. Maximum context at KV8 and prefill chunk 512: 61488 tokens.\"}";
        let got = t.event(&json!({"type": "message_end", "message": {
            "role": "assistant", "content": [], "stopReason": "error", "errorMessage": error,
            "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
        }}));
        let [
            SessionEvent::MessageEnd {
                stop_reason,
                error_message,
                usage,
                ..
            },
        ] = got.as_slice()
        else {
            panic!("{got:?}");
        };
        assert_eq!(*stop_reason, StopReason::Error);
        assert_eq!(*usage, Usage::default());
        assert_eq!(
            capacity_limit(error_message.as_deref().unwrap()),
            Some(61488)
        );
        let plain = t.event(&json!({"type": "message_end", "message": {
            "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "400: not enough GPU memory"
        }}));
        assert!(
            matches!(&plain[0], SessionEvent::MessageEnd { error_message: Some(m), .. } if m == "400: not enough GPU memory")
        );
    }

    #[test]
    fn stop_reasons_map_to_the_runners_names() {
        assert_eq!(stop_reason(Some("stop")), StopReason::Stop);
        assert_eq!(stop_reason(Some("toolUse")), StopReason::ToolUse);
        assert_eq!(stop_reason(Some("maxTokens")), StopReason::MaxTokens);
        assert_eq!(stop_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(stop_reason(Some("aborted")), StopReason::Aborted);
        assert_eq!(stop_reason(Some("error")), StopReason::Error);
        assert_eq!(
            stop_reason(Some("refusal")),
            StopReason::Other("refusal".into())
        );
        assert_eq!(stop_reason(None), StopReason::Other("unknown".into()));
        // `StopReason::as_str` round-trips Pi's own spelling.
        assert_eq!(
            stop_reason(Some(StopReason::ToolUse.as_str())),
            StopReason::ToolUse
        );
    }

    #[test]
    fn tool_results_without_text_blocks_fall_back_to_json() {
        assert_eq!(
            result_text(Some(&json!({"content": [
                {"type": "text", "text": "a"},
                {"type": "image", "data": "...", "mimeType": "image/png"},
                {"type": "text", "text": "b"}
            ]}))),
            "ab"
        );
        assert_eq!(
            result_text(Some(&json!({"output": "x"}))),
            "{\"output\":\"x\"}"
        );
        assert_eq!(result_text(None), "");
        assert_eq!(result_text(Some(&Value::Null)), "");
        let err = Translator::new().event(&json!({"type": "tool_execution_end", "toolCallId": "c", "toolName": "bash", "result": {"content": [{"type": "text", "text": "no such file"}]}, "isError": true}));
        assert!(
            matches!(&err[0], SessionEvent::ToolCallEnd { is_error: true, result_text, .. } if result_text == "no such file")
        );
    }

    #[test]
    fn a_prompt_that_did_not_start_a_run_ends_the_session() {
        let mut t = Translator::new();
        let rejected = t.event(&json!({"id": "prompt-0", "type": "response", "command": "prompt", "success": false, "error": "Model not found: x/y"}));
        assert_eq!(
            rejected,
            vec![
                SessionEvent::PromptRejected {
                    reason: "Model not found: x/y".into()
                },
                SessionEvent::Settled { aborted: true }
            ]
        );
        let handled = t.event(&json!({"type": "response", "command": "prompt", "success": true, "data": {"disposition": "handled"}}));
        assert!(
            matches!(&handled[0], SessionEvent::PromptRejected { reason } if reason == "handled")
        );
        let queued = t.event(&json!({"type": "response", "command": "prompt", "success": true, "data": {"disposition": "queued"}}));
        assert!(queued.is_empty());
    }

    #[test]
    fn first_token_once_per_turn_and_compaction_reason() {
        let mut t = Translator::new();
        let update = json!({"type": "message_update", "usage": {}, "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "x"}});
        assert_eq!(t.event(&update), vec![SessionEvent::FirstToken]);
        assert!(t.event(&update).is_empty());
        t.event(&json!({"type": "turn_start"}));
        assert_eq!(t.event(&update), vec![SessionEvent::FirstToken]);
        assert_eq!(
            t.event(&json!({"type": "compaction_start", "reason": "overflow"})),
            vec![SessionEvent::CompactionStart {
                reason: "overflow".into()
            }]
        );
    }
}
