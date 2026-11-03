//! `OpenCode` specifics: its configuration in an isolated `HOME` and how
//! `opencode acp` is launched (ADR 0018).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tenaz_core::ledger::HarnessKind;
use tenaz_core::ports::{HarnessInfo, SessionModel};
use tenaz_core::util::wrap_command;

use crate::{AcpHarness, ConfiguredHarness, ProfileMcpServer, write_json};

/// What Tenaz writes into `OpenCode`'s configuration: one OpenAI-compatible
/// provider (the engine proxy) with the session's model, and permissions
/// that make `OpenCode` ask the client before every edit and command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeConfig {
    pub provider: String,
    /// The proxy's base URL (`http://127.0.0.1:<port>/v1`).
    pub base_url: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub supports_images: bool,
    pub mcp: Vec<ProfileMcpServer>,
    /// The only skills `OpenCode` may load; `None` lets it load every skill
    /// it finds.
    pub skills: Option<Vec<String>>,
}

impl OpenCodeConfig {
    /// This configuration with `model` in place of the model it names.
    #[must_use]
    pub fn for_model(&self, model: &SessionModel) -> Self {
        Self {
            model: model.id.clone(),
            context_window: model.context_window,
            supports_images: model.supports_images,
            ..self.clone()
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let input = if self.supports_images {
            vec!["text", "image"]
        } else {
            vec!["text"]
        };
        let mcp: serde_json::Map<String, serde_json::Value> = self
            .mcp
            .iter()
            .map(|s| {
                let server = serde_json::json!({
                    "type": "remote",
                    "url": s.url,
                    "enabled": true,
                    "oauth": false,
                    "timeout": s.operation_timeout_ms
                });
                (s.name.clone(), server)
            })
            .collect();
        let mut permission = serde_json::json!({
            "edit": "ask",
            "bash": "ask",
            "webfetch": "deny"
        });
        if let Some(skills) = &self.skills {
            // A denied skill is not listed to the model.
            let mut rules = serde_json::Map::new();
            rules.insert("*".to_owned(), "deny".into());
            for name in skills {
                rules.insert(name.clone(), "allow".into());
            }
            permission["skill"] = rules.into();
        }
        serde_json::json!({
            "$schema": "https://opencode.ai/config.json",
            "autoupdate": false,
            "share": "disabled",
            "enabled_providers": [&self.provider],
            "provider": {
                &self.provider: {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": &self.provider,
                    "options": { "baseURL": self.base_url },
                    "models": {
                        &self.model: {
                            "name": self.model,
                            "tool_call": true,
                            "modalities": { "input": input, "output": ["text"] },
                            "limit": {
                                "context": self.context_window,
                                "output": self.max_output_tokens
                            }
                        }
                    }
                }
            },
            "model": format!("{}/{}", self.provider, self.model),
            "permission": permission,
            "mcp": mcp
        })
    }
}

/// How `OpenCode` is launched for ACP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeLaunch {
    pub binary: PathBuf,
    /// The isolated `HOME`, which holds the configuration and everything
    /// `OpenCode` keeps between sessions.
    pub home: PathBuf,
    /// npm's cache: the user's own, so that the package `OpenCode` installs
    /// into its configuration folder at start needs no network after the
    /// first run. `None` leaves it in `home`.
    pub npm_cache: Option<PathBuf>,
    /// A program and leading arguments `OpenCode` is started through: the
    /// sandbox launcher, when the run is sandboxed.
    pub wrapper: Vec<String>,
}

impl OpenCodeLaunch {
    pub fn new(binary: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            home: home.into(),
            npm_cache: None,
            wrapper: Vec::new(),
        }
    }

    /// Where the configuration is written (`OPENCODE_CONFIG`).
    pub fn config_path(&self) -> PathBuf {
        config_path(&self.home)
    }

    /// The environment: `HOME` and the XDG folders inside `home`, so the
    /// user's own configuration, data and plugins stay out, the
    /// configuration file, and no update or model list fetched.
    pub fn env(&self) -> Vec<(String, String)> {
        let at = |path: &str| self.home.join(path).display().to_string();
        let mut env = vec![
            ("HOME".to_owned(), self.home.display().to_string()),
            ("XDG_CONFIG_HOME".to_owned(), at(".config")),
            ("XDG_DATA_HOME".to_owned(), at(".local/share")),
            ("XDG_STATE_HOME".to_owned(), at(".local/state")),
            ("XDG_CACHE_HOME".to_owned(), at(".cache")),
            (
                "OPENCODE_CONFIG".to_owned(),
                self.config_path().display().to_string(),
            ),
            // Set in the user's shell, these would bring their own configuration in.
            ("OPENCODE_CONFIG_DIR".to_owned(), at(".config/opencode")),
            ("OPENCODE_CONFIG_CONTENT".to_owned(), "{}".to_owned()),
            ("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned()),
            ("OPENCODE_DISABLE_MODELS_FETCH".to_owned(), "1".to_owned()),
        ];
        if let Some(cache) = &self.npm_cache {
            env.push(("npm_config_cache".to_owned(), cache.display().to_string()));
        }
        env
    }

    /// `opencode acp`, through the wrapper when there is one.
    pub fn command(&self) -> (PathBuf, Vec<String>) {
        wrap_command(&self.wrapper, &self.binary, vec!["acp".to_owned()])
    }

    /// The agent: [`command`](Self::command) with [`env`](Self::env).
    /// [`configured`] writes its configuration for each session's model.
    pub fn harness(&self, version: String) -> AcpHarness {
        let info = HarnessInfo {
            name: HarnessKind::OpenCode.as_str().to_owned(),
            version,
        };
        let (command, args) = self.command();
        AcpHarness::subprocess(info, command, args, self.env())
    }
}

