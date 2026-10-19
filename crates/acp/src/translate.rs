//! Pure translation of ACP session updates into the runner's session
//! events, with the per-session bookkeeping it needs (tool names, text).

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOptionKind, RequestPermissionRequest, SessionUpdate,
    StopReason as AcpStop, ToolCallContent, ToolCallStatus,
};
use std::collections::HashMap;
use tenaz_core::session::{PermissionOption, SessionEvent, StopReason, Usage};

/// Session state the translation depends on.
#[derive(Debug, Default)]
pub struct Translator {
    tool_names: HashMap<String, String>,
    text: String,
    thinking_chars: usize,
    first_token_sent: bool,
    /// `used` from the agent's last `usage_update`.
    pub last_used: Option<u64>,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Events for one `session/update`.
    pub fn update(&mut self, update: &SessionUpdate) -> Vec<SessionEvent> {
        match update {
            SessionUpdate::ToolCall(call) => {
                let name = call.name.clone().unwrap_or_else(|| call.title.clone());
                self.tool_names
                    .insert(call.tool_call_id.to_string(), name.clone());
                vec![SessionEvent::ToolCallStart {
                    id: call.tool_call_id.to_string(),
                    name,
                    args: call.raw_input.clone().unwrap_or(serde_json::Value::Null),
                }]
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.to_string();
                match update.fields.status {
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed) => {
                        let name = update
                            .fields
                            .name
                            .clone()
                            .or_else(|| self.tool_names.remove(&id))
                            .unwrap_or_default();
                        let result_text = update
                            .fields
                            .content
                            .as_ref()
                            .map(|c| content_text(c))
                            .filter(|t| !t.is_empty())
                            .or_else(|| update.fields.raw_output.as_ref().map(ToString::to_string))
                            .unwrap_or_default();
                        vec![SessionEvent::ToolCallEnd {
                            id,
                            name,
                            result_text,
                            is_error: update.fields.status == Some(ToolCallStatus::Failed),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = &chunk.content {
                    self.text.push_str(&text.text);
                }
                self.first_token()
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text(text) = &chunk.content {
                    self.thinking_chars += text.text.chars().count();
                }
                self.first_token()
            }
            SessionUpdate::UsageUpdate(usage) => {
                self.last_used = Some(usage.used);
                Vec::new()
            }
            SessionUpdate::CompactionUpdate(_) => vec![SessionEvent::CompactionStart {
                reason: "agent compaction".to_owned(),
            }],
            _ => Vec::new(),
        }
    }

    fn first_token(&mut self) -> Vec<SessionEvent> {
        if self.first_token_sent {
            Vec::new()
        } else {
            self.first_token_sent = true;
            vec![SessionEvent::FirstToken]
        }
    }

    /// The events of one model request's end, from the proxy's usage.
    pub fn request_end(
        &mut self,
        usage: Usage,
        stop_reason: StopReason,
        error_message: Option<String>,
        server: Option<tenaz_core::session::ServerTimings>,
    ) -> Vec<SessionEvent> {
        let text = std::mem::take(&mut self.text);
        let thinking_chars = std::mem::take(&mut self.thinking_chars);
        self.first_token_sent = false;
        vec![
            SessionEvent::MessageEnd {
                server,
                usage,
                stop_reason,
                error_message,
                thinking_chars,
                text,
            },
            SessionEvent::TurnEnd,
        ]
    }

    /// The events when the agent finishes the prompt. Without a proxy feed,
    /// the whole prompt is one turn and `used` is the agent's figure.
    pub fn prompt_end(&mut self, stop: AcpStop, request_seen: bool) -> Vec<SessionEvent> {
        let aborted = stop == AcpStop::Cancelled;
        let mut events = Vec::new();
        if !request_seen {
            let usage = Usage {
                input: self.last_used.unwrap_or(0),
                ..Default::default()
            };
            events.extend(self.request_end(usage, stop_reason(stop), None, None));
        }
        events.push(SessionEvent::Settled { aborted });
        events
    }

    /// A permission request as the runner sees it.
    pub fn permission(request: &RequestPermissionRequest) -> SessionEvent {
        let fields = &request.tool_call.fields;
        SessionEvent::PermissionRequest {
            id: request.tool_call.tool_call_id.to_string(),
            tool: fields
                .name
                .clone()
                .or_else(|| fields.title.clone())
                .unwrap_or_default(),
            args: fields.raw_input.clone().unwrap_or(serde_json::Value::Null),
            options: request
                .options
                .iter()
                .map(|o| PermissionOption {
                    id: o.option_id.to_string(),
                    kind: kind_name(o.kind).to_owned(),
                    name: o.name.clone(),
                })
                .collect(),
        }
    }
}

fn kind_name(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

/// The option to pick for a decision: allow once, else allow always; reject once, else reject always.
pub fn choose_option(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
    allow: bool,
) -> Option<agent_client_protocol::schema::v1::PermissionOptionId> {
    let wanted: [PermissionOptionKind; 2] = if allow {
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    wanted
        .iter()
        .find_map(|kind| options.iter().find(|o| o.kind == *kind))
        .map(|o| o.option_id.clone())
}

fn content_text(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(inner) => match &inner.content {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect::<String>()
}

/// ACP stop reasons as the runner names them.
pub fn stop_reason(stop: AcpStop) -> StopReason {
    match stop {
        AcpStop::EndTurn => StopReason::Stop,
        AcpStop::MaxTokens => StopReason::MaxTokens,
        AcpStop::Cancelled => StopReason::Aborted,
        AcpStop::Refusal => StopReason::Other("refusal".to_owned()),
        AcpStop::MaxTurnRequests => StopReason::Other("max_turn_requests".to_owned()),
        _ => StopReason::Other("unknown".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        ContentChunk, PermissionOption as AcpOption, PermissionOptionId, SessionId, TextContent,
        ToolCall, ToolCallId, ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
    };

    use super::*;

    #[test]
    fn tool_calls_become_start_and_end_events_with_their_names() {
        let mut t = Translator::new();
        let call = ToolCall::new(ToolCallId::new("c1"), "Run shell")
            .name("mcp_lease_slow_render")
            .raw_input(serde_json::json!({"seconds": 10}));
        let start = t.update(&SessionUpdate::ToolCall(call));
        assert_eq!(
            start,
            vec![SessionEvent::ToolCallStart {
                id: "c1".into(),
                name: "mcp_lease_slow_render".into(),
                args: serde_json::json!({"seconds": 10})
            }]
        );
        let progress = ToolCallUpdate::new(
            ToolCallId::new("c1"),
            ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
        );
        assert!(
            t.update(&SessionUpdate::ToolCallUpdate(progress))
                .is_empty()
        );
        let done = ToolCallUpdate::new(
            ToolCallId::new("c1"),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    TextContent::new("/tmp/out.png"),
                ))]),
        );
        let end = t.update(&SessionUpdate::ToolCallUpdate(done));
        assert_eq!(
            end,
            vec![SessionEvent::ToolCallEnd {
                id: "c1".into(),
                name: "mcp_lease_slow_render".into(),
                result_text: "/tmp/out.png".into(),
                is_error: false
            }]
        );
    }

    #[test]
    fn text_and_thoughts_accumulate_into_the_message_end() {
        let mut t = Translator::new();
        let chunk = |s: &str| ContentChunk::new(ContentBlock::Text(TextContent::new(s)));
        assert_eq!(
            t.update(&SessionUpdate::AgentThoughtChunk(chunk("hmm"))),
            vec![SessionEvent::FirstToken]
        );
        assert!(
            t.update(&SessionUpdate::AgentMessageChunk(chunk("hel")))
                .is_empty()
        );
        t.update(&SessionUpdate::AgentMessageChunk(chunk("lo")));
        t.update(&SessionUpdate::UsageUpdate(UsageUpdate::new(8060, 131072)));
        let events = t.prompt_end(AcpStop::EndTurn, false);
        assert_eq!(events.len(), 3);
        assert!(
            matches!(&events[0], SessionEvent::MessageEnd { usage, stop_reason: StopReason::Stop, thinking_chars: 3, text, .. } if text == "hello" && usage.input == 8060)
        );
        assert_eq!(events[1], SessionEvent::TurnEnd);
        assert_eq!(events[2], SessionEvent::Settled { aborted: false });
        assert_eq!(
            t.prompt_end(AcpStop::Cancelled, true),
            vec![SessionEvent::Settled { aborted: true }]
        );
    }

    #[test]
    fn permission_requests_and_option_choice() {
        let options = vec![
            AcpOption::new(
                PermissionOptionId::new("always"),
                "Always",
                PermissionOptionKind::AllowAlways,
            ),
            AcpOption::new(
                PermissionOptionId::new("once"),
                "Once",
                PermissionOptionKind::AllowOnce,
            ),
            AcpOption::new(
                PermissionOptionId::new("no"),
                "No",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        let update = ToolCallUpdate::new(
            ToolCallId::new("c9"),
            ToolCallUpdateFields::new()
                .name("shell")
                .raw_input(serde_json::json!({"cmd": "ls"})),
        );
        let request = RequestPermissionRequest::new(SessionId::new("s"), update, options.clone());
        let event = Translator::permission(&request);
        assert!(
            matches!(&event, SessionEvent::PermissionRequest { id, tool, options, .. } if id == "c9" && tool == "shell" && options.len() == 3 && options[0].kind == "allow_always")
        );
        assert_eq!(choose_option(&options, true).unwrap().to_string(), "once");
        assert_eq!(choose_option(&options, false).unwrap().to_string(), "no");
        assert_eq!(choose_option(&[], true), None);
    }
}
