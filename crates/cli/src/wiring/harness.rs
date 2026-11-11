//! The harness of a run, built from the ledger: each kind writes its own
//! configuration into the project (ADR 0019) and is launched through the
//! sandbox when the run has one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tenaz_acp::agent::{self, AgentProfile};
use tenaz_acp::dsh::{DshAgent, DshLaunch, DshPatch};
use tenaz_acp::fx::{self, FxAgent, FxLaunch, FxSettings};
use tenaz_acp::generic::{GenericAgent, SessionFacts};
use tenaz_acp::opencode::{OpenCodeAgent, OpenCodeConfig, OpenCodeLaunch};
use tenaz_acp::qwen::{QwenAgent, QwenLaunch, QwenSettings};
use tenaz_acp::{AcpHarness, EngineObservation, McpEndpoint, ProfileMcpServer};
use tenaz_core::ledger::{HarnessKind, ModelConfig, SkillPin};
use tenaz_core::ports::Harness;
use tenaz_core::project::Project;
use tenaz_core::testing::ScriptedHarness;
use tenaz_pi::PiLaunch;

use super::{HARNESS_MAX_OUTPUT_TOKENS, HarnessOptions, reasoning_effort};

/// What the harnesses are built from; each takes the part it needs.
pub(super) struct HarnessInputs<'a> {
    pub(super) project: &'a Project,
    pub(super) config: &'a ModelConfig,
    pub(super) options: &'a HarnessOptions,
    pub(super) media_timeout: Duration,
    /// The proxy's base URL, or the engine's with `--no-proxy`.
    pub(super) base_url: String,
    pub(super) mcp_endpoints: &'a [McpEndpoint],
    pub(super) skills: &'a [SkillPin],
    pub(super) agent_skills: bool,
    pub(super) verify_path: &'a str,
    pub(super) feed: &'a tokio::sync::broadcast::Sender<EngineObservation>,
    /// The sandbox launcher, when the run is sandboxed.
    pub(super) wrapper: &'a [String],
    /// The user's browser and package caches, for an agent with an
    /// isolated `HOME` (see [`user_caches`]).
    pub(super) caches: &'a [(String, String)],
}

impl HarnessInputs<'_> {
    /// The run's MCP servers as an agent's own configuration lists them,
    /// with the media time limit.
    fn profile_mcp(&self) -> Vec<ProfileMcpServer> {
        let timeout_ms = u64::try_from(self.media_timeout.as_millis()).unwrap_or(u64::MAX);
        self.mcp_endpoints
            .iter()
            .map(|e| ProfileMcpServer {
                name: e.name.clone(),
                url: e.url.clone(),
                operation_timeout_ms: timeout_ms,
            })
            .collect()
    }

    /// The agent's isolated `HOME`, `.tenaz/private/<name>`.
    fn home(&self, name: &str) -> PathBuf {
        self.project.private_dir().join(name)
    }

    /// Every ACP agent the same way: launched as `agent` says, fed the
    /// engine's observations, given `meta` in `session/new`, with its
    /// configuration written for the run's model (a home that cannot be
    /// written stops the run here, not in its first session), then the
    /// pinned skills linked into `skills_dir`, for an agent that has one.
    fn acp_agent(
        &self,
        agent: Arc<dyn AgentProfile>,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
        skills_dir: Option<&Path>,
    ) -> anyhow::Result<Arc<dyn Harness>> {
        let mut acp: AcpHarness =
            agent::harness(agent.clone(), self.config.harness_version.clone())
                .with_observations(self.feed.clone());
        if let Some(meta) = meta {
            acp = acp.with_session_meta(meta);
        }
        let configured = agent::configured(agent, acp, &self.config.run_model())?;
        match skills_dir {
            Some(dir) => link_skills(self.project, dir, self.skills)?,
            None if !self.skills.is_empty() => {
                tracing::warn!("{} is not given the pinned skills", self.config.harness);
            }
            None => {}
        }
        Ok(Arc::new(configured))
    }
}

