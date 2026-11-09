//! `DeepSeek` Harness (`dsh`) specifics: the patch that points its `acp`
//! profile at the engine proxy, and how `dsh --profile acp` is launched in a
//! home of its own (ADR 0027).

use std::io;
use std::path::{Path, PathBuf};

use tenaz_core::ledger::HarnessKind;
use tenaz_core::ports::{SessionModel, SessionSpec};

use crate::agent::AgentProfile;
use crate::write_json;

/// The opening of dsh's request to summarize its conversation
/// (`COMPACTION_INSTRUCTION` in `dsh-compaction-basic` 0.2.2-rc.1, sent as
/// the last user message). The patch turns automatic compaction off; a
/// request that still arrives is refused, as Qwen Code's is (ADR 0030).
pub const COMPACTION_PROMPT: &str =
    "You are now acting as a compaction engine for this AI coding assistant.";

/// The provider name the patch gives the engine proxy inside dsh.
const PROVIDER: &str = "tenaz";

/// What Tenaz overrides in dsh's `acp` profile, which by default uses the
/// `deepseek-official` provider: the default model and the ACP app's model
/// (both on the proxy), an OpenAI-compatible provider at the proxy with
/// the session's model, and no automatic compaction, so that Tenaz's
/// context budget decides when a session ends (ADR 0030).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DshPatch {
    /// The proxy's base URL (`http://127.0.0.1:<port>/v1`).
    pub base_url: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub supports_images: bool,
    /// The reasoning effort dsh asks for (`low`, `medium` or `xhigh`);
    /// `None` sends none, and the engine's own default applies.
    pub effort: Option<String>,
}

impl DshPatch {
    /// This patch with `model` in place of the model it names.
    #[must_use]
    pub fn for_model(&self, model: &SessionModel) -> Self {
        Self {
            model: model.id.clone(),
            context_window: model.context_window,
            supports_images: model.supports_images,
            ..self.clone()
        }
    }

    /// The patch list. dsh reads YAML; this is JSON, which YAML reads too.
    pub fn to_json(&self) -> serde_json::Value {
        let input = if self.supports_images {
            vec!["text", "image"]
        } else {
            vec!["text"]
        };
        let uses_proxy = serde_json::json!({ "provider": PROVIDER, "model": self.model });
        let mut provider = serde_json::json!({
            "displayName": PROVIDER,
            "api": "openai-completions",
            "baseURL": self.base_url,
            "headers": { "Authorization": "Bearer local" },
            "compat": {
                "supportsDeveloperRole": false,
                "supportsReasoningEffort": self.effort.is_some(),
                "maxTokensField": "max_tokens"
            },
            "models": [{
                "id": self.model,
                "name": self.model,
                "contextWindow": self.context_window,
                "maxTokens": self.max_output_tokens,
                "input": input,
                "reasoningEfforts": {
                    "off": "off", "low": "low", "medium": "medium", "xhigh": "xhigh"
                }
            }]
        });
        if let Some(effort) = &self.effort {
            provider["reasoning"] = effort.clone().into();
        }
        serde_json::json!([
            {
                "id": "agent-default-model",
                "name": "@deepseek-ai/dsh-agent-default-model",
                "config": uses_proxy
            },
            {
                "id": "acp",
                "name": "@deepseek-ai/dsh-acp",
                // Replacing the entry drops what it injects unless listed again.
                "inject": ["acpAppStartup"],
                "config": uses_proxy
            },
            {
                "id": "llm-pi-ai",
                "name": "@deepseek-ai/dsh-llm-pi-ai",
                "config": { "providers": { PROVIDER: provider } }
            },
            {
                // dsh-base gives this entry no config, so none is lost.
                "id": "compaction-basic",
                "name": "@deepseek-ai/dsh-compaction-basic",
                "config": { "auto": false }
            }
        ])
    }
}

