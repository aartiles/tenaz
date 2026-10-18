//! The ACP harness against an in-process fake agent built with the same
//! crate: initialize, session/new with MCP servers and meta, a prompt with
//! a tool call, a permission request the runner answers, text and usage,
//! a cancelled prompt, and fx-style steering (a second `session/prompt`
//! carrying `_meta.fx.steer` while the first one runs).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason, TextContent, ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, UsageUpdate,
};
use agent_client_protocol::{Agent, DynConnectTo};
use smr_acp::{AcpHarness, EngineObservation, McpEndpoint};
use smr_core::ports::{Harness, HarnessInfo, PermissionDecision, SessionSpec};
use smr_core::session::{SessionEvent, StopReason as CoreStop, Usage};

/// A steer prompt as the fake agent received it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SteerSeen {
    text: String,
    meta: serde_json::Value,
    /// The first prompt had not been answered yet.
    while_running: bool,
}

#[derive(Default)]
struct Seen {
    mcp_servers: usize,
    meta: Option<serde_json::Value>,
    cancelled: bool,
    /// A prompt is being worked on (its response not sent yet).
    running: bool,
    steer: Option<SteerSeen>,
}

fn is_steer(request: &PromptRequest) -> bool {
    request
        .meta
        .as_ref()
        .and_then(|m| m.get("fx"))
        .and_then(|fx| fx.get("steer"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The fake agent; with `steering` it advertises fx's extension in
/// `agentCapabilities._meta` and answers a steer prompt when the turn it
/// steered completes, as fx 0.0.14 does.
fn fake_agent(
    seen: Arc<Mutex<Seen>>,
    steering: bool,
) -> DynConnectTo<agent_client_protocol::Client> {
    let on_new = seen.clone();
    let on_cancel = seen.clone();
    let on_prompt = seen;
    let agent = Agent
        .builder()
        .name("fake-agent")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                let mut capabilities = AgentCapabilities::new();
                if steering {
                    capabilities = capabilities.meta(serde_json::json!({"fx": {"steering": true}}).as_object().cloned());
                }
                responder.respond(InitializeResponse::new(request.protocol_version).agent_capabilities(capabilities))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _cx| {
                let mut seen = on_new.lock().unwrap();
                seen.mcp_servers = request.mcp_servers.len();
                seen.meta = request.meta.as_ref().map(|m| serde_json::to_value(m).unwrap());
                drop(seen);
                responder.respond(NewSessionResponse::new(SessionId::new("s1")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_cancel: CancelNotification, _cx| {
                on_cancel.lock().unwrap().cancelled = true;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                let text = match request.prompt.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    _ => String::new(),
                };
                let seen = on_prompt.clone();
                if is_steer(&request) {
                    let while_running = {
                        let mut s = seen.lock().unwrap();
                        s.steer = Some(SteerSeen { text, meta: serde_json::to_value(request.meta.as_ref().unwrap()).unwrap(), while_running: s.running });
                        s.running
                    };
                    // fx completes the steer's request together with the active turn.
                    cx.spawn(async move {
                        while while_running && seen.lock().unwrap().running {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    })?;
                    return Ok(());
                }
                seen.lock().unwrap().running = true;
                let session_id = request.session_id.clone();
                cx.spawn({
                    let cx = cx.clone();
                    async move {
                        let sid = session_id;
                        let notify = |cx: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>, update: SessionUpdate| cx.send_notification(SessionNotification::new(sid.clone(), update));
                        let finish = |responder: agent_client_protocol::Responder<PromptResponse>, stop: StopReason| {
                            let result = responder.respond(PromptResponse::new(stop));
                            seen.lock().unwrap().running = false;
                            result
                        };
                        if text == "loop" {
                            let mut n = 0;
                            while !seen.lock().unwrap().cancelled && seen.lock().unwrap().steer.is_none() && n < 200 {
                                n += 1;
                                notify(&cx, SessionUpdate::ToolCall(ToolCall::new(ToolCallId::new(format!("l{n}")), "ls").name("shell").raw_input(serde_json::json!({"cmd": "ls"}))))?;
                                notify(&cx, SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(ToolCallId::new(format!("l{n}")), ToolCallUpdateFields::new().status(ToolCallStatus::Completed).content(vec![ContentBlock::Text(TextContent::new("same")).into()]))))?;
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                            let stop = if seen.lock().unwrap().cancelled { StopReason::Cancelled } else { StopReason::EndTurn };
                            return finish(responder, stop);
                        }
                        notify(&cx, SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new("thinking")))))?;
                        notify(&cx, SessionUpdate::ToolCall(ToolCall::new(ToolCallId::new("c1"), "Render").name("mcp_lease_render").raw_input(serde_json::json!({"prompt": "a cat"}))))?;
                        let permission = RequestPermissionRequest::new(
                            sid.clone(),
                            ToolCallUpdate::new(ToolCallId::new("c1"), ToolCallUpdateFields::new().name("mcp_lease_render").raw_input(serde_json::json!({"prompt": "a cat"}))),
                            vec![PermissionOption::new(PermissionOptionId::new("allow_once"), "Allow", PermissionOptionKind::AllowOnce), PermissionOption::new(PermissionOptionId::new("reject"), "Reject", PermissionOptionKind::RejectOnce)],
                        );
                        let answer = cx.send_request(permission).block_task().await?;
                        let allowed = matches!(answer.outcome, RequestPermissionOutcome::Selected(ref s) if s.option_id.to_string() == "allow_once");
                        notify(&cx, SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(ToolCallId::new("c1"), ToolCallUpdateFields::new().status(if allowed { ToolCallStatus::Completed } else { ToolCallStatus::Failed }).content(vec![ContentBlock::Text(TextContent::new("/out/cat.png")).into()]))))?;
                        notify(&cx, SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new("done")))))?;
                        notify(&cx, SessionUpdate::UsageUpdate(UsageUpdate::new(8060, 131072)))?;
                        finish(responder, StopReason::EndTurn)
                    }
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        );
    DynConnectTo::new(agent)
}

fn harness(seen: Arc<Mutex<Seen>>) -> AcpHarness {
    harness_with(seen, false)
}

fn harness_with(seen: Arc<Mutex<Seen>>, steering: bool) -> AcpHarness {
    let info = HarnessInfo {
        name: "fake".into(),
        version: "1".into(),
    };
    AcpHarness::new(info, Arc::new(move || fake_agent(seen.clone(), steering)))
}

fn spec(dir: &Path) -> SessionSpec {
    SessionSpec {
        label: "001-builder-T001".into(),
        cwd: dir.to_path_buf(),
        persist_dir: None,
        raw_log: Some(dir.join("raw.jsonl")),
        stderr_log: None,
    }
}

use std::path::Path;

async fn drain(
    session: &mut Box<dyn smr_core::ports::Session>,
    answer: PermissionDecision,
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        let Some(event) = tokio::time::timeout(Duration::from_secs(10), session.next_event())
            .await
            .expect("event within 10 s")
        else {
            break;
        };
        if let SessionEvent::PermissionRequest { id, .. } = &event {
            session.answer_permission(id, answer).await.unwrap();
        }
        let settled = matches!(event, SessionEvent::Settled { .. });
        events.push(event);
        if settled {
            break;
        }
    }
    events
}

