//! Qwen Code specifics: its settings in an isolated `HOME` and how
//! `qwen --acp` is launched (ADR 0019).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tenaz_core::ledger::HarnessKind;
use tenaz_core::ports::{SessionModel, SessionSpec};

use crate::agent::AgentProfile;
use crate::{ProfileMcpServer, write_json};

/// The opening of Qwen Code's request to summarize its conversation
/// (`getCompressionPrompt` in 0.26.6). It cannot be turned off: the engine
/// proxy refuses the request and Tenaz ends the session there (ADR 0022).
pub const COMPACTION_PROMPT: &str = "You are the component that summarizes a conversation when its context window is about to overflow.";

/// What Tenaz writes into `~/.qwen/settings.json`. The model and the engine
/// are given on the command line; the settings hold the model's window and
/// turn off what would carry from one session to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QwenSettings {
    pub context_window: u64,
    pub mcp: Vec<ProfileMcpServer>,
    /// Let Qwen Code load the skills of the project, of its extensions and
    /// its own; off, it loads only those in `~/.qwen/skills`, where the
    /// pinned ones are linked.
    pub discover_skills: bool,
}

impl QwenSettings {
    pub fn to_json(&self) -> serde_json::Value {
        let mcp: serde_json::Map<String, serde_json::Value> = self
            .mcp
            .iter()
            .map(|s| {
                let server = serde_json::json!({
                    "httpUrl": s.url,
                    "timeout": s.operation_timeout_ms
                });
                (s.name.clone(), server)
            })
            .collect();
        let mut json = serde_json::json!({
            "general": { "enableAutoUpdate": false, "chatRecording": false },
            "privacy": { "usageStatisticsEnabled": false },
            "memory": { "enableManagedAutoMemory": false, "enableManagedAutoDream": false },
            "model": { "generationConfig": { "contextWindowSize": self.context_window } },
            "mcpServers": mcp
        });
        if !self.discover_skills {
            json["skills"] =
                serde_json::json!({ "disabledLevels": ["project", "extension", "bundled"] });
        }
        json
    }

    /// These settings for `model`'s window. They do not say whether the
    /// model reads images (see "Known gaps" in docs/COMPATIBILITY.md).
    #[must_use]
    pub fn for_model(&self, model: &SessionModel) -> Self {
        Self {
            context_window: model.context_window,
            ..self.clone()
        }
    }
}

/// How Qwen Code is launched for ACP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QwenLaunch {
    pub binary: PathBuf,
    /// The isolated `HOME` holding `.qwen/settings.json`.
    pub home: PathBuf,
    /// The proxy's base URL (`http://127.0.0.1:<port>/v1`).
    pub base_url: String,
    /// A program and leading arguments Qwen Code is started through: the
    /// sandbox launcher, when the run is sandboxed.
    pub wrapper: Vec<String>,
}

impl QwenLaunch {
    pub fn new(
        binary: impl Into<PathBuf>,
        home: impl Into<PathBuf>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            binary: binary.into(),
            home: home.into(),
            base_url: base_url.into(),
            wrapper: Vec::new(),
        }
    }

    /// The command line for a session on `model`: ACP mode, an
    /// OpenAI-compatible engine that needs no key, and the `default`
    /// approval mode, which asks the client before edits and commands.
    pub fn command_args(&self, model: &str) -> Vec<String> {
        [
            "--acp",
            "--auth-type",
            "openai",
            "--openai-base-url",
            &self.base_url,
            "--openai-api-key",
            "none",
            "--model",
            model,
            "--approval-mode",
            "default",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    /// The environment Qwen Code starts with: `HOME` at `home`, where it
    /// finds the settings Tenaz writes.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![("HOME".to_owned(), self.home.display().to_string())]
    }
}

/// Qwen Code as an [`AgentProfile`]: each session on its own model, named on
/// the command line, with its home prepared for that model.
#[derive(Clone, Debug)]
pub struct QwenAgent {
    pub launch: QwenLaunch,
    pub settings: QwenSettings,
}

impl AgentProfile for QwenAgent {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Qwen
    }

    fn config_name(&self) -> &'static str {
        "the Qwen Code settings"
    }

    fn program(&self) -> &Path {
        &self.launch.binary
    }

    /// The id the engine lists for the session's model, not the provider's
    /// name for it.
    fn args(&self, spec: &SessionSpec) -> Vec<String> {
        self.launch.command_args(&spec.model.id)
    }

    fn env(&self) -> Vec<(String, String)> {
        self.launch.env()
    }

    fn wrapper(&self) -> &[String] {
        &self.launch.wrapper
    }

    fn write_config(&self, model: &SessionModel) -> io::Result<()> {
        prepare_home(&self.launch.home, &self.settings.for_model(model))
    }
}