/// dsh's reasoning effort for the run's thinking level: none when the
/// level is unset, `off`, `none` or `auto` (as for the other agents),
/// `low`, `xhigh` for `high`, and `medium` otherwise.
pub fn effort(thinking: Option<&str>) -> Option<&'static str> {
    match thinking.map(str::trim) {
        None | Some("" | "off" | "none" | "auto") => None,
        Some("low" | "minimal") => Some("low"),
        Some("high" | "xhigh") => Some("xhigh"),
        Some(_) => Some("medium"),
    }
}

/// How dsh is launched for ACP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DshLaunch {
    pub binary: PathBuf,
    /// The isolated `HOME`; dsh's own home (`DSH_HOME`, its profiles and
    /// sessions) is `.dsh` inside it, and the patch is written there.
    pub home: PathBuf,
    /// A program and leading arguments dsh is started through: the sandbox
    /// launcher, when the run is sandboxed.
    pub wrapper: Vec<String>,
}

impl DshLaunch {
    pub fn new(binary: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            home: home.into(),
            wrapper: Vec::new(),
        }
    }

    /// `DSH_HOME`: where dsh creates its `acp` profile on first start.
    pub fn dsh_home(&self) -> PathBuf {
        self.home.join(".dsh")
    }

    /// Where the patch is written.
    pub fn patch_path(&self) -> PathBuf {
        self.dsh_home().join("tenaz-patch.yml")
    }

    /// `--profile acp --patch <patch>`.
    pub fn command_args(&self) -> Vec<String> {
        vec![
            "--profile".to_owned(),
            "acp".to_owned(),
            "--patch".to_owned(),
            self.patch_path().display().to_string(),
        ]
    }

    /// `HOME` and `DSH_HOME` inside `home`, so the user's own profiles and
    /// sessions stay out.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".to_owned(), self.home.display().to_string()),
            ("DSH_HOME".to_owned(), self.dsh_home().display().to_string()),
        ]
    }
}

/// dsh as an [`AgentProfile`]: its patch written for each session's model.
#[derive(Clone, Debug)]
pub struct DshAgent {
    pub launch: DshLaunch,
    pub patch: DshPatch,
}