#[tokio::test]
async fn a_prompt_with_a_tool_call_and_permission_round_trips() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(seen.clone())
        .with_mcp_servers(vec![McpEndpoint {
            name: "lease".into(),
            url: "http://127.0.0.1:1/mcp".into(),
        }])
        .with_session_meta(
            serde_json::json!({"fx": {"profileMcpServers": true}})
                .as_object()
                .unwrap()
                .clone(),
        );
    let mut session = harness.open(spec(dir.path())).await.unwrap();
    assert_eq!(seen.lock().unwrap().mcp_servers, 1);
    assert_eq!(
        seen.lock().unwrap().meta.as_ref().unwrap()["fx"]["profileMcpServers"],
        true
    );
    session.prompt("render a cat").await.unwrap();
    let events = drain(&mut session, PermissionDecision::Allow).await;
    let kinds: Vec<String> = events
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "turn_start",
            "first_token",
            "tool_call_start",
            "permission_request",
            "tool_call_end",
            "message_end",
            "turn_end",
            "settled"
        ]
    );
    assert!(
        matches!(&events[2], SessionEvent::ToolCallStart { name, args, .. } if name == "mcp_lease_render" && args["prompt"] == "a cat")
    );
    assert!(
        matches!(&events[4], SessionEvent::ToolCallEnd { result_text, is_error: false, .. } if result_text == "/out/cat.png")
    );
    assert!(
        matches!(&events[5], SessionEvent::MessageEnd { usage, stop_reason: CoreStop::Stop, thinking_chars: 8, text, .. } if text == "done" && usage.input == 8060)
    );
    assert_eq!(events[7], SessionEvent::Settled { aborted: false });
    let exit = session.close(Duration::from_secs(5)).await;
    assert_eq!(exit.code, Some(0));
    assert!(
        std::fs::read_to_string(dir.path().join("raw.jsonl"))
            .unwrap()
            .lines()
            .count()
            >= 8
    );
}