/// The context files Qwen Code 0.26.6 reads from `<home>/.qwen/` into
/// every session; the model can write them with its edit tools.
const CONTEXT_FILES: [&str; 2] = ["QWEN.md", "AGENTS.md"];

/// Writes `settings.json` under `<home>/.qwen/` and removes the context
/// files there, so a session starts from the plan and the repository.
fn prepare_home(home: &Path, settings: &QwenSettings) -> io::Result<()> {
    for name in CONTEXT_FILES {
        match fs::remove_file(home.join(".qwen").join(name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    write_json(&home.join(".qwen/settings.json"), &settings.to_json())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> QwenSettings {
        QwenSettings {
            context_window: 131_072,
            mcp: vec![ProfileMcpServer {
                name: "media".into(),
                url: "http://127.0.0.1:4002/mcp".into(),
                operation_timeout_ms: 1_800_000,
            }],
            discover_skills: false,
        }
    }

    #[test]
    fn nothing_is_kept_for_the_next_session() {
        let json = settings().to_json();
        assert_eq!(json["memory"]["enableManagedAutoMemory"], false);
        assert_eq!(json["memory"]["enableManagedAutoDream"], false);
        assert_eq!(json["general"]["chatRecording"], false);
        assert_eq!(json["general"]["enableAutoUpdate"], false);
        assert_eq!(json["privacy"]["usageStatisticsEnabled"], false);
    }

    #[test]
    fn what_an_earlier_session_left_for_the_next_to_read_is_removed() {
        let home = tempfile::tempdir().unwrap();
        prepare_home(home.path(), &settings()).unwrap();
        let qwen = home.path().join(".qwen");
        fs::write(qwen.join("QWEN.md"), "- the user prefers tabs\n").unwrap();
        fs::write(qwen.join("AGENTS.md"), "- tests run with make\n").unwrap();

        prepare_home(home.path(), &settings()).unwrap();

        assert!(!qwen.join("QWEN.md").exists());
        assert!(!qwen.join("AGENTS.md").exists());
    }

    #[test]
    fn the_settings_hold_the_session_s_window_and_the_media_server() {
        let model = SessionModel {
            name: "engine/critic".into(),
            id: "critic".into(),
            context_window: 32_768,
            supports_images: false,
        };
        let json = settings().for_model(&model).to_json();
        assert_eq!(
            json["model"]["generationConfig"]["contextWindowSize"],
            32_768
        );
        assert_eq!(
            json["mcpServers"]["media"],
            serde_json::json!({ "httpUrl": "http://127.0.0.1:4002/mcp", "timeout": 1_800_000 })
        );
    }

    #[test]
    fn only_the_user_level_skills_load_unless_every_skill_may() {
        assert_eq!(
            settings().to_json()["skills"]["disabledLevels"],
            serde_json::json!(["project", "extension", "bundled"])
        );
        let every = QwenSettings {
            discover_skills: true,
            ..settings()
        };
        assert!(every.to_json().get("skills").is_none());
    }

    #[test]
    fn each_session_names_its_model_needs_no_key_and_asks_before_edits() {
        let launch = QwenLaunch::new("/opt/qwen", "/run/home", "http://127.0.0.1:4001/v1");
        let args = [
            "--acp",
            "--auth-type",
            "openai",
            "--openai-base-url",
            "http://127.0.0.1:4001/v1",
            "--openai-api-key",
            "none",
            "--model",
            "critic",
            "--approval-mode",
            "default",
        ]
        .map(str::to_owned)
        .to_vec();
        let critic = SessionModel {
            name: "engine/critic".into(),
            id: "critic".into(),
            context_window: 32_768,
            supports_images: false,
        };
        assert_eq!(launch.command_args(&critic.id), args);
    }

    #[test]
    fn qwen_code_runs_in_its_own_home() {
        let launch = QwenLaunch::new("/opt/qwen", "/run/home", "http://127.0.0.1:4001/v1");
        assert_eq!(launch.env(), [("HOME".to_owned(), "/run/home".to_owned())]);
    }
}
