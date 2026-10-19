//! One ACP session: the connection runs in its own task; commands go in,
//! session events come out.

use std::collections::HashMap;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, FileSystemCapabilities, Implementation,
    InitializeRequest, InitializeResponse, McpServer, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{Agent, Client, ConnectionTo, DynConnectTo, Responder, SessionMessage};
use async_trait::async_trait;
use tenaz_core::ports::{HarnessError, PermissionDecision, Session, SessionExit, SessionSpec};
use tenaz_core::session::{SessionEvent, StopReason, Usage};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::translate::{Translator, choose_option};

/// What the engine proxy saw: one request's usage and why the engine
/// stopped, or an engine error.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineObservation {
    Completed {
        usage: Usage,
        stop_reason: StopReason,
        /// The engine's own figures for the request, when it reported them.
        server: Option<tenaz_core::session::ServerTimings>,
    },
    Error {
        message: String,
    },
}

/// The runner's stop reason for an OpenAI-style `finish_reason`. Without
/// one, the agent is assumed to carry on (a tool call), as fx does.
pub fn stop_reason_for_finish(finish_reason: Option<&str>) -> StopReason {
    match finish_reason {
        None | Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("stop") => StopReason::Stop,
        Some("length") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_owned()),
    }
}

/// Fan-out of proxy observations to the open sessions.
pub type ObservationFeed = broadcast::Sender<EngineObservation>;

enum Command {
    Prompt(String),
    Steer(String),
    Abort,
    Permission { id: String, allow: bool },
    Close(oneshot::Sender<()>),
}

pub(crate) struct AcpSession {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    raw_log: Option<std::path::PathBuf>,
    stderr_log: Option<std::path::PathBuf>,
    task: Option<tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>>,
}

impl AcpSession {
    pub(crate) async fn open(
        transport: DynConnectTo<Client>,
        spec: SessionSpec,
        mcp_servers: Vec<McpServer>,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
        observations: Option<broadcast::Receiver<EngineObservation>>,
    ) -> Result<Self, HarnessError> {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        let cwd = spec.cwd.clone();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready_tx);
            Client
                .builder()
                .name("tenaz")
                .connect_with(transport, async |cx| {
                    let capabilities = ClientCapabilities::new().fs(FileSystemCapabilities::new().read_text_file(false).write_text_file(false)).terminal(false);
                    let init = InitializeRequest::new(ProtocolVersion::V1).client_capabilities(capabilities).client_info(Implementation::new("tenaz", env!("CARGO_PKG_VERSION")));
                    let steering = match cx.send_request(init).block_task().await {
                        Ok(response) => advertises_steering(&response),
                        Err(error) => {
                            if let Some(ready) = ready.take() {
                                let _ = ready.send(Err(format!("initialize: {error}")));
                            }
                            return Err(error);
                        }
                    };
                    let mut request = NewSessionRequest::new(cwd.clone()).mcp_servers(mcp_servers);
                    if let Some(meta) = meta {
                        request = request.meta(meta);
                    }
                    let mut session = match cx.build_session_from(request).block_task().start_session().await {
                        Ok(session) => session,
                        Err(error) => {
                            if let Some(ready) = ready.take() {
                                let _ = ready.send(Err(format!("session/new: {error}")));
                            }
                            return Err(error);
                        }
                    };
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Ok(()));
                    }
                    let mut state = Running { translator: Translator::new(), pending: HashMap::new(), request_seen: false, prompting: false, steering, events: event_tx };
                    let mut commands = command_rx;
                    let mut observations = observations;
                    loop {
                        tokio::select! {
                            command = commands.recv() => match command {
                                None | Some(Command::Close(_)) => {
                                    if let Some(Command::Close(done)) = command {
                                        let _ = done.send(());
                                    }
                                    return Ok(());
                                }
                                Some(Command::Prompt(text)) => {
                                    state.request_seen = false;
                                    state.emit(SessionEvent::TurnStart);
                                    match session.send_prompt(text) {
                                        Ok(()) => state.prompting = true,
                                        Err(error) => {
                                            state.emit(SessionEvent::PromptRejected { reason: error.to_string() });
                                            state.emit(SessionEvent::Settled { aborted: true });
                                        }
                                    }
                                }
                                Some(Command::Steer(text)) => {
                                    if !state.steering {
                                        tracing::debug!("steer dropped: the agent does not advertise fx steering");
                                    } else if !state.prompting {
                                        tracing::debug!("steer dropped: no prompt in flight");
                                    } else if let Err(error) = send_steer(&cx, session.session_id().clone(), text) {
                                        tracing::warn!(%error, "steer prompt could not be sent");
                                    }
                                }
                                Some(Command::Abort) => {
                                    let _ = cx.send_notification(CancelNotification::new(session.session_id().clone()));
                                }
                                Some(Command::Permission { id, allow }) => state.answer(&id, allow),
                            },
                            observation = recv_observation(&mut observations) => match observation {
                                EngineObservation::Completed { usage, stop_reason, server } => {
                                    state.request_seen = true;
                                    for event in state.translator.request_end(usage, stop_reason, None, server) {
                                        state.emit(event);
                                    }
                                    state.emit(SessionEvent::TurnStart);
                                }
                                EngineObservation::Error { message } => {
                                    state.request_seen = true;
                                    for event in state.translator.request_end(Usage::default(), StopReason::Error, Some(message), None) {
                                        state.emit(event);
                                    }
                                    state.emit(SessionEvent::TurnStart);
                                }
                            },
                            message = session.read_update() => match message {
                                Err(error) => return Err(error),
                                Ok(SessionMessage::StopReason(stop)) => {
                                    state.prompting = false;
                                    for event in state.translator.prompt_end(stop, state.request_seen) {
                                        state.emit(event);
                                    }
                                }
                                Ok(SessionMessage::SessionMessage(dispatch)) => {
                                    let state_ref = &mut state;
                                    MatchDispatch::new(dispatch)
                                        .if_notification(async |notification: SessionNotification| {
                                            for event in state_ref.translator.update(&notification.update) {
                                                state_ref.emit(event);
                                            }
                                            Ok(())
                                        })
                                        .await
                                        .if_request(async |request: RequestPermissionRequest, responder: Responder<RequestPermissionResponse>| {
                                            let id = request.tool_call.tool_call_id.to_string();
                                            state_ref.pending.insert(id, (responder, request.options.clone()));
                                            state_ref.emit(Translator::permission(&request));
                                            Ok(())
                                        })
                                        .await
                                        .otherwise_ignore()?;
                                }
                                _ => {}
                            },
                        }
                    }
                })
                .await
        });
        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                commands,
                events,
                raw_log: spec.raw_log,
                stderr_log: spec.stderr_log,
                task: Some(task),
            }),
            Ok(Err(message)) => Err(HarnessError::Spawn(message)),
            Err(_) => {
                let error = match task.await {
                    Ok(Err(error)) => error.to_string(),
                    Ok(Ok(())) => {
                        "the agent connection ended before the session started".to_owned()
                    }
                    Err(join) => join.to_string(),
                };
                Err(HarnessError::Spawn(error))
            }
        }
    }
}