/// The harness named in the ledger: fx, `OpenCode` or Qwen Code over ACP
/// with their configuration written into the project, Pi in RPC mode with
/// the pinned skills, or the scripted fake (writing `verify` where the run
/// expects it).
pub(super) fn build_harness(inputs: &HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    match inputs.config.harness {
        // The scripted harness writes from inside Tenaz: there is no process to sandbox.
        HarnessKind::Fake => Ok(Arc::new(ScriptedHarness {
            verify_path: Some(inputs.verify_path.to_owned()),
            ..ScriptedHarness::every_recovery()
        })),
        HarnessKind::Pi => pi_harness(inputs),
        HarnessKind::Fx => fx_harness(inputs),
        HarnessKind::OpenCode => opencode_harness(inputs),
        HarnessKind::Qwen => qwen_harness(inputs),
        HarnessKind::Dsh => dsh_harness(inputs),
        HarnessKind::Acp => generic_harness(inputs),
    }
}

/// Pi with an agent directory of its own pointing it at the proxy (or the
/// engine); the media MCP server is not on its path.
fn pi_harness(inputs: &HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let HarnessInputs {
        project, config, ..
    } = inputs;
    let agent_dir = inputs.home("pi-agent");
    let user_models = tenaz_pi::models::default_models_path();
    let models = write_pi_agent(&agent_dir, config, &inputs.base_url, user_models.as_deref())?;
    let launch = PiLaunch::new(&inputs.options.bin, models.run_name())
        .other_models(models.other_names())
        .thinking(
            config
                .thinking
                .clone()
                .unwrap_or_else(|| "medium".to_owned()),
        )
        .skills(
            inputs
                .skills
                .iter()
                .map(|s| project.tenaz_dir().join("skills").join(&s.name))
                .collect(),
        )
        .agent_dir(&agent_dir)
        .discover_skills(inputs.agent_skills)
        .wrapper(inputs.wrapper.to_vec());
    Ok(Arc::new(launch.harness(config.harness_version.clone())))
}

/// fx with its profile in `.tenaz/private/fx-home`.
fn fx_harness(inputs: &HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let HarnessInputs {
        project,
        config,
        skills,
        agent_skills,
        ..
    } = *inputs;
    let home = inputs.home("fx-home");
    let settings = FxSettings {
        provider: provider(config),
        base_url: inputs.base_url.clone(),
        model: config.id.clone(),
        context_window: config.context_window,
        max_output_tokens: HARNESS_MAX_OUTPUT_TOKENS,
        supports_vision: config.supports_images(),
        permission_mode: "ask".to_owned(),
        effort: reasoning_effort(config.thinking.as_deref()).map(str::to_owned),
        skill_catalog: agent_skills || !skills.is_empty(),
    };
    fx::write_mcp(&home, &inputs.profile_mcp()).context("cannot write the fx MCP servers")?;
    if !skills.is_empty() && !agent_skills {
        tracing::warn!(
            "fx lists the pinned skills, and with them any skills it finds in the folders \
             above the project; Pi loads only the pinned ones"
        );
    }
    let mut launch = FxLaunch::new(&inputs.options.bin, &home);
    launch.args.clone_from(&inputs.options.fx_args);
    launch.log_dir = Some(project.private_dir());
    // fx is started through this binary, which puts right what fx
    // says outside the protocol (see tenaz_acp::fx::normalize_line).
    launch.shim = std::env::current_exe().ok();
    launch.wrapper = inputs.wrapper.to_vec();
    // Profile servers accept a long operation timeout; servers passed in session/new are cut at 60 s.
    let meta = (!inputs.mcp_endpoints.is_empty()).then(|| {
        serde_json::json!({ "fx": { "profileMcpServers": true } })
            .as_object()
            .cloned()
            .unwrap_or_default()
    });
    inputs.acp_agent(
        Arc::new(FxAgent { launch, settings }),
        meta,
        Some(&home.join(".agents/skills")),
    )
}