#[tokio::test]
async fn a_denied_permission_fails_the_tool_call() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(seen).open(spec(dir.path())).await.unwrap();
    session.prompt("render a cat").await.unwrap();
    let events = drain(&mut session, PermissionDecision::Deny).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolCallEnd { is_error: true, .. }))
    );
    session.close(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn abort_cancels_the_running_prompt() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(seen).open(spec(dir.path())).await.unwrap();
    session.prompt("loop").await.unwrap();
    let mut calls = 0;
    let mut settled = None;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), session.next_event())
        .await
        .unwrap()
    {
        match event {
            SessionEvent::ToolCallStart { .. } => {
                calls += 1;
                if calls == 3 {
                    session.abort().await.unwrap();
                }
            }
            SessionEvent::Settled { aborted } => {
                settled = Some(aborted);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(settled, Some(true));
    assert!(calls < 200);
    session.close(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn proxy_observations_become_per_request_turns() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let dir = tempfile::tempdir().unwrap();
    let (feed, _keep) = tokio::sync::broadcast::channel(16);
    let harness = harness(seen).with_observations(feed.clone());
    let mut session = harness.open(spec(dir.path())).await.unwrap();
    session.prompt("render a cat").await.unwrap();
    // The proxy reports two engine requests while the agent works on the prompt.
    feed.send(EngineObservation::Completed {
        usage: Usage {
            input: 100,
            output: 10,
            cache_read: 90,
            ..Default::default()
        },
        stop_reason: CoreStop::ToolUse,
    })
    .unwrap();
    // A reasoning-only response that ended with `stop` and no tool call (seen
    // with fx 0.0.14 and Qwen3.8 on Sushi): the engine's finish reason is kept.
    feed.send(EngineObservation::Completed {
        usage: Usage {
            input: 1818,
            output: 4848,
            cache_read: 14634,
            reasoning: 4848,
            ..Default::default()
        },
        stop_reason: CoreStop::Stop,
    })
    .unwrap();
    feed.send(EngineObservation::Error {
        message: "Maximum context at KV8 and prefill chunk 512: 61488 tokens.".into(),
    })
    .unwrap();
    let events = drain(&mut session, PermissionDecision::Allow).await;
    let ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::MessageEnd { .. }))
        .collect();
    assert_eq!(ends.len(), 3, "{events:?}");
    assert!(
        matches!(ends[0], SessionEvent::MessageEnd { usage, stop_reason: CoreStop::ToolUse, .. } if usage.cache_read == 90)
    );
    assert!(
        matches!(ends[1], SessionEvent::MessageEnd { usage, stop_reason: CoreStop::Stop, .. } if usage.reasoning == 4848 && usage.output == 4848)
    );
    assert!(
        matches!(ends[2], SessionEvent::MessageEnd { stop_reason: CoreStop::Error, error_message: Some(m), .. } if m.contains("61488"))
    );
    assert_eq!(
        events.last(),
        Some(&SessionEvent::Settled { aborted: false })
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::TurnEnd))
            .count(),
        3
    );
    session.close(Duration::from_secs(5)).await;
}

/// Runs the `loop` prompt, steers at the third tool call and, with `abort`,
/// aborts at the sixth; returns the tool-call count and every `Settled`.
async fn loop_with_steer(
    session: &mut Box<dyn smr_core::ports::Session>,
    abort: bool,
) -> (usize, Vec<bool>) {
    session.prompt("loop").await.unwrap();
    let mut calls = 0;
    let mut settled = Vec::new();
    loop {
        let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_secs(10), session.next_event()).await
        else {
            break;
        };
        match event {
            SessionEvent::ToolCallStart { .. } => {
                calls += 1;
                if calls == 3 {
                    session.steer("focus on the tests").await.unwrap();
                }
                if abort && calls == 6 {
                    session.abort().await.unwrap();
                }
            }
            SessionEvent::Settled { aborted } => {
                settled.push(aborted);
                // A second `Settled` would be a bug: give it a moment to show.
                while let Ok(Some(event)) =
                    tokio::time::timeout(Duration::from_millis(200), session.next_event()).await
                {
                    if let SessionEvent::Settled { aborted } = event {
                        settled.push(aborted);
                    }
                }
                break;
            }
            _ => {}
        }
    }
    (calls, settled)
}

#[tokio::test]
async fn a_steer_is_a_prompt_with_the_fx_meta_while_the_turn_runs() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness_with(seen.clone(), true)
        .open(spec(dir.path()))
        .await
        .unwrap();
    let (calls, settled) = loop_with_steer(&mut session, false).await;
    let steer = seen
        .lock()
        .unwrap()
        .steer
        .clone()
        .expect("the agent got the steer");
    assert_eq!(steer.text, "focus on the tests");
    assert_eq!(steer.meta, serde_json::json!({"fx": {"steer": true}}));
    assert!(
        steer.while_running,
        "the steer arrived while the first prompt ran"
    );
    assert!(
        (3..200).contains(&calls),
        "the agent stopped looping on the steer: {calls}"
    );
    assert_eq!(
        settled,
        vec![false],
        "exactly one Settled, from the first prompt"
    );
    assert!(!seen.lock().unwrap().cancelled);
    assert_eq!(session.close(Duration::from_secs(5)).await.code, Some(0));
}

#[tokio::test]
async fn a_steer_is_dropped_when_the_agent_does_not_advertise_it() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(seen.clone()).open(spec(dir.path())).await.unwrap();
    let (calls, settled) = loop_with_steer(&mut session, true).await;
    assert!(
        seen.lock().unwrap().steer.is_none(),
        "no steer prompt was sent"
    );
    assert!(seen.lock().unwrap().cancelled);
    assert!((6..200).contains(&calls));
    assert_eq!(settled, vec![true]);
    session.close(Duration::from_secs(5)).await;
}
