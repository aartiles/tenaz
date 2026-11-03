//! Qwen Code specifics: its settings in an isolated `HOME` and how
//! `qwen --acp` is launched (ADR 0018).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tenaz_core::ledger::HarnessKind;
use tenaz_core::ports::{HarnessInfo, SessionModel};
use tenaz_core::util::wrap_command;

use crate::{AcpHarness, ConfiguredHarness, ProfileMcpServer, write_json};

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

    /// The program and arguments for a session on `model` (the id the
    /// engine lists, not the provider's name for it), through the wrapper
    /// when there is one.
    pub fn session_command(&self, model: &SessionModel) -> (PathBuf, Vec<String>) {
        wrap_command(&self.wrapper, &self.binary, self.command_args(&model.id))
    }

    /// The environment Qwen Code starts with: `HOME` at `home`, where it
    /// finds the settings Tenaz writes.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![("HOME".to_owned(), self.home.display().to_string())]
    }

    /// The agent: `qwen --acp` with [`env`](Self::env), each session on its
    /// own model. [`configured`](Self::configured) prepares its home for
    /// that model.
    pub fn harness(&self, version: String) -> AcpHarness {
        let info = HarnessInfo {
            name: HarnessKind::Qwen.as_str().to_owned(),
            version,
        };
        let launch = self.clone();
        let (program, _) = wrap_command(&self.wrapper, &self.binary, Vec::new());
        AcpHarness::subprocess_with(
            info,
            program,
            Arc::new(move |spec| launch.session_command(&spec.model).1),
            self.env(),
        )
    }

    /// `acp` with its home prepared ([`prepare_home`]) for each session's
    /// model before Qwen Code starts, and now for the run's `first`.
    pub fn configured(
        &self,
        acp: AcpHarness,
        settings: QwenSettings,
        first: &SessionModel,
    ) -> io::Result<ConfiguredHarness> {
        let home = self.home.clone();
        ConfiguredHarness::new(
            acp,
            "the Qwen Code settings",
            Arc::new(move |model| prepare_home(&home, &settings.for_model(model))),
            first,
        )
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
        let mut launch = QwenLaunch::new("/opt/qwen", "/run/home", "http://127.0.0.1:4001/v1");
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
        assert_eq!(
            launch.session_command(&critic),
            ("/opt/qwen".into(), args.clone())
        );
        launch.wrapper = vec!["/sbx".into(), "--".into()];
        let wrapped = [vec!["--".to_owned(), "/opt/qwen".to_owned()], args].concat();
        assert_eq!(launch.session_command(&critic), ("/sbx".into(), wrapped));
    }

    #[test]
    fn qwen_code_runs_in_its_own_home() {
        let launch = QwenLaunch::new("/opt/qwen", "/run/home", "http://127.0.0.1:4001/v1");
        assert_eq!(launch.env(), [("HOME".to_owned(), "/run/home".to_owned())]);
    }
}