/// `OpenCode` with its configuration in `.tenaz/private/opencode-home`.
fn opencode_harness(inputs: &HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let config = inputs.config;
    let home = inputs.home("opencode-home");
    let opencode = OpenCodeConfig {
        provider: provider(config),
        mcp: inputs.profile_mcp(),
        base_url: inputs.base_url.clone(),
        model: config.id.clone(),
        context_window: config.context_window,
        max_output_tokens: HARNESS_MAX_OUTPUT_TOKENS,
        supports_images: config.supports_images(),
        skills: allowed_skills(inputs.skills, inputs.agent_skills),
    };
    let mut launch = OpenCodeLaunch::new(&inputs.options.bin, &home);
    // npm's own cache, which the sandbox lets the agent write by default;
    // a `sandboxAllowWrite` without `~/.npm` leaves OpenCode unable to start.
    launch.npm_cache = std::env::home_dir().map(|h| h.join(".npm"));
    launch.wrapper = inputs.wrapper.to_vec();
    inputs.acp_agent(
        Arc::new(OpenCodeAgent {
            launch,
            config: opencode,
        }),
        None,
        // OpenCode finds skills in `.agents/skills` of its home, as fx does.
        Some(&home.join(".agents/skills")),
    )
}

/// Qwen Code with its settings in `.tenaz/private/qwen-home`.
fn qwen_harness(inputs: &HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let home = inputs.home("qwen-home");
    let settings = QwenSettings {
        context_window: inputs.config.context_window,
        mcp: inputs.profile_mcp(),
        discover_skills: inputs.agent_skills,
    };
    let mut launch = QwenLaunch::new(&inputs.options.bin, &home, inputs.base_url.clone());
    launch.wrapper = inputs.wrapper.to_vec();
    inputs.acp_agent(
        Arc::new(QwenAgent { launch, settings }),
        None,
        Some(&home.join(".qwen/skills")),
    )
}

/// `DeepSeek` Harness with its patch and its own `DSH_HOME` in
/// `.tenaz/private/dsh-home`. It is given neither the media MCP server nor
/// the pinned skills yet (see Known gaps in docs/COMPATIBILITY.md).
fn dsh_harness(inputs: &HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let config = inputs.config;
    let home = inputs.home("dsh-home");
    let patch = DshPatch {
        base_url: inputs.base_url.clone(),
        model: config.id.clone(),
        context_window: config.context_window,
        max_output_tokens: HARNESS_MAX_OUTPUT_TOKENS,
        supports_images: config.supports_images(),
        effort: tenaz_acp::dsh::effort(config.thinking.as_deref()).map(str::to_owned),
    };
    // The user's own servers never get here: init and run refuse them for
    // dsh (tenaz_core::mcp_servers::check_servers).
    if !inputs.mcp_endpoints.is_empty() {
        tracing::warn!("dsh is not given the media tools");
    }
    let mut launch = DshLaunch::new(&inputs.options.bin, &home);
    launch.wrapper = inputs.wrapper.to_vec();
    launch.caches = inputs.caches.to_vec();
    inputs.acp_agent(Arc::new(DshAgent { launch, patch }), None, None)
}

/// The user's ACP command, told about each session in
/// `.tenaz/private/acp-session.json`; it keeps the user's `HOME`.
fn generic_harness(inputs: &HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let config = inputs.config;
    let agent = GenericAgent {
        binary: inputs.options.bin.clone(),
        args: inputs.options.acp_args.clone(),
        session_file: inputs.project.private_dir().join("acp-session.json"),
        facts: SessionFacts {
            base_url: inputs.base_url.clone(),
            max_output_tokens: HARNESS_MAX_OUTPUT_TOKENS,
            reasoning_effort: reasoning_effort(config.thinking.as_deref()).map(str::to_owned),
            mcp_servers: inputs.mcp_endpoints.to_vec(),
        },
        wrapper: inputs.wrapper.to_vec(),
    };
    inputs.acp_agent(Arc::new(agent), None, None)
}

/// The user's browser and package caches, for an agent with an isolated
/// `HOME` (see [`tenaz_acp::caches::user_caches`]).
pub(super) fn user_caches() -> Vec<(String, String)> {
    std::env::home_dir().map_or_else(Vec::new, |home| {
        tenaz_acp::caches::user_caches(
            &home,
            |name| std::env::var(name).ok(),
            cfg!(target_os = "macos"),
        )
    })
}

