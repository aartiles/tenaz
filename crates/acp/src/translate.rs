//! Pure translation of ACP session updates into the runner's session
//! events, with the per-session bookkeeping it needs (tool names, text).

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOptionKind, RequestPermissionRequest, SessionUpdate,
    StopReason as AcpStop, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
};
use std::collections::HashMap;
use tenaz_core::session::{PermissionOption, SessionEvent, StopReason, ToolEffect, Usage};

/// A tool call the agent announced that has not run yet.
#[derive(Debug)]
struct Pending {
    name: String,
    /// The latest input the agent sent for it.
    args: serde_json::Value,
    /// What its latest `kind` says it does to files.
    effect: Option<ToolEffect>,
}

/// Session state the translation depends on.
#[derive(Debug, Default)]
pub struct Translator {
    /// Calls announced as pending, by id. Such a call is reported only when
    /// it is about to run (its permission request, `in_progress` or its
    /// end), with the latest input: `OpenCode` and Qwen Code announce a call
    /// before its input is complete (`bash` with only `cwd`, a read with
    /// `{}`) and send the input with `in_progress`; fx sends it whole. Loop
    /// checks then see what the call really does.
    announced: HashMap<String, Pending>,
    /// The names of reported calls, by id, until they end.
    running: HashMap<String, String>,
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
                let id = call.tool_call_id.to_string();
                // A call announced again is the same call: what earlier
                // updates sent is kept, and a running call stays running.
                if !self.running.contains_key(&id) {
                    self.announced.entry(id).or_insert_with(|| Pending {
                        name: call.title.clone(),
                        args: serde_json::Value::Null,
                        effect: None,
                    });
                }
                self.tool_call_update(&ToolCallUpdate::from(call.clone()))
            }
            SessionUpdate::ToolCallUpdate(update) => self.tool_call_update(update),
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

    fn tool_call_update(&mut self, update: &ToolCallUpdate) -> Vec<SessionEvent> {
        let id = update.tool_call_id.to_string();
        self.refresh(&id, &update.fields);
        match update.fields.status {
            None | Some(ToolCallStatus::Pending) => Vec::new(),
            Some(status @ (ToolCallStatus::Completed | ToolCallStatus::Failed)) => self
                .start(&id)
                .into_iter()
                .chain(self.end(&id, &update.fields, status == ToolCallStatus::Failed))
                .collect(),
            // `in_progress`, and any status a later protocol adds: the call runs.
            Some(_) => self.start(&id).into_iter().collect(),
        }
    }

    /// Takes the name and input an update brings for a pending call. A new
    /// title is ignored: it describes the call for people (`ReadFile:
    /// a.txt`), and the loop checks need a name that stays the same.
    fn refresh(&mut self, id: &str, fields: &ToolCallUpdateFields) {
        if let Some(call) = self.announced.get_mut(id) {
            if let Some(name) = &fields.name {
                call.name.clone_from(name);
            }
            if let Some(input) = &fields.raw_input {
                call.args.clone_from(input);
            }
            // A later `other` (the default) does not undo a declared effect.
            if let Some(declared) = fields.kind.and_then(effect) {
                call.effect = Some(declared);
            }
        }
    }

    /// The start of a pending call, which then runs.
    fn start(&mut self, id: &str) -> Option<SessionEvent> {
        let call = self.announced.remove(id)?;
        self.running.insert(id.to_owned(), call.name.clone());
        Some(SessionEvent::ToolCallStart {
            id: id.to_owned(),
            name: call.name,
            args: call.args,
            effect: call.effect,
        })
    }

    /// The end of a running call. Any other call was never reported as
    /// started (a pending one its prompt's end forgot), so it has no end.
    fn end(
        &mut self,
        id: &str,
        fields: &ToolCallUpdateFields,
        is_error: bool,
    ) -> Option<SessionEvent> {
        let started = self.running.remove(id)?;
        let result_text = fields
            .content
            .as_ref()
            .map(|c| content_text(c))
            .filter(|t| !t.is_empty())
            .or_else(|| fields.raw_output.as_ref().map(ToString::to_string))
            .unwrap_or_default();
        Some(SessionEvent::ToolCallEnd {
            id: id.to_owned(),
            name: fields.name.clone().unwrap_or(started),
            result_text,
            is_error,
        })
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
        sent: Option<tenaz_core::session::RequestSettings>,
    ) -> Vec<SessionEvent> {
        let text = std::mem::take(&mut self.text);
        let thinking_chars = std::mem::take(&mut self.thinking_chars);
        self.first_token_sent = false;
        vec![
            SessionEvent::MessageEnd {
                server,
                sent,
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
    /// A call still pending never ran, and is forgotten without a report.
    /// Message text that arrived after the last request ended (`dsh`
    /// sends it so) is kept as late text of that message.
    pub fn prompt_end(&mut self, stop: AcpStop, request_seen: bool) -> Vec<SessionEvent> {
        self.announced.clear();
        let aborted = stop == AcpStop::Cancelled;
        let mut events = Vec::new();
        if !request_seen {
            let usage = Usage {
                input: self.last_used.unwrap_or(0),
                ..Default::default()
            };
            events.extend(self.request_end(usage, stop_reason(stop), None, None, None));
        } else if !self.text.is_empty() {
            events.push(SessionEvent::LateText {
                text: std::mem::take(&mut self.text),
            });
        }
        events.push(SessionEvent::Settled { aborted });
        events
    }

    /// The events of a permission request: the start of its call, if not
    /// yet reported (with the request's input, the latest), then the request.
    pub fn permission_events(&mut self, request: &RequestPermissionRequest) -> Vec<SessionEvent> {
        let id = request.tool_call.tool_call_id.to_string();
        self.refresh(&id, &request.tool_call.fields);
        let start = self.start(&id);
        // The request names the call as its start did, not by its title.
        let (tool, args) = match &start {
            Some(SessionEvent::ToolCallStart { name, args, .. }) => {
                (Some(name.clone()), Some(args.clone()))
            }
            _ => (self.running.get(&id).cloned(), None),
        };
        let mut events: Vec<_> = start.into_iter().collect();
        events.push(Self::permission(request, tool, args));
        events
    }

    /// A permission request as the runner sees it. `tool` and `args` are
    /// the call's name and input as the translator knows them; for a call
    /// already running it keeps only the name.
    fn permission(
        request: &RequestPermissionRequest,
        tool: Option<String>,
        args: Option<serde_json::Value>,
    ) -> SessionEvent {
        let fields = &request.tool_call.fields;
        SessionEvent::PermissionRequest {
            id: request.tool_call.tool_call_id.to_string(),
            tool: tool
                .or_else(|| fields.name.clone())
                .or_else(|| fields.title.clone())
                .unwrap_or_default(),
            args: args
                .or_else(|| fields.raw_input.clone())
                .unwrap_or(serde_json::Value::Null),
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

/// The reasoning text an update carries, if it is a thought chunk.
pub fn thought_text(update: &SessionUpdate) -> Option<&str> {
    match update {
        SessionUpdate::AgentThoughtChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => Some(&text.text),
            _ => None,
        },
        _ => None,
    }
}

/// What an ACP tool kind does to files: `read` reads, `edit` writes.
/// `delete` and `move` are left out: no agent has sent them to Tenaz. Any other kind says nothing, and the
/// watchdog then goes by the call's name (ADR 0021).
fn effect(kind: ToolKind) -> Option<ToolEffect> {
    match kind {
        ToolKind::Read => Some(ToolEffect::Read),
        ToolKind::Edit => Some(ToolEffect::Write),
        _ => None,
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

/// A call's result as text. A diff is named by its path, not copied:
/// agents such as Qwen Code send the whole file before and after, which
/// would count as a big output.
fn content_text(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(inner) => match &inner.content {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            },
            ToolCallContent::Diff(diff) => Some(format!("wrote {}\n", diff.path.display())),
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

    /// The id of the one call each test follows.
    const ID: &str = "c1";

    fn announce(t: &mut Translator, name: &str, args: serde_json::Value) {
        let call = ToolCall::new(ToolCallId::new(ID), name).raw_input(args);
        assert!(
            t.update(&SessionUpdate::ToolCall(call)).is_empty(),
            "a pending call waits until it runs"
        );
    }

    fn update(fields: ToolCallUpdateFields) -> SessionUpdate {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(ToolCallId::new(ID), fields))
    }

    fn running() -> ToolCallUpdateFields {
        ToolCallUpdateFields::new().status(ToolCallStatus::InProgress)
    }

    fn start(name: &str, args: serde_json::Value) -> SessionEvent {
        SessionEvent::ToolCallStart {
            id: ID.into(),
            name: name.into(),
            args,
            effect: None,
        }
    }

    fn end(name: &str, result_text: &str, is_error: bool) -> SessionEvent {
        SessionEvent::ToolCallEnd {
            id: ID.into(),
            name: name.into(),
            result_text: result_text.into(),
            is_error,
        }
    }

    fn permission_request(fields: ToolCallUpdateFields) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new("s"),
            ToolCallUpdate::new(ToolCallId::new(ID), fields),
            Vec::new(),
        )
    }

    #[test]
    fn an_edit_ends_with_the_file_it_changed_not_the_file_twice() {
        use agent_client_protocol::schema::v1::Diff;
        let mut t = Translator::new();
        announce(&mut t, "Edit", serde_json::json!({"file_path": "/w/a.py"}));
        t.update(&update(running()));
        let whole = "x = 1\n".repeat(5000);
        let done = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Diff(
                Diff::new("/w/a.py", format!("{whole}y = 2\n")).old_text(whole.clone()),
            )])
            .raw_output(serde_json::json!({"originalContent": whole, "newContent": "..."}));
        assert_eq!(
            t.update(&update(done)),
            vec![end("Edit", "wrote /w/a.py\n", false)]
        );
    }

    #[test]
    fn tool_calls_become_start_and_end_events_with_their_names() {
        let mut t = Translator::new();
        let call = ToolCall::new(ToolCallId::new(ID), "Run shell")
            .name("mcp_lease_slow_render")
            .raw_input(serde_json::json!({"seconds": 10}));
        assert!(t.update(&SessionUpdate::ToolCall(call)).is_empty());
        let started = start("mcp_lease_slow_render", serde_json::json!({"seconds": 10}));
        assert_eq!(t.update(&update(running())), vec![started]);
        assert!(
            t.update(&update(running())).is_empty(),
            "a call starts once"
        );
        let done = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new("/tmp/out.png"),
            ))]);
        assert_eq!(
            t.update(&update(done)),
            vec![end("mcp_lease_slow_render", "/tmp/out.png", false)]
        );
    }

    /// The effect of a call announced with `announced` and run with
    /// `running` as its kind.
    fn effect_of(announced: ToolKind, running_kind: ToolKind) -> Option<ToolEffect> {
        let mut t = Translator::new();
        let call = ToolCall::new(ToolCallId::new(ID), "ToolCall").kind(announced);
        assert!(t.update(&SessionUpdate::ToolCall(call)).is_empty());
        match t.update(&update(running().kind(running_kind))).as_slice() {
            [SessionEvent::ToolCallStart { effect, .. }] => *effect,
            other => panic!("expected one start, got {other:?}"),
        }
    }

    #[test]
    fn a_call_starts_with_the_effect_its_kind_declares() {
        use ToolKind::{Delete, Edit, Execute, Other, Read, Search};
        let cases = [
            (Read, Other, Some(ToolEffect::Read)),
            (Other, Edit, Some(ToolEffect::Write)),
            (Read, Edit, Some(ToolEffect::Write)),
            (Edit, Other, Some(ToolEffect::Write)),
            (Other, Delete, None),
            (Other, Execute, None),
            (Search, Other, None),
        ];
        // (announced kind, running kind, effect)
        for (announced, running_kind, want) in cases {
            assert_eq!(
                effect_of(announced, running_kind),
                want,
                "announced {announced:?}, running {running_kind:?}"
            );
        }
    }

    #[test]
    fn a_call_starts_with_the_input_sent_when_it_runs() {
        // OpenCode 1.19.5 and Qwen Code 0.26.6: `bash` announced with only
        // `cwd`, its command sent with `in_progress`.
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({"cwd": "/p"}));
        let full = serde_json::json!({"cwd": "/p", "command": "cat probe.txt"});
        assert_eq!(
            t.update(&update(running().raw_input(full.clone()))),
            vec![start("bash", full)]
        );
    }

    #[test]
    fn a_name_and_input_sent_before_the_call_runs_are_kept() {
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({}));
        let input = serde_json::json!({"command": "ls"});
        let streamed = ToolCallUpdateFields::new()
            .name("shell")
            .raw_input(input.clone());
        assert!(t.update(&update(streamed)).is_empty());
        assert_eq!(t.update(&update(running())), vec![start("shell", input)]);
    }

    #[test]
    fn calls_in_parallel_start_and_end_on_their_own() {
        let mut t = Translator::new();
        let step = |t: &mut Translator, id: &str, fields: ToolCallUpdateFields| {
            t.update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(id),
                fields,
            )))
        };
        let status = |s| ToolCallUpdateFields::new().status(s);
        for id in ["a", "b"] {
            let call = ToolCall::new(ToolCallId::new(id), "read").raw_input(serde_json::json!({}));
            assert!(t.update(&SessionUpdate::ToolCall(call)).is_empty());
            let input = ToolCallUpdateFields::new().raw_input(serde_json::json!({ "path": id }));
            assert!(step(&mut t, id, input).is_empty());
        }
        let starts = |events: &[SessionEvent], want: &str| matches!(events, [SessionEvent::ToolCallStart { id, args, .. }] if id == want && args["path"] == want);
        let ends = |events: &[SessionEvent], want: &str| matches!(events, [SessionEvent::ToolCallEnd { id, name, .. }] if id == want && name == "read");
        let a = step(&mut t, "a", status(ToolCallStatus::InProgress));
        assert!(starts(&a, "a"), "{a:?}");
        let b = step(&mut t, "b", status(ToolCallStatus::InProgress));
        assert!(starts(&b, "b"), "{b:?}");
        let b = step(&mut t, "b", status(ToolCallStatus::Completed));
        assert!(ends(&b, "b"), "{b:?}");
        let a = step(&mut t, "a", status(ToolCallStatus::Completed));
        assert!(ends(&a, "a"), "{a:?}");
    }

    #[test]
    fn a_pending_call_announced_again_keeps_what_updates_sent() {
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({}));
        let input = serde_json::json!({"command": "ls"});
        let sent = ToolCallUpdateFields::new()
            .name("shell")
            .raw_input(input.clone());
        t.update(&update(sent));
        let again = ToolCall::new(ToolCallId::new(ID), "bash");
        assert!(t.update(&SessionUpdate::ToolCall(again)).is_empty());
        assert_eq!(t.update(&update(running())), vec![start("shell", input)]);
    }

    #[test]
    fn a_running_call_announced_again_does_not_start_again() {
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({"command": "ls"}));
        t.update(&update(running()));
        let again = ToolCall::new(ToolCallId::new(ID), "bash");
        assert!(t.update(&SessionUpdate::ToolCall(again)).is_empty());
        let done = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        assert_eq!(t.update(&update(done)), vec![end("bash", "", false)]);
    }

    #[test]
    fn a_call_announced_as_done_starts_and_ends_at_once() {
        let mut t = Translator::new();
        let call = ToolCall::new(ToolCallId::new(ID), "ls").status(ToolCallStatus::Completed);
        assert_eq!(
            t.update(&SessionUpdate::ToolCall(call)),
            vec![start("ls", serde_json::Value::Null), end("ls", "", false)]
        );
    }

    #[test]
    fn updates_for_a_call_never_announced_are_not_reported() {
        let mut t = Translator::new();
        assert!(t.update(&update(running())).is_empty());
        let done = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        assert!(t.update(&update(done)).is_empty());
    }

    #[test]
    fn a_failed_call_that_never_ran_starts_and_ends_at_once() {
        let mut t = Translator::new();
        announce(&mut t, "ReadFile", serde_json::json!({}));
        let failed = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .raw_input(serde_json::json!({"file_path": "a"}));
        assert_eq!(
            t.update(&update(failed)),
            vec![
                start("ReadFile", serde_json::json!({"file_path": "a"})),
                end("ReadFile", "", true),
            ]
        );
    }

    #[test]
    fn a_call_starts_before_its_permission_request() {
        // fx 0.0.16: the whole input in `tool_call` (titled "Running"),
        // then the request, then `in_progress`.
        let mut t = Translator::new();
        announce(&mut t, "Running", serde_json::json!({"command": "ls"}));
        let events = t.permission_events(&permission_request(ToolCallUpdateFields::new()));
        let [started, asked] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert_eq!(
            *started,
            start("Running", serde_json::json!({"command": "ls"}))
        );
        assert!(matches!(asked, SessionEvent::PermissionRequest { id, .. } if id == ID));
    }

    #[test]
    fn a_call_started_by_its_permission_request_does_not_start_again() {
        let mut t = Translator::new();
        announce(&mut t, "Running", serde_json::json!({"command": "ls"}));
        let request = permission_request(ToolCallUpdateFields::new());
        t.permission_events(&request);
        assert!(
            t.update(&update(running())).is_empty(),
            "in_progress does not start it again"
        );
        assert!(
            matches!(
                t.permission_events(&request).as_slice(),
                [SessionEvent::PermissionRequest { .. }]
            ),
            "a second request does not start it again"
        );
    }

    #[test]
    fn a_permission_request_brings_the_latest_input() {
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({"cwd": "/p"}));
        let input = serde_json::json!({"cwd": "/p", "command": "rm -r build"});
        let request = permission_request(ToolCallUpdateFields::new().raw_input(input.clone()));
        assert_eq!(
            t.permission_events(&request).first(),
            Some(&start("bash", input))
        );
    }

    #[test]
    fn a_permission_request_names_the_call_as_its_start_does() {
        // The request's title describes the call for people; the runner
        // checks the call by the name and input it started with.
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({"command": "ls"}));
        let request = permission_request(ToolCallUpdateFields::new().title("Running ls"));
        let events = t.permission_events(&request);
        let [_, SessionEvent::PermissionRequest { tool, args, .. }] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert_eq!(tool, "bash");
        assert_eq!(*args, serde_json::json!({"command": "ls"}));
    }

    #[test]
    fn a_permission_request_for_an_unknown_call_is_only_a_request() {
        let mut t = Translator::new();
        let request = permission_request(ToolCallUpdateFields::new().name("bash"));
        assert!(matches!(
            t.permission_events(&request).as_slice(),
            [SessionEvent::PermissionRequest { id, .. }] if id == ID
        ));
    }

    #[test]
    fn a_call_still_pending_when_the_prompt_ends_is_never_reported() {
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({"command": "ls"}));
        let events = t.prompt_end(AcpStop::Cancelled, true);
        assert_eq!(events, vec![SessionEvent::Settled { aborted: true }]);
        assert!(
            t.update(&update(running())).is_empty(),
            "a late update does not start a call of an ended prompt"
        );
        let done = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        assert!(
            t.update(&update(done)).is_empty(),
            "a call never reported as started has no end"
        );
    }

    #[test]
    fn a_running_call_keeps_its_name_after_the_prompt_ends() {
        let mut t = Translator::new();
        announce(&mut t, "bash", serde_json::json!({"command": "sleep 1"}));
        t.update(&update(running()));
        t.prompt_end(AcpStop::Cancelled, true);
        let done = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        assert_eq!(t.update(&update(done)), vec![end("bash", "", false)]);
    }

    #[test]
    fn a_call_that_is_already_running_starts_at_once() {
        let mut t = Translator::new();
        let call = ToolCall::new(ToolCallId::new(ID), "ls").status(ToolCallStatus::InProgress);
        assert_eq!(
            t.update(&SessionUpdate::ToolCall(call)),
            vec![start("ls", serde_json::Value::Null)]
        );
    }

    #[test]
    fn only_thought_chunks_carry_thought_text() {
        let chunk = |s: &str| ContentChunk::new(ContentBlock::Text(TextContent::new(s)));
        assert_eq!(
            thought_text(&SessionUpdate::AgentThoughtChunk(chunk("hmm"))),
            Some("hmm")
        );
        assert_eq!(
            thought_text(&SessionUpdate::AgentMessageChunk(chunk("hi"))),
            None
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
    fn text_sent_after_its_request_ended_is_kept_as_late_text() {
        let mut t = Translator::new();
        let chunk = |s: &str| ContentChunk::new(ContentBlock::Text(TextContent::new(s)));
        let request_seen = true;
        t.update(&SessionUpdate::AgentMessageChunk(chunk("calling the tool")));
        let ended = t.request_end(Usage::default(), StopReason::Stop, None, None, None);
        t.update(&SessionUpdate::AgentMessageChunk(chunk("marmalade-")));
        t.update(&SessionUpdate::AgentMessageChunk(chunk("4817")));
        let settled = t.prompt_end(AcpStop::EndTurn, request_seen);

        assert!(
            matches!(&ended[0], SessionEvent::MessageEnd { text, .. } if text == "calling the tool")
        );
        assert_eq!(
            settled,
            vec![
                SessionEvent::LateText {
                    text: "marmalade-4817".to_owned()
                },
                SessionEvent::Settled { aborted: false },
            ]
        );
    }

    #[test]
    fn late_text_is_reported_once() {
        let mut t = Translator::new();
        let request_seen = true;
        t.request_end(Usage::default(), StopReason::Stop, None, None, None);
        t.update(&SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("done")),
        )));
        t.prompt_end(AcpStop::EndTurn, request_seen);
        assert_eq!(
            t.prompt_end(AcpStop::EndTurn, request_seen),
            vec![SessionEvent::Settled { aborted: false }]
        );
    }

    /// What the agent said before a prompt was cancelled is still its
    /// answer so far, and is reported as late text.
    #[test]
    fn late_text_of_a_cancelled_prompt_is_kept() {
        let mut t = Translator::new();
        let request_seen = true;
        t.request_end(Usage::default(), StopReason::Stop, None, None, None);
        t.update(&SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("half an answer")),
        )));
        assert_eq!(
            t.prompt_end(AcpStop::Cancelled, request_seen),
            vec![
                SessionEvent::LateText {
                    text: "half an answer".to_owned()
                },
                SessionEvent::Settled { aborted: true },
            ]
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
            ToolCallId::new(ID),
            ToolCallUpdateFields::new()
                .name("shell")
                .raw_input(serde_json::json!({"cmd": "ls"})),
        );
        let request = RequestPermissionRequest::new(SessionId::new("s"), update, options.clone());
        let events = Translator::new().permission_events(&request);
        assert!(
            matches!(events.as_slice(), [SessionEvent::PermissionRequest { id, tool, options, .. }] if id == ID && tool == "shell" && options.len() == 3 && options[0].kind == "allow_always")
        );
        assert_eq!(choose_option(&options, true).unwrap().to_string(), "once");
        assert_eq!(choose_option(&options, false).unwrap().to_string(), "no");
        assert_eq!(choose_option(&[], true), None);
    }
}
