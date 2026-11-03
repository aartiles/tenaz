//! The harness of a run, built from the ledger: each kind writes its own
//! configuration into the project (ADR 0018) and is launched through the
//! sandbox when the run has one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tenaz_acp::fx::{self, FxLaunch, FxSettings, write_profile};
use tenaz_acp::opencode::{self, OpenCodeConfig, OpenCodeLaunch};
use tenaz_acp::qwen::{self, QwenLaunch, QwenSettings};
use tenaz_acp::{EngineObservation, McpEndpoint, ProfileMcpServer};
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
    pub(super) media_timeout: &'a Duration,
    /// The proxy's base URL, or the engine's with `--no-proxy`.
    pub(super) base_url: String,
    pub(super) mcp_endpoints: &'a [McpEndpoint],
    pub(super) skills: &'a [SkillPin],
    pub(super) agent_skills: bool,
    pub(super) verify_path: &'a str,
    pub(super) feed: &'a tokio::sync::broadcast::Sender<EngineObservation>,
    /// The sandbox launcher, when the run is sandboxed.
    pub(super) wrapper: &'a [String],
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
}

/// The harness named in the ledger: fx, `OpenCode` or Qwen Code over ACP
/// with their configuration written into the project, Pi in RPC mode with
/// the pinned skills, or the scripted fake (writing `verify` where the run
/// expects it).
pub(super) fn build_harness(inputs: HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    match inputs.config.harness {
        // The scripted harness writes from inside Tenaz: there is no process to sandbox.
        HarnessKind::Fake => Ok(Arc::new(ScriptedHarness {
            verify_path: Some(inputs.verify_path.to_owned()),
            ..ScriptedHarness::every_recovery()
        })),
        HarnessKind::Pi => pi_harness(&inputs),
        HarnessKind::Fx => fx_harness(inputs),
        HarnessKind::OpenCode => opencode_harness(inputs),
        HarnessKind::Qwen => qwen_harness(inputs),
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
fn fx_harness(inputs: HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let HarnessInputs {
        project,
        config,
        skills,
        agent_skills,
        ..
    } = inputs;
    let home = inputs.home("fx-home");
    let profile_mcp = inputs.profile_mcp();
    let settings = FxSettings {
        provider: provider(config),
        base_url: inputs.base_url,
        model: config.id.clone(),
        context_window: config.context_window,
        max_output_tokens: HARNESS_MAX_OUTPUT_TOKENS,
        supports_vision: config.supports_images(),
        permission_mode: "ask".to_owned(),
        effort: reasoning_effort(config.thinking.as_deref()).map(str::to_owned),
        skill_catalog: agent_skills || !skills.is_empty(),
    };
    write_profile(&home, &settings, &profile_mcp).context("cannot write the fx profile")?;
    link_skills(project, &home.join(".agents/skills"), skills)?;
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
    let mut acp = launch
        .harness(config.harness_version.clone())
        .with_observations(inputs.feed.clone());
    if !inputs.mcp_endpoints.is_empty() {
        // Profile servers accept a long operation timeout; servers passed in session/new are cut at 60 s.
        acp = acp.with_session_meta(
            serde_json::json!({ "fx": { "profileMcpServers": true } })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
    }
    Ok(Arc::new(fx::configured(acp, &launch.home, settings)))
}

/// `OpenCode` with its configuration in `.tenaz/private/opencode-home`.
fn opencode_harness(inputs: HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let HarnessInputs {
        project,
        config,
        skills,
        ..
    } = inputs;
    let home = inputs.home("opencode-home");
    // OpenCode finds skills in `.agents/skills` of its home, as fx does.
    link_skills(project, &home.join(".agents/skills"), skills)?;
    let opencode = OpenCodeConfig {
        provider: provider(config),
        mcp: inputs.profile_mcp(),
        base_url: inputs.base_url,
        model: config.id.clone(),
        context_window: config.context_window,
        max_output_tokens: HARNESS_MAX_OUTPUT_TOKENS,
        supports_images: config.supports_images(),
        skills: allowed_skills(skills, inputs.agent_skills),
    };
    let mut launch = OpenCodeLaunch::new(&inputs.options.bin, &home);
    // A home that cannot be written stops the run here, not in its first session.
    opencode::write_config(&home, &opencode).context("cannot write the OpenCode configuration")?;
    // npm's own cache, which the sandbox lets the agent write by default;
    // a `sandboxAllowWrite` without `~/.npm` leaves OpenCode unable to start.
    launch.npm_cache = std::env::home_dir().map(|h| h.join(".npm"));
    launch.wrapper = inputs.wrapper.to_vec();
    let acp = launch
        .harness(config.harness_version.clone())
        .with_observations(inputs.feed.clone());
    Ok(Arc::new(opencode::configured(acp, home, opencode)))
}

/// Qwen Code with its settings in `.tenaz/private/qwen-home`.
fn qwen_harness(inputs: HarnessInputs<'_>) -> anyhow::Result<Arc<dyn Harness>> {
    let home = inputs.home("qwen-home");
    link_skills(inputs.project, &home.join(".qwen/skills"), inputs.skills)?;
    let settings = QwenSettings {
        context_window: inputs.config.context_window,
        mcp: inputs.profile_mcp(),
        discover_skills: inputs.agent_skills,
    };
    // A home that cannot be written stops the run here, not in its first session.
    qwen::write_settings(&home, &settings).context("cannot write the Qwen Code settings")?;
    let mut launch = QwenLaunch::new(&inputs.options.bin, &home, inputs.base_url);
    launch.wrapper = inputs.wrapper.to_vec();
    let acp = launch
        .harness(inputs.config.harness_version.clone())
        .with_observations(inputs.feed.clone());
    Ok(Arc::new(qwen::configured(acp, home, settings)))
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
    std::fs::create_dir_all(dir)?;
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