/// The provider name the harness's configuration uses.
fn provider(config: &ModelConfig) -> String {
    config
        .provider
        .clone()
        .unwrap_or_else(|| "engine".to_owned())
}

/// The skills `OpenCode` may load: only the pinned ones, unless the run
/// lets the agent load every skill it finds (`agentSkills`).
fn allowed_skills(skills: &[SkillPin], agent_skills: bool) -> Option<Vec<String>> {
    (!agent_skills).then(|| skills.iter().map(|s| s.name.clone()).collect())
}

/// Writes Pi's agent directory for the run's models, the run's own first
/// and each role's model once with its own window and inputs (ADR 0011),
/// keeping what the user's `models` file (`user_models`) has for them.
pub(super) fn write_pi_agent(
    agent_dir: &Path,
    config: &ModelConfig,
    base_url: &str,
    user_models: Option<&Path>,
) -> anyhow::Result<tenaz_pi::models::AgentModels> {
    let models = pi_models(config, base_url);
    let template = user_models.and_then(|path| pi_template(path, &models));
    tenaz_pi::write_agent_dir(agent_dir, &models, template)
        .context("cannot write Pi's agent directory")?;
    Ok(models)
}

/// The run's models as Pi's `models.json` describes them, on the run's
/// provider at `base_url`.
fn pi_models(config: &ModelConfig, base_url: &str) -> tenaz_pi::models::AgentModels {
    let reasoning = reasoning_effort(config.thinking.as_deref()).is_some();
    let model = |id: &str, context_window, input: &[String]| tenaz_pi::models::AgentModel {
        id: id.to_owned(),
        context_window,
        input: input.to_vec(),
        reasoning,
    };
    let mut others: Vec<tenaz_pi::models::AgentModel> = Vec::new();
    for role in config.role_models.values() {
        if role.id != config.id && others.iter().all(|m| m.id != role.id) {
            others.push(model(&role.id, role.context_window, &role.input));
        }
    }
    tenaz_pi::models::AgentModels {
        provider: config
            .provider
            .clone()
            .or_else(|| config.model.split_once('/').map(|(p, _)| p.to_owned()))
            .unwrap_or_else(|| "engine".to_owned()),
        base_url: base_url.to_owned(),
        run: model(&config.id, config.context_window, &config.input),
        others,
    }
}

/// The user's own settings for `models`, when their Pi `models` file at
/// `path` lists them; the file is optional, so a broken one is only logged.
fn pi_template(
    path: &Path,
    models: &tenaz_pi::models::AgentModels,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    tenaz_pi::models::provider_template(path, &models.provider, &models.ids())
        .inspect_err(|error| tracing::warn!(%error, "ignoring Pi's models.json"))
        .ok()
        .flatten()
}