/// `acp` with `config` written under `home` for each session's model
/// before `OpenCode` starts.
pub fn configured(acp: AcpHarness, home: PathBuf, config: OpenCodeConfig) -> ConfiguredHarness {
    ConfiguredHarness::new(
        acp,
        Arc::new(move |model| write_config(&home, &config.for_model(model))),
    )
}

/// Where the configuration is written under `home`.
fn config_path(home: &Path) -> PathBuf {
    home.join("opencode.json")
}

/// Writes the configuration under `home`.
pub fn write_config(home: &Path, config: &OpenCodeConfig) -> io::Result<()> {
    write_json(&config_path(home), &config.to_json())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn config() -> OpenCodeConfig {
        OpenCodeConfig {
            provider: "engine".into(),
            base_url: "http://127.0.0.1:4001/v1".into(),
            model: "Qwen3.8".into(),
            context_window: 131_072,
            max_output_tokens: 16_384,
            supports_images: false,
            mcp: vec![ProfileMcpServer {
                name: "media".into(),
                url: "http://127.0.0.1:4002/mcp".into(),
                operation_timeout_ms: 1_800_000,
            }],
            skills: None,
        }
    }

    #[test]
    fn the_proxy_serves_the_run_s_model() {
        let json = config().to_json();
        let provider = &json["provider"]["engine"];
        assert_eq!(provider["options"]["baseURL"], "http://127.0.0.1:4001/v1");
        assert_eq!(
            provider["models"]["Qwen3.8"]["limit"],
            serde_json::json!({ "context": 131_072, "output": 16_384 })
        );
        assert_eq!(
            provider["models"]["Qwen3.8"]["modalities"]["input"],
            serde_json::json!(["text"])
        );
        assert_eq!(json["model"], "engine/Qwen3.8");
        assert_eq!(json["enabled_providers"], serde_json::json!(["engine"]));
    }

    #[test]
    fn opencode_asks_before_edits_and_commands_and_never_fetches_the_web() {
        assert_eq!(
            config().to_json()["permission"],
            serde_json::json!({ "edit": "ask", "bash": "ask", "webfetch": "deny" })
        );
    }

    #[test]
    fn nothing_is_shared_or_updated() {
        let json = config().to_json();
        assert_eq!(
            (&json["autoupdate"], &json["share"]),
            (&false.into(), &"disabled".into())
        );
    }

    #[test]
    fn the_media_server_is_remote_without_oauth_and_with_the_media_time_limit() {
        assert_eq!(
            config().to_json()["mcp"]["media"],
            serde_json::json!({
                "type": "remote",
                "url": "http://127.0.0.1:4002/mcp",
                "enabled": true,
                "oauth": false,
                "timeout": 1_800_000
            })
        );
    }

    #[test]
    fn a_session_s_model_replaces_the_run_s_model() {
        let model = SessionModel {
            name: "engine/critic".into(),
            id: "critic".into(),
            context_window: 32_768,
            supports_images: true,
        };
        let json = config().for_model(&model).to_json();
        let models = json["provider"]["engine"]["models"].as_object().unwrap();
        assert_eq!(models.keys().collect::<Vec<_>>(), ["critic"]);
        assert_eq!(models["critic"]["limit"]["context"], 32_768);
        assert_eq!(
            models["critic"]["modalities"]["input"],
            serde_json::json!(["text", "image"])
        );
        assert_eq!(json["model"], "engine/critic");
    }

    #[test]
    fn only_the_pinned_skills_are_allowed_unless_every_skill_is() {
        let pinned = OpenCodeConfig {
            skills: Some(vec!["rust".into()]),
            ..config()
        };
        assert_eq!(
            pinned.to_json()["permission"]["skill"],
            serde_json::json!({ "*": "deny", "rust": "allow" })
        );
        assert!(config().to_json()["permission"].get("skill").is_none());
    }

    #[test]
    fn opencode_runs_in_its_own_home_with_its_configuration() {
        let mut launch = OpenCodeLaunch::new("/opt/opencode", "/run/home");
        let env = |launch: &OpenCodeLaunch| launch.env().into_iter().collect::<BTreeMap<_, _>>();
        let owned = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            env(&launch),
            owned(&[
                ("HOME", "/run/home"),
                ("XDG_CONFIG_HOME", "/run/home/.config"),
                ("XDG_DATA_HOME", "/run/home/.local/share"),
                ("XDG_STATE_HOME", "/run/home/.local/state"),
                ("XDG_CACHE_HOME", "/run/home/.cache"),
                ("OPENCODE_CONFIG", "/run/home/opencode.json"),
                ("OPENCODE_CONFIG_DIR", "/run/home/.config/opencode"),
                ("OPENCODE_CONFIG_CONTENT", "{}"),
                ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
                ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
            ])
        );
        launch.npm_cache = Some("/Users/me/.npm".into());
        assert_eq!(env(&launch)["npm_config_cache"], "/Users/me/.npm");
    }

    #[test]
    fn opencode_is_started_through_the_sandbox_launcher() {
        let mut launch = OpenCodeLaunch::new("/opt/opencode", "/run/home");
        assert_eq!(
            launch.command(),
            ("/opt/opencode".into(), vec!["acp".to_owned()])
        );
        launch.wrapper = vec!["/sbx".into(), "--".into()];
        assert_eq!(
            launch.command(),
            (
                "/sbx".into(),
                vec!["--".to_owned(), "/opt/opencode".into(), "acp".into()]
            )
        );
    }
}
