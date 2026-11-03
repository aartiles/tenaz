//! The ACP harness adapter: runs an ACP agent (fx, `OpenCode` or Qwen Code)
//! as a subprocess and turns its session updates into the runner's session
//! events. Token usage per request comes from the engine proxy when one is
//! wired in, because ACP reports usage only once per turn.

pub mod fx;
pub mod opencode;
pub mod qwen;
mod session;
mod translate;

use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{HttpHeader, McpServer, McpServerHttp};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Client, DynConnectTo, LineDirection};
use async_trait::async_trait;
use tenaz_core::ports::{Harness, HarnessError, HarnessInfo, Session, SessionModel, SessionSpec};

pub use session::{EngineObservation, ObservationFeed, stop_reason_for_finish};
pub use translate::Translator;

/// Builds a fresh transport to the agent for every session.
pub type Connector = Arc<dyn Fn(&SessionSpec) -> DynConnectTo<Client> + Send + Sync>;

/// The command-line arguments of one session's agent process.
pub type ArgsFor = Arc<dyn Fn(&SessionSpec) -> Vec<String> + Send + Sync>;

/// An MCP server the runner hosts and offers to every session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpEndpoint {
    pub name: String,
    pub url: String,
}

/// The harness. It runs the model its agent is set up with, or the one its
/// per-session arguments name (Qwen Code); an agent that reads the model
/// from its configuration is wrapped in a [`ConfiguredHarness`].
#[derive(Clone)]
pub struct AcpHarness {
    info: HarnessInfo,
    connector: Connector,
    mcp_servers: Vec<McpEndpoint>,
    session_meta: Option<serde_json::Map<String, serde_json::Value>>,
    observations: Option<ObservationFeed>,
}

impl std::fmt::Debug for AcpHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpHarness")
            .field("info", &self.info)
            .field("mcp_servers", &self.mcp_servers)
            .finish_non_exhaustive()
    }
}

impl AcpHarness {
    /// A harness over any ACP transport factory.
    pub fn new(info: HarnessInfo, connector: Connector) -> Self {
        Self {
            info,
            connector,
            mcp_servers: Vec::new(),
            session_meta: None,
            observations: None,
        }
    }

    /// A harness that spawns `command args...` with `env` for every session.
    pub fn subprocess(
        info: HarnessInfo,
        command: impl Into<std::path::PathBuf>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self::subprocess_with(info, command, Arc::new(move |_| args.clone()), env)
    }

    /// A harness that spawns `command` with per-session arguments. The
    /// agent's stderr is appended to the session's stderr log, so a crash
    /// leaves its diagnostics behind.
    pub fn subprocess_with(
        info: HarnessInfo,
        command: impl Into<std::path::PathBuf>,
        args_for: ArgsFor,
        env: Vec<(String, String)>,
    ) -> Self {
        let command = command.into();
        let connector: Connector = Arc::new(move |spec: &SessionSpec| {
            let config = AcpAgentConfig::new(command.clone())
                .args(args_for(spec))
                .envs(env.clone());
            let agent = AcpAgent::new(config);
            let agent = match spec.stderr_log.clone() {
                Some(log) => agent.with_debug(move |line, direction| {
                    if direction == LineDirection::Stderr {
                        append_line(&log, line);
                    }
                }),
                None => agent,
            };
            DynConnectTo::new(agent)
        });
        Self::new(info, connector)
    }

    /// MCP servers passed in `session/new`.
    #[must_use]
    pub fn with_mcp_servers(mut self, servers: Vec<McpEndpoint>) -> Self {
        self.mcp_servers = servers;
        self
    }

    /// Extra `_meta` for `session/new` (for fx: `{"fx": {"profileMcpServers": true}}`).
    #[must_use]
    pub fn with_session_meta(mut self, meta: serde_json::Map<String, serde_json::Value>) -> Self {
        self.session_meta = Some(meta);
        self
    }

    /// Per-request usage and engine errors observed by the proxy. With a
    /// feed, every engine request becomes a turn of the session; without
    /// it, a whole prompt is one turn with the agent's own usage figure.
    #[must_use]
    pub fn with_observations(mut self, feed: ObservationFeed) -> Self {
        self.observations = Some(feed);
        self
    }