/// Puts the pinned skills in `dir`, an agent's own skill folder in its
/// isolated `HOME`, as links to `.tenaz/skills/<name>`. Links from an
/// earlier run are replaced.
fn link_skills(project: &Project, dir: &Path, skills: &[SkillPin]) -> anyhow::Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).context("cannot clear the agent's skill folder")?;
    }
    if skills.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).context("cannot create the agent's skill folder")?;
    for skill in skills {
        let target = project.tenaz_dir().join("skills").join(&skill.name);
        let target = std::path::absolute(&target).unwrap_or(target);
        std::os::unix::fs::symlink(&target, dir.join(&skill.name))
            .with_context(|| format!("cannot link the skill {}", skill.name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(name: &str) -> SkillPin {
        SkillPin {
            name: name.to_owned(),
            source: String::new(),
            sha256: String::new(),
            files: 1,
        }
    }

    /// Builds the harness `kind` names for a project in `dir`, with the
    /// skills `skills`.
    fn build(dir: &Path, kind: HarnessKind, skills: &[SkillPin]) -> anyhow::Result<()> {
        let extras = Extras {
            skills,
            ..Extras::default()
        };
        build_with(dir, kind, &extras).map(|_| ())
    }

    /// What a test gives the harness besides its kind: skills, MCP
    /// servers, the sandbox launcher (`wrapper`) and the user's caches.
    #[derive(Default)]
    struct Extras<'a> {
        skills: &'a [SkillPin],
        mcp_endpoints: &'a [McpEndpoint],
        wrapper: &'a [String],
        caches: &'a [(String, String)],
    }

    /// The same, with `extras`.
    fn build_with(
        dir: &Path,
        kind: HarnessKind,
        extras: &Extras<'_>,
    ) -> anyhow::Result<Arc<dyn Harness>> {
        let config = ModelConfig {
            harness: kind,
            harness_version: "1".into(),
            model: "engine/m".into(),
            provider: Some("engine".into()),
            id: "m".into(),
            base_url: None,
            context_window: 65_536,
            input: vec!["text".into()],
            thinking: None,
            role_models: std::collections::BTreeMap::new(),
        };
        let options = HarnessOptions {
            // A path that cannot exist, so no test starts a real program.
            bin: dir.join("no-such-agent"),
            fx_args: Vec::new(),
            acp_args: Vec::new(),
        };
        let (feed, _) = tokio::sync::broadcast::channel(1);
        build_harness(&HarnessInputs {
            project: &Project::new(dir),
            config: &config,
            options: &options,
            media_timeout: Duration::from_secs(60),
            base_url: "http://127.0.0.1:4001/v1".into(),
            mcp_endpoints: extras.mcp_endpoints,
            skills: extras.skills,
            agent_skills: false,
            verify_path: ".tenaz/verify",
            feed: &feed,
            wrapper: extras.wrapper,
            caches: extras.caches,
        })
    }

    /// A builder session's spec for a project in `dir`.
    fn session_spec(dir: &Path) -> tenaz_core::ports::SessionSpec {
        tenaz_core::ports::SessionSpec {
            label: "001-builder-T001".into(),
            cwd: dir.to_path_buf(),
            persist_dir: None,
            raw_log: None,
            thinking_log: None,
            stderr_log: None,
            model: tenaz_core::ports::SessionModel {
                name: "engine/m".into(),
                id: "m".into(),
                context_window: 65_536,
                supports_images: false,
            },
        }
    }

    /// The program the wrapper starts for `kind` in a project in `dir`, or
    /// `None` for a harness that is not an ACP agent. A new harness kind
    /// must answer here, and be added to the list in the test below.
    fn acp_program(kind: HarnessKind, dir: &Path) -> Option<String> {
        match kind {
            // fx starts through Tenaz's own shim.
            HarnessKind::Fx => Some(std::env::current_exe().unwrap().display().to_string()),
            HarnessKind::OpenCode | HarnessKind::Qwen | HarnessKind::Dsh | HarnessKind::Acp => {
                Some(dir.join("no-such-agent").display().to_string())
            }
            HarnessKind::Pi | HarnessKind::Fake => None,
        }
    }

    /// Every ACP agent the wiring builds starts through the run's wrapper,
    /// so a sandboxed run never starts one outside the sandbox.
    #[cfg(unix)]
    #[tokio::test]
    async fn every_acp_agent_starts_through_the_wrapper() {
        // Every kind, so that acp_program decides for each one.
        let kinds = [
            HarnessKind::Fx,
            HarnessKind::Pi,
            HarnessKind::OpenCode,
            HarnessKind::Qwen,
            HarnessKind::Dsh,
            HarnessKind::Acp,
            HarnessKind::Fake,
        ];
        for kind in kinds {
            let dir = tempfile::tempdir().unwrap();
            let Some(program) = acp_program(kind, dir.path()) else {
                continue;
            };
            let argv = dir.path().join("argv");
            let recorder = dir.path().join("record");
            tenaz_adapters::testing::write_executable(
                &recorder,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 1\n",
                    argv.display()
                ),
            );
            let wrapper = [recorder.display().to_string(), "--sandboxed".into()];
            let extras = Extras {
                wrapper: &wrapper,
                ..Extras::default()
            };
            let harness = build_with(dir.path(), kind, &extras).unwrap();
            let spec = session_spec(dir.path());
            let opened = tokio::time::timeout(Duration::from_secs(10), harness.open(spec))
                .await
                .expect("the launch ends");
            assert!(opened.is_err(), "{kind}: the recorder is not an ACP agent");
            let args = std::fs::read_to_string(&argv)
                .unwrap_or_else(|e| panic!("{kind}: the wrapper did not run: {e}"));
            let args: Vec<&str> = args.lines().take(2).collect();
            assert_eq!(args, ["--sandboxed", program.as_str()], "{kind}");
        }
    }

    /// dsh starts with the user's caches it is given, so what it installs
    /// there is where `verify`, with the user's own `HOME`, finds it.
    #[cfg(unix)]
    #[tokio::test]
    async fn dsh_starts_with_the_user_s_caches() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("env");
        let recorder = dir.path().join("record");
        tenaz_adapters::testing::write_executable(
            &recorder,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$PLAYWRIGHT_BROWSERS_PATH\" \"$npm_config_cache\" > '{}'\nexit 1\n",
                seen.display()
            ),
        );
        let caches = [
            (
                "PLAYWRIGHT_BROWSERS_PATH".to_owned(),
                "/u/browsers".to_owned(),
            ),
            ("npm_config_cache".to_owned(), "/u/npm".to_owned()),
        ];
        let wrapper = [recorder.display().to_string()];
        let extras = Extras {
            wrapper: &wrapper,
            caches: &caches,
            ..Extras::default()
        };
        let harness = build_with(dir.path(), HarnessKind::Dsh, &extras).unwrap();
        let spec = session_spec(dir.path());
        let opened = tokio::time::timeout(Duration::from_secs(10), harness.open(spec))
            .await
            .expect("the launch ends");
        assert!(opened.is_err(), "the recorder is not an ACP agent");
        let env = std::fs::read_to_string(&seen).expect("the wrapper ran");
        assert_eq!(env.lines().collect::<Vec<_>>(), ["/u/browsers", "/u/npm"]);
    }

    fn json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn opencode_is_set_up_in_its_home_before_its_first_session() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), HarnessKind::OpenCode, &[pin("rust")]).unwrap();
        let home = Project::new(dir.path()).private_dir().join("opencode-home");
        assert_eq!(json(&home.join("opencode.json"))["model"], "engine/m");
        assert!(home.join(".agents/skills/rust").is_symlink());
    }

    #[test]
    fn fx_is_set_up_in_its_home_before_its_first_session() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), HarnessKind::Fx, &[pin("rust")]).unwrap();
        let home = Project::new(dir.path()).private_dir().join("fx-home");
        assert_eq!(
            json(&home.join(".fx/settings.json"))["models"]["engine"],
            "m"
        );
        assert!(home.join(".fx/mcp.json").exists());
        assert!(home.join(".agents/skills/rust").is_symlink());
    }

    #[test]
    fn qwen_code_is_set_up_in_its_home_before_its_first_session() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), HarnessKind::Qwen, &[pin("rust")]).unwrap();
        let home = Project::new(dir.path()).private_dir().join("qwen-home");
        let settings = json(&home.join(".qwen/settings.json"));
        assert_eq!(
            settings["model"]["generationConfig"]["contextWindowSize"],
            65_536
        );
        assert!(home.join(".qwen/skills/rust").is_symlink());
    }

    #[test]
    fn dsh_is_set_up_in_its_home_before_its_first_session() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), HarnessKind::Dsh, &[]).unwrap();
        let home = Project::new(dir.path()).private_dir().join("dsh-home");
        let patch = json(&home.join(".dsh/tenaz-patch.yml"));
        let entry = |id: &str| {
            patch
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(entry("acp")["config"]["model"], "m");
        assert_eq!(
            entry("llm-pi-ai")["config"]["providers"]["tenaz"]["baseURL"],
            "http://127.0.0.1:4001/v1"
        );
    }

    #[test]
    fn the_generic_agent_is_told_its_first_session_before_it_starts() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), HarnessKind::Acp, &[]).unwrap();
        let file = Project::new(dir.path())
            .private_dir()
            .join("acp-session.json");
        let session = json(&file);
        assert_eq!(session["model"], "m");
        assert_eq!(session["baseUrl"], "http://127.0.0.1:4001/v1");
    }

    /// A server of the user's own reaches each agent's configuration with
    /// the media time limit, as the media server does (ADR 0032).
    #[test]
    fn the_run_s_mcp_servers_reach_each_agent_s_configuration() {
        let endpoints = [McpEndpoint {
            name: "blender".into(),
            url: "http://127.0.0.1:9191/".into(),
        }];
        let extras = Extras {
            mcp_endpoints: &endpoints,
            ..Extras::default()
        };
        let configured = |kind, file: &str| {
            let dir = tempfile::tempdir().unwrap();
            build_with(dir.path(), kind, &extras).unwrap();
            json(&Project::new(dir.path()).private_dir().join(file))
        };
        let fx = configured(HarnessKind::Fx, "fx-home/.fx/mcp.json");
        assert_eq!(fx["mcp"]["blender"]["url"], "http://127.0.0.1:9191/");
        assert_eq!(fx["mcp"]["blender"]["operation_timeout_ms"], 60_000);
        let opencode = configured(HarnessKind::OpenCode, "opencode-home/opencode.json");
        assert_eq!(opencode["mcp"]["blender"]["url"], "http://127.0.0.1:9191/");
        let qwen = configured(HarnessKind::Qwen, "qwen-home/.qwen/settings.json");
        assert_eq!(
            qwen["mcpServers"]["blender"]["httpUrl"],
            "http://127.0.0.1:9191/"
        );
        let acp = configured(HarnessKind::Acp, "acp-session.json");
        assert_eq!(
            acp["mcpServers"],
            serde_json::json!([{ "name": "blender", "url": "http://127.0.0.1:9191/" }])
        );
    }

    #[test]
    fn a_configuration_that_cannot_be_written_stops_the_run_before_it_starts() {
        // A folder where the agent's configuration file goes: only that
        // write fails, and the skills are not linked after it.
        for (kind, file, skills, what) in [
            (
                HarnessKind::Fx,
                "fx-home/.fx/settings.json",
                "fx-home/.agents/skills",
                "the fx profile",
            ),
            (
                HarnessKind::OpenCode,
                "opencode-home/opencode.json",
                "opencode-home/.agents/skills",
                "the OpenCode configuration",
            ),
            (
                HarnessKind::Qwen,
                "qwen-home/.qwen/settings.json",
                "qwen-home/.qwen/skills",
                "the Qwen Code settings",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let private = Project::new(dir.path()).private_dir();
            std::fs::create_dir_all(private.join(file)).unwrap();
            let error = build(dir.path(), kind, &[pin("rust")]).unwrap_err();
            let prefix = format!("cannot write {what}: ");
            assert!(error.to_string().starts_with(&prefix), "{error}");
            assert!(!private.join(skills).exists(), "{kind:?} linked its skills");
        }
    }

    #[test]
    fn opencode_may_load_only_the_pinned_skills_unless_every_skill() {
        let pinned = [pin("rust")];
        assert_eq!(
            allowed_skills(&pinned, false),
            Some(vec!["rust".to_owned()])
        );
        assert_eq!(allowed_skills(&[], false), Some(Vec::new()));
        assert_eq!(allowed_skills(&pinned, true), None);
    }

    #[test]
    fn the_pinned_skills_replace_those_of_an_earlier_run() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::new(dir.path());
        let skills = dir.path().join("home/.qwen/skills");
        std::fs::create_dir_all(skills.join("old")).unwrap();
        link_skills(&project, &skills, &[pin("rust")]).unwrap();
        let names: Vec<_> = std::fs::read_dir(&skills)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["rust"]);
        assert_eq!(
            std::fs::read_link(skills.join("rust")).unwrap(),
            std::path::absolute(project.tenaz_dir().join("skills/rust")).unwrap()
        );
    }
}