/// Whether the agent advertised fx's steering extension
/// (`agentCapabilities._meta.fx.steering`, `true` in fx 0.0.14).
fn advertises_steering(response: &InitializeResponse) -> bool {
    response
        .agent_capabilities
        .meta
        .as_ref()
        .and_then(|meta| meta.get("fx"))
        .and_then(|fx| fx.get("steering"))
        .is_some_and(|steering| steering.as_bool().unwrap_or(!steering.is_null()))
}

/// The `_meta` that marks a `session/prompt` as a steer for fx.
fn steer_meta() -> serde_json::Map<String, serde_json::Value> {
    let mut meta = serde_json::Map::new();
    meta.insert("fx".to_owned(), serde_json::json!({ "steer": true }));
    meta
}

/// fx's steer: a text `session/prompt` with `_meta.fx.steer: true`, applied
/// at the next safe model boundary; its response completes with the active
/// turn. It goes around `ActiveSession::send_prompt` so that response is
/// only logged and never becomes a second `Settled` for the turn.
fn send_steer(
    cx: &ConnectionTo<Agent>,
    session_id: SessionId,
    text: String,
) -> Result<(), agent_client_protocol::Error> {
    let request = PromptRequest::new(session_id, vec![text.into()]).meta(steer_meta());
    cx.send_request(request)
        .on_receiving_result(async |result| {
            match result {
                Ok(response) => {
                    tracing::debug!(stop_reason = ?response.stop_reason, "steer prompt completed");
                }
                Err(error) => tracing::warn!(%error, "steer prompt failed"),
            }
            Ok(())
        })
}

/// Per-connection state inside the task.
struct Running {
    translator: Translator,
    pending: HashMap<
        String,
        (
            Responder<RequestPermissionResponse>,
            Vec<agent_client_protocol::schema::v1::PermissionOption>,
        ),
    >,
    request_seen: bool,
    /// A prompt has been sent and its response has not arrived.
    prompting: bool,
    /// The agent accepts fx steer prompts.
    steering: bool,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl Running {
    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }

    fn answer(&mut self, id: &str, allow: bool) {
        let Some((responder, options)) = self.pending.remove(id) else {
            return;
        };
        let outcome = match choose_option(&options, allow) {
            Some(option) => {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option))
            }
            None => RequestPermissionOutcome::Cancelled,
        };
        let _ = responder.respond(RequestPermissionResponse::new(outcome));
    }
}

async fn recv_observation(
    feed: &mut Option<broadcast::Receiver<EngineObservation>>,
) -> EngineObservation {
    match feed {
        Some(receiver) => loop {
            match receiver.recv().await {
                Ok(observation) => return observation,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            }
        },
        None => std::future::pending().await,
    }
}

#[async_trait]
impl Session for AcpSession {
    async fn prompt(&mut self, text: &str) -> Result<(), HarnessError> {
        self.commands
            .send(Command::Prompt(text.to_owned()))
            .map_err(|_| HarnessError::Closed)
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        let event = self.events.recv().await?;
        if let Some(log) = &self.raw_log
            && let Ok(line) = serde_json::to_string(&event)
        {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .and_then(|mut f| {
                    std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes())
                });
        }
        Some(event)
    }

    /// ACP itself has no steering message. With fx's extension (advertised
    /// at initialize) the steer is a `session/prompt` carrying
    /// `_meta.fx.steer: true`, sent only while a prompt is in flight;
    /// otherwise it is a no-op and the abort path handles the rest.
    async fn steer(&mut self, text: &str) -> Result<(), HarnessError> {
        self.commands
            .send(Command::Steer(text.to_owned()))
            .map_err(|_| HarnessError::Closed)
    }

    async fn abort(&mut self) -> Result<(), HarnessError> {
        self.commands
            .send(Command::Abort)
            .map_err(|_| HarnessError::Closed)
    }

    async fn answer_permission(
        &mut self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        self.commands
            .send(Command::Permission {
                id: request_id.to_owned(),
                allow: decision == PermissionDecision::Allow,
            })
            .map_err(|_| HarnessError::Closed)
    }

    async fn close(&mut self, grace: Duration) -> SessionExit {
        let (done, finished) = oneshot::channel();
        let _ = self.commands.send(Command::Close(done));
        let _ = tokio::time::timeout(grace.max(Duration::from_millis(50)), finished).await;
        let Some(task) = self.task.take() else {
            return SessionExit { code: None };
        };
        match tokio::time::timeout(grace.max(Duration::from_millis(50)), task).await {
            Ok(Ok(Ok(()))) => SessionExit { code: Some(0) },
            Ok(Ok(Err(error))) => {
                // The connection ended with an error: usually the agent process
                // exited, and the error carries its status and stderr tail.
                if let Some(log) = &self.stderr_log {
                    crate::append_line(
                        log,
                        &format!("[tenaz] the agent connection ended: {error}"),
                    );
                }
                SessionExit { code: Some(1) }
            }
            Ok(Err(_)) => SessionExit { code: Some(1) },
            Err(_) => SessionExit { code: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::AgentCapabilities;

    fn initialize_with(meta: Option<serde_json::Value>) -> InitializeResponse {
        let mut capabilities = AgentCapabilities::new();
        if let Some(meta) = meta {
            capabilities = capabilities.meta(meta.as_object().cloned());
        }
        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(capabilities)
    }

    #[test]
    fn steering_is_read_from_the_fx_capability_meta() {
        assert!(!advertises_steering(&initialize_with(None)));
        assert!(!advertises_steering(&initialize_with(Some(
            serde_json::json!({"fx": {"steering": false}})
        ))));
        assert!(!advertises_steering(&initialize_with(Some(
            serde_json::json!({"fx": {"other": true}})
        ))));
        assert!(advertises_steering(&initialize_with(Some(
            serde_json::json!({"fx": {"steering": true}})
        ))));
        assert_eq!(
            serde_json::Value::Object(steer_meta()),
            serde_json::json!({"fx": {"steer": true}})
        );
    }

    #[test]
    fn finish_reasons_map_to_the_runner_stop_reasons() {
        assert_eq!(stop_reason_for_finish(Some("stop")), StopReason::Stop);
        assert_eq!(
            stop_reason_for_finish(Some("tool_calls")),
            StopReason::ToolUse
        );
        assert_eq!(stop_reason_for_finish(None), StopReason::ToolUse);
        assert_eq!(
            stop_reason_for_finish(Some("length")),
            StopReason::MaxTokens
        );
        assert_eq!(
            stop_reason_for_finish(Some("content_filter")),
            StopReason::Other("content_filter".into())
        );
    }
}