    fn mcp_servers(&self) -> Vec<McpServer> {
        self.mcp_servers
            .iter()
            .map(|s| {
                McpServer::Http(
                    McpServerHttp::new(s.name.clone(), s.url.clone())
                        .headers(Vec::<HttpHeader>::new()),
                )
            })
            .collect()
    }
}

/// An MCP server written into an agent's own configuration, which accepts
/// a time limit as long as a media job's; a server passed in `session/new`
/// gets the agent's default limit.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileMcpServer {
    pub name: String,
    pub url: String,
    pub operation_timeout_ms: u64,
}

/// Writes an agent's configuration for one session's model.
pub type WriteConfig = Arc<dyn Fn(&SessionModel) -> io::Result<()> + Send + Sync>;

/// An ACP agent that reads its configuration when it starts, with that
/// configuration written for each session's model first. Sessions run one
/// after another, so one configuration holds the model of the session that
/// is starting.
pub struct ConfiguredHarness {
    acp: AcpHarness,
    /// What `write` writes, for its errors: `the OpenCode configuration`.
    what: &'static str,
    write: WriteConfig,
}

impl std::fmt::Debug for ConfiguredHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfiguredHarness")
            .field("acp", &self.acp)
            .field("what", &self.what)
            .finish_non_exhaustive()
    }
}

impl ConfiguredHarness {
    /// `acp` with `write` run before each session, and once now for the
    /// run's model `first`, so a configuration that cannot be written
    /// stops the run before its first session. `what` names it in errors.
    pub fn new(
        acp: AcpHarness,
        what: &'static str,
        write: WriteConfig,
        first: &SessionModel,
    ) -> io::Result<Self> {
        write(first).map_err(|e| io::Error::new(e.kind(), cannot_write(what, &e)))?;
        Ok(Self { acp, what, write })
    }
}

/// The error of a configuration that could not be written.
fn cannot_write(what: &str, error: &io::Error) -> String {
    format!("cannot write {what}: {error}")
}

#[async_trait]
impl Harness for ConfiguredHarness {
    fn info(&self) -> HarnessInfo {
        self.acp.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        (self.write)(&spec.model).map_err(|e| HarnessError::Spawn(cannot_write(self.what, &e)))?;
        self.acp.open(spec).await
    }
}

/// The first line of `<binary> --version`, trimmed: how every agent Tenaz
/// drives, Pi included, reports its version.
pub fn agent_version(binary: &Path) -> io::Result<String> {
    let output = Command::new(binary).arg("--version").output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{} --version exited with {}",
            binary.display(),
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned())
}

/// Writes `value` as pretty JSON to `path`, creating its folder.
pub(crate) fn write_json(path: &Path, value: &serde_json::Value) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)?)
}

/// Appends one line to a log file, creating it; errors are ignored (a lost
/// diagnostic line must never stop a session).
pub(crate) fn append_line(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn info(&self) -> HarnessInfo {
        self.info.clone()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        let transport = (self.connector)(&spec);
        let session = session::AcpSession::open(
            transport,
            spec,
            self.mcp_servers(),
            self.session_meta.clone(),
            self.observations
                .as_ref()
                .map(tokio::sync::broadcast::Sender::subscribe),
        )
        .await?;
        Ok(Box::new(session))
    }
}

#[cfg(test)]
mod tests {
    use tenaz_adapters::testing::write_executable;

    use super::*;

    #[test]
    fn the_version_is_the_first_line_of_the_agent_s_answer_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        write_executable(&agent, "#!/bin/sh\nprintf '  1.2.3\\nextra\\n'\n");
        assert_eq!(agent_version(&agent).unwrap(), "1.2.3");
    }

    #[test]
    fn an_agent_that_fails_has_no_version() {
        assert!(agent_version(Path::new("false")).is_err());
    }

    #[test]
    fn a_missing_agent_has_no_version() {
        assert!(agent_version(Path::new("/nonexistent/agent")).is_err());
    }
}
