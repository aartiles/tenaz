//! Any ACP agent Tenaz has no profile for (`--harness acp`, ADR 0027): the
//! user's command, told about each session in a file it can read, so a
//! small script can configure the agent before starting it.

use std::io;
use std::path::{Path, PathBuf};

use tenaz_core::ledger::HarnessKind;
use tenaz_core::ports::{SessionModel, SessionSpec};

use crate::agent::AgentProfile;
use crate::{McpEndpoint, write_json};

/// The environment variable that names the session file.
pub const SESSION_FILE_VAR: &str = "TENAZ_AGENT_SESSION";

/// What the session file says, besides the session's model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFacts {
    /// The proxy's base URL (`http://127.0.0.1:<port>/v1`).
    pub base_url: String,
    pub max_output_tokens: u64,
    /// The reasoning effort of the run's thinking level, when it has one.
    pub reasoning_effort: Option<String>,
    /// The run's MCP servers (the media tools).
    pub mcp_servers: Vec<McpEndpoint>,
}

impl SessionFacts {
    /// The session file for a session on `model`.
    pub fn to_json(&self, model: &SessionModel) -> serde_json::Value {
        let mcp: Vec<serde_json::Value> = self
            .mcp_servers
            .iter()
            .map(|s| serde_json::json!({ "name": s.name, "url": s.url }))
            .collect();
        serde_json::json!({
            "baseUrl": self.base_url,
            "model": model.id,
            "contextWindow": model.context_window,
            "maxOutputTokens": self.max_output_tokens,
            "supportsImages": model.supports_images,
            "reasoningEffort": self.reasoning_effort,
            "mcpServers": mcp
        })
    }
}

/// The user's ACP command as an [`AgentProfile`]. It keeps the user's own
/// `HOME`: what it reads and keeps between sessions is up to the command.
#[derive(Clone, Debug)]
pub struct GenericAgent {
    pub binary: PathBuf,
    /// Arguments that put it in ACP mode.
    pub args: Vec<String>,
    /// Where the session file is written.
    pub session_file: PathBuf,
    pub facts: SessionFacts,
    /// A program and leading arguments it is started through: the sandbox
    /// launcher, when the run is sandboxed.
    pub wrapper: Vec<String>,
}

impl AgentProfile for GenericAgent {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Acp
    }

    fn config_name(&self) -> &'static str {
        "the agent's session file"
    }

    fn program(&self) -> &Path {
        &self.binary
    }

    fn args(&self, _spec: &SessionSpec) -> Vec<String> {
        self.args.clone()
    }

    fn env(&self) -> Vec<(String, String)> {
        let file = std::path::absolute(&self.session_file).unwrap_or(self.session_file.clone());
        vec![(SESSION_FILE_VAR.to_owned(), file.display().to_string())]
    }

    fn wrapper(&self) -> &[String] {
        &self.wrapper
    }

    fn write_config(&self, model: &SessionModel) -> io::Result<()> {
        write_json(&self.session_file, &self.facts.to_json(model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(dir: &Path) -> GenericAgent {
        GenericAgent {
            binary: "/opt/agent".into(),
            args: vec!["--acp".into()],
            session_file: dir.join("session.json"),
            facts: SessionFacts {
                base_url: "http://127.0.0.1:4001/v1".into(),
                max_output_tokens: 16_384,
                reasoning_effort: Some("medium".into()),
                mcp_servers: vec![McpEndpoint {
                    name: "media".into(),
                    url: "http://127.0.0.1:4002/mcp".into(),
                }],
            },
            wrapper: Vec::new(),
        }
    }

    #[test]
    fn each_session_s_file_names_its_model_and_the_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path());
        let critic = SessionModel {
            name: "engine/critic".into(),
            id: "critic".into(),
            context_window: 32_768,
            supports_images: true,
        };
        agent.write_config(&critic).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&agent.session_file).unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "baseUrl": "http://127.0.0.1:4001/v1",
                "model": "critic",
                "contextWindow": 32_768,
                "maxOutputTokens": 16_384,
                "supportsImages": true,
                "reasoningEffort": "medium",
                "mcpServers": [{ "name": "media", "url": "http://127.0.0.1:4002/mcp" }]
            })
        );
    }

    #[test]
    fn the_command_starts_with_its_arguments_told_only_where_the_session_file_is() {
        let agent = agent(Path::new("/run/private/acp"));
        let spec = SessionSpec {
            label: "001-bootstrap".into(),
            cwd: "/work".into(),
            persist_dir: None,
            raw_log: None,
            thinking_log: None,
            stderr_log: None,
            model: SessionModel {
                name: "p/m".into(),
                id: "m".into(),
                context_window: 65536,
                supports_images: false,
            },
        };
        assert_eq!(agent.args(&spec), ["--acp"]);
        assert_eq!(
            agent.env(),
            [(
                SESSION_FILE_VAR.to_owned(),
                "/run/private/acp/session.json".to_owned()
            )]
        );
        assert_eq!(agent.program(), Path::new("/opt/agent"));
    }
}
