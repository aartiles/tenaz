//! One ACP session: the connection runs in its own task; commands go in,
//! session events come out.

use std::collections::HashMap;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, FileSystemCapabilities, Implementation,
    InitializeRequest, McpServer, NewSessionRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{Client, DynConnectTo, Responder, SessionMessage};
use async_trait::async_trait;
use smr_core::ports::{HarnessError, PermissionDecision, Session, SessionExit, SessionSpec};
use smr_core::session::{SessionEvent, StopReason, Usage};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::translate::{Translator, choose_option};

/// What the engine proxy saw: one request's usage, or an engine error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineObservation {
    Completed { usage: Usage },
    Error { message: String },
}

/// Fan-out of proxy observations to the open sessions.
pub type ObservationFeed = broadcast::Sender<EngineObservation>;

enum Command {
    Prompt(String),
    Abort,
    Permission { id: String, allow: bool },
    Close(oneshot::Sender<()>),
}

pub(crate) struct AcpSession {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    raw_log: Option<std::path::PathBuf>,
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
                .name("smr")
                .connect_with(transport, async |cx| {
                    let capabilities = ClientCapabilities::new().fs(FileSystemCapabilities::new().read_text_file(false).write_text_file(false)).terminal(false);
                    let init = InitializeRequest::new(ProtocolVersion::V1).client_capabilities(capabilities).client_info(Implementation::new("smr", env!("CARGO_PKG_VERSION")));
                    if let Err(error) = cx.send_request(init).block_task().await {
                        if let Some(ready) = ready.take() {
                            let _ = ready.send(Err(format!("initialize: {error}")));
                        }
                        return Err(error);
                    }
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
                    let mut state = Running { translator: Translator::new(), pending: HashMap::new(), request_seen: false, events: event_tx };
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
                                    if let Err(error) = session.send_prompt(text) {
                                        state.emit(SessionEvent::PromptRejected { reason: error.to_string() });
                                        state.emit(SessionEvent::Settled { aborted: true });
                                    }
                                }
                                Some(Command::Abort) => {
                                    let _ = cx.send_notification(CancelNotification::new(session.session_id().clone()));
                                }
                                Some(Command::Permission { id, allow }) => state.answer(&id, allow),
                            },
                            observation = recv_observation(&mut observations) => match observation {
                                EngineObservation::Completed { usage } => {
                                    state.request_seen = true;
                                    for event in state.translator.request_end(usage, StopReason::ToolUse, None) {
                                        state.emit(event);
                                    }
                                    state.emit(SessionEvent::TurnStart);
                                }
                                EngineObservation::Error { message } => {
                                    state.request_seen = true;
                                    for event in state.translator.request_end(Usage::default(), StopReason::Error, Some(message)) {
                                        state.emit(event);
                                    }
                                    state.emit(SessionEvent::TurnStart);
                                }
                            },
                            message = session.read_update() => match message {
                                Err(error) => return Err(error),
                                Ok(SessionMessage::StopReason(stop)) => {
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

    /// ACP has no steering message; the steer is recorded by the runner and
    /// the abort path handles the rest.
    async fn steer(&mut self, _text: &str) -> Result<(), HarnessError> {
        Ok(())
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
            Ok(_) => SessionExit { code: Some(1) },
            Err(_) => SessionExit { code: None },
        }
    }
}