impl AgentProfile for DshAgent {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Dsh
    }

    fn config_name(&self) -> &'static str {
        "the dsh patch"
    }

    fn program(&self) -> &Path {
        &self.launch.binary
    }

    fn args(&self, _spec: &SessionSpec) -> Vec<String> {
        self.launch.command_args()
    }

    fn env(&self) -> Vec<(String, String)> {
        self.launch.env()
    }

    fn wrapper(&self) -> &[String] {
        &self.launch.wrapper
    }

    fn write_config(&self, model: &SessionModel) -> io::Result<()> {
        write_json(
            &self.launch.patch_path(),
            &self.patch.for_model(model).to_json(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch() -> DshPatch {
        DshPatch {
            base_url: "http://127.0.0.1:4001/v1".into(),
            model: "run".into(),
            context_window: 131_072,
            max_output_tokens: 16_384,
            supports_images: false,
            effort: Some("medium".into()),
        }
    }

    fn critic(supports_images: bool) -> SessionModel {
        SessionModel {
            name: "engine/critic".into(),
            id: "critic".into(),
            context_window: 32_768,
            supports_images,
        }
    }

    fn entry<'a>(json: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
        json.as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap_or_else(|| panic!("no patch entry with id {id}"))
    }

    #[test]
    fn both_model_entries_use_the_proxy_and_the_acp_app_still_starts() {
        let json = patch().to_json();
        for id in ["agent-default-model", "acp"] {
            assert_eq!(
                entry(&json, id)["config"],
                serde_json::json!({ "provider": "tenaz", "model": "run" }),
                "{id}"
            );
        }
        assert_eq!(
            entry(&json, "acp")["inject"],
            serde_json::json!(["acpAppStartup"])
        );
    }

    #[test]
    fn the_proxy_is_an_openai_compatible_provider_with_the_model_s_limits() {
        let json = patch().to_json();
        let provider = &entry(&json, "llm-pi-ai")["config"]["providers"]["tenaz"];
        assert_eq!(provider["api"], "openai-completions");
        assert_eq!(provider["baseURL"], "http://127.0.0.1:4001/v1");
        assert_eq!(provider["reasoning"], "medium");
        assert_eq!(
            provider["compat"],
            serde_json::json!({
                "supportsDeveloperRole": false,
                "supportsReasoningEffort": true,
                "maxTokensField": "max_tokens"
            })
        );
        let model = &provider["models"][0];
        assert_eq!(model["contextWindow"], 131_072);
        assert_eq!(model["maxTokens"], 16_384);
        assert_eq!(model["input"], serde_json::json!(["text"]));
    }

    #[test]
    fn the_patch_turns_off_dsh_s_automatic_compaction() {
        let json = patch().to_json();
        let compaction = entry(&json, "compaction-basic");
        assert_eq!(compaction["name"], "@deepseek-ai/dsh-compaction-basic");
        assert_eq!(compaction["config"], serde_json::json!({ "auto": false }));
    }

    #[test]
    fn without_an_effort_dsh_sends_none() {
        let json = DshPatch {
            effort: None,
            ..patch()
        }
        .to_json();
        let provider = &entry(&json, "llm-pi-ai")["config"]["providers"]["tenaz"];
        assert_eq!(provider["compat"]["supportsReasoningEffort"], false);
        assert!(provider.get("reasoning").is_none());
    }

    #[test]
    fn a_session_s_model_replaces_the_run_s_model() {
        let critic = critic(true);
        let json = patch().for_model(&critic).to_json();
        assert_eq!(entry(&json, "acp")["config"]["model"], "critic");
        let model = &entry(&json, "llm-pi-ai")["config"]["providers"]["tenaz"]["models"][0];
        assert_eq!(model["id"], "critic");
        assert_eq!(model["contextWindow"], 32_768);
        assert_eq!(model["input"], serde_json::json!(["text", "image"]));
    }

    #[test]
    fn the_thinking_level_maps_to_an_effort_dsh_knows() {
        for (thinking, expected) in [
            (None, None),
            (Some("off"), None),
            (Some("none"), None),
            (Some("auto"), None),
            (Some("low"), Some("low")),
            (Some("minimal"), Some("low")),
            (Some("medium"), Some("medium")),
            (Some("high"), Some("xhigh")),
            (Some("xhigh"), Some("xhigh")),
            (Some(" high "), Some("xhigh")),
        ] {
            assert_eq!(effort(thinking), expected, "{thinking:?}");
        }
    }

    #[test]
    fn dsh_runs_its_acp_profile_in_its_own_home_with_the_patch() {
        let launch = DshLaunch::new("/opt/dsh", "/run/home");
        assert_eq!(
            launch.command_args(),
            [
                "--profile",
                "acp",
                "--patch",
                "/run/home/.dsh/tenaz-patch.yml"
            ]
        );
        assert_eq!(
            launch.env(),
            [
                ("HOME".to_owned(), "/run/home".to_owned()),
                ("DSH_HOME".to_owned(), "/run/home/.dsh".to_owned()),
            ]
        );
    }

    #[test]
    fn the_patch_is_written_where_the_command_line_names_it() {
        let home = tempfile::tempdir().unwrap();
        let agent = DshAgent {
            launch: DshLaunch::new("/opt/dsh", home.path()),
            patch: patch(),
        };
        let critic = critic(false);
        agent.write_config(&critic).unwrap();
        let spec = SessionSpec {
            label: "001-builder-T001".into(),
            cwd: home.path().to_path_buf(),
            persist_dir: None,
            raw_log: None,
            thinking_log: None,
            stderr_log: None,
            model: critic.clone(),
        };
        let args = agent.args(&spec);
        let named = args
            .iter()
            .skip_while(|a| *a != "--patch")
            .nth(1)
            .expect("the command line names a patch");
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(named).unwrap()).unwrap();
        assert_eq!(written, patch().for_model(&critic).to_json());
    }
}
