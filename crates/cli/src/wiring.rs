//! Composition: builds the adapters, the lease, the proxy, the MCP server and
//! the harness for a run from the ledger and the command line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use tenaz_acp::fx::{FxLaunch, FxSettings, ProfileMcpServer, write_profile};
use tenaz_acp::{EngineObservation, McpEndpoint, stop_reason_for_finish};
use tenaz_adapters::clock::SystemClock;
use tenaz_adapters::engine::generic::{Commands, GenericEngine};
use tenaz_adapters::engine::sushi::SushiEngine;
use tenaz_adapters::git::GitRepo;
use tenaz_adapters::media::comfyui::ComfyUi;
use tenaz_adapters::metrics::PrometheusMetrics;
use tenaz_adapters::notify::DesktopNotifier;
use tenaz_adapters::probe_macos::MacMemoryProbe;
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::lease::{Gate, LeaseManager, LeaseTransition};
use tenaz_core::ledger::{JournalRecord, SkillPin};
use tenaz_core::orchestrator::Deps;
use tenaz_core::ports::{EngineAdapter, Harness, MediaAdapter};
use tenaz_core::project::Project;
use tenaz_core::session::Usage;
use tenaz_core::testing::ScriptedHarness;
use tenaz_mcp::workflow::WorkflowTemplate;
use tenaz_mcp::{MediaTools, serve};
use tenaz_pi::PiLaunch;
use tenaz_proxy::{Proxy, ProxyEvent};

pub enum EngineChoice {
    Sushi,
    Generic {
        start: Option<String>,
        stop: Option<String>,
    },
}

pub struct EngineOptions {
    pub kind: EngineChoice,
    pub proxy: bool,
}

pub struct HarnessOptions {
    pub fx_bin: PathBuf,
    pub fx_args: Vec<String>,
    pub pi_bin: PathBuf,
}

pub struct MediaOptions {
    pub comfyui: Option<String>,
    pub workflows: BTreeMap<String, WorkflowTemplate>,
    pub timeout: Duration,
}

/// Background services a run holds open.
#[derive(Default)]
pub struct Services {
    proxy: Option<tenaz_proxy::ProxyHandle>,
    mcp: Option<tenaz_mcp::McpHandle>,
}

impl Services {
    pub async fn stop(self) {
        if let Some(mcp) = self.mcp {
            mcp.stop().await;
        }
        if let Some(proxy) = self.proxy {
            proxy.stop().await;
        }
    }
}

pub struct Wired {
    pub deps: Deps,
    pub services: Services,
}

/// `name=path.json` pairs.
pub fn parse_workflows(specs: &[String]) -> anyhow::Result<BTreeMap<String, WorkflowTemplate>> {
    let mut out = BTreeMap::new();
    for spec in specs {
        let (name, path) = spec
            .split_once('=')
            .with_context(|| format!("--workflow expects name=path, got {spec}"))?;
        out.insert(
            name.to_owned(),
            WorkflowTemplate::from_file(std::path::Path::new(path))?,
        );
    }
    Ok(out)
}

/// The `reasoning_effort` the engine is asked for, from the ledger's
/// thinking level; `None` leaves the engine's default (`off`, `none`, unset).
fn reasoning_effort(thinking: Option<&str>) -> Option<&str> {
    match thinking.map(str::trim) {
        None | Some("" | "off" | "none" | "auto") => None,
        Some(level) => Some(level),
    }
}

fn split_command(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

fn origin(base_url: &str) -> anyhow::Result<String> {
    let url = reqwest_url(base_url)?;
    Ok(format!(
        "{}://{}{}",
        url.0,
        url.1,
        url.2.map(|p| format!(":{p}")).unwrap_or_default()
    ))
}

/// `(scheme, host, port)` of a URL, without pulling a URL crate into the binary.
fn reqwest_url(base_url: &str) -> anyhow::Result<(String, String, Option<u16>)> {
    let (scheme, rest) = base_url
        .split_once("://")
        .with_context(|| format!("not a URL: {base_url}"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_owned(), p.parse().ok()),
        _ => (authority.to_owned(), None),
    };
    Ok((scheme.to_owned(), host, port))
}

/// Builds everything a run needs.
pub async fn wire(
    project: &Project,
    engine: &EngineOptions,
    harness: &HarnessOptions,
    media: &MediaOptions,
) -> anyhow::Result<Wired> {
    let ledger = project
        .load_ledger()
        .context("No .tenaz/run.json; run init first")?;
    let config = &ledger.config;
    let base_url = config
        .base_url
        .clone()
        .context("the ledger has no engine base URL")?;
    let engine_adapter: Arc<dyn EngineAdapter> = match &engine.kind {
        EngineChoice::Sushi => Arc::new(SushiEngine::new(&base_url)),
        EngineChoice::Generic { start, stop } => {
            let commands = Commands {
                start: start.as_deref().map(split_command),
                stop: stop.as_deref().map(split_command),
            };
            Arc::new(GenericEngine::new(
                "generic",
                &base_url,
                commands,
                Duration::from_mins(20),
            ))
        }
    };
    let gate = Gate::new();
    let journal_project = project.clone();
    let clock_for_lease = SystemClock::new();
    let observer: tenaz_core::lease::Observer = Arc::new(move |t: LeaseTransition| {
        use tenaz_core::ports::Clock as _;
        let _ = journal_project.append_journal(
            clock_for_lease.now_rfc3339(),
            JournalRecord::Lease {
                holder: t.holder.to_string(),
                transition: t.transition,
                ms: t.ms,
                detail: t.detail,
            },
        );
    });
    let mut lease = LeaseManager::new(
        engine_adapter.clone(),
        config.id.clone(),
        gate.clone(),
        observer,
    );
    let mut services = Services::default();
    let mut mcp_endpoints = Vec::new();
    if let Some(comfy_url) = &media.comfyui {
        if media.workflows.is_empty() {
            bail!("--comfyui needs at least one --workflow name=path.json");
        }
        let comfy: Arc<dyn MediaAdapter> = Arc::new(ComfyUi::new(comfy_url, media.timeout));
        lease = lease.with_media(comfy);
    }
    let lease = Arc::new(lease);
    if media.comfyui.is_some() {
        let tools = MediaTools::new(
            lease.clone(),
            project.private_dir().join("media"),
            media.workflows.clone(),
            "comfyui",
        );
        let handle = serve(tools, "127.0.0.1:0".parse()?)
            .await
            .context("cannot start the media MCP server")?;
        tracing::info!("media MCP server at {}", handle.url());
        mcp_endpoints.push(McpEndpoint {
            name: "media".to_owned(),
            url: handle.url(),
        });
        services.mcp = Some(handle);
    }
    // The proxy sits between the harness and the engine; the harness is pointed at it.
    let (feed, _) = tokio::sync::broadcast::channel::<EngineObservation>(256);
    let harness_base_url = if engine.proxy {
        let handle = start_proxy(
            &base_url,
            gate.clone(),
            feed.clone(),
            config.thinking.as_deref(),
            ledger.params.repair_tool_args.then(|| {
                project
                    .private_dir()
                    .join(tenaz_core::orchestrator::TOOL_REPAIRS)
            }),
        )
        .await?;
        let url = handle.base_url();
        services.proxy = Some(handle);
        url
    } else {
        base_url.clone()
    };
    let harness = build_harness(
        project,
        config,
        harness,
        &media.timeout,
        harness_base_url,
        &mcp_endpoints,
        &ledger.skills,
        ledger.params.agent_skills,
        ledger.params.verify_path(),
        &feed,
    )?;
    let port = reqwest_url(&base_url)?.2;
    let deps = Deps {
        harness,
        vcs: Arc::new(GitRepo::new(project.root())),
        verify: Arc::new(VerifyProcess::new(project.root()).at(ledger.params.verify_path())),
        clock: Arc::new(SystemClock::new()),
        notifier: Arc::new(DesktopNotifier::new(
            project
                .root()
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )),
        engine: (config.harness != "fake").then_some(engine_adapter),
        lease: (config.harness != "fake").then_some(lease),
        engine_metrics: PrometheusMetrics::from_base_url(&base_url)
            .map(|m| Arc::new(m) as Arc<dyn tenaz_core::ports::EngineMetricsSource>),
        memory: cfg!(target_os = "macos").then(|| {
            Arc::new(MacMemoryProbe::new(port)) as Arc<dyn tenaz_core::ports::MemoryProbe>
        }),
    };
    Ok(Wired { deps, services })
}

/// Appends one repair to `log`, as a JSON line.
fn log_repair(log: &Path, request: u64, tool: &str, fields: &[String]) {
    use tenaz_core::ports::Clock as _;
    tracing::info!(
        tool,
        ?fields,
        "repaired tool-call arguments the engine sent as text"
    );
    let line = serde_json::json!({
        "at": tenaz_adapters::clock::SystemClock::new().now_rfc3339(),
        "request": request,
        "tool": tool,
        "fields": fields,
    });
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()));
    if let Err(error) = written {
        tracing::warn!(%error, "cannot record a tool-call repair");
    }
}

/// The engine proxy: per-request usage and errors go to `feed`, requests
/// are given the `reasoning_effort` the thinking level asks for, and with a
/// `repairs` log, tool-call arguments sent as text are repaired and logged.
async fn start_proxy(
    base_url: &str,
    gate: Gate,
    feed: tokio::sync::broadcast::Sender<EngineObservation>,
    thinking: Option<&str>,
    repairs: Option<PathBuf>,
) -> anyhow::Result<tenaz_proxy::ProxyHandle> {
    let repair_log = repairs.clone();
    let mut proxy = Proxy::new(
        origin(base_url)?,
        gate,
        Arc::new(move |event: ProxyEvent| match event {
            ProxyEvent::Completed {
                usage: Some(u),
                finish_reason,
                timings,
                ..
            } => {
                let _ = feed.send(EngineObservation::Completed {
                    usage: Usage {
                        input: u.prompt_tokens.saturating_sub(u.cached_tokens),
                        output: u.completion_tokens,
                        cache_read: u.cached_tokens,
                        cache_write: 0,
                        reasoning: u.reasoning_tokens,
                    },
                    stop_reason: stop_reason_for_finish(finish_reason.as_deref()),
                    server: timings,
                });
            }
            ProxyEvent::EngineError { message, .. } => {
                let _ = feed.send(EngineObservation::Error { message });
            }
            ProxyEvent::ToolArgsRepaired {
                request,
                tool,
                fields,
            } => {
                if let Some(log) = &repair_log {
                    log_repair(log, request, &tool, &fields);
                }
            }
            ProxyEvent::Completed { .. } | ProxyEvent::Held { .. } => {}
        }),
    );
    proxy.repair_tool_args = repairs.is_some();
    if let Some(effort) = reasoning_effort(thinking) {
        // fx does not forward its effort to a custom provider; the proxy fills it in.
        proxy.request_defaults.insert(
            "reasoning_effort".to_owned(),
            serde_json::Value::String(effort.to_owned()),
        );
    }
    let handle = Arc::new(proxy)
        .start("127.0.0.1:0".parse()?)
        .await
        .context("cannot start the engine proxy")?;
    tracing::info!("engine proxy at {} -> {}", handle.base_url(), base_url);
    Ok(handle)
}

/// The harness named in the ledger: fx over ACP with its profile written
/// into the project, Pi in RPC mode with the pinned skills, or the scripted
/// fake (writing `verify` where the run expects it).
#[allow(clippy::too_many_arguments)]
fn build_harness(
    project: &Project,
    config: &tenaz_core::ledger::ModelConfig,
    options: &HarnessOptions,
    media_timeout: &Duration,
    harness_base_url: String,
    mcp_endpoints: &[McpEndpoint],
    skills: &[SkillPin],
    agent_skills: bool,
    verify_path: &str,
    feed: &tokio::sync::broadcast::Sender<EngineObservation>,
) -> anyhow::Result<Arc<dyn Harness>> {
    match config.harness.as_str() {
        "fake" => Ok(Arc::new(ScriptedHarness {
            verify_path: Some(verify_path.to_owned()),
            ..ScriptedHarness::default()
        })),
        "pi" => {
            // Pi gets an agent directory of its own pointing it at the proxy
            // (or the engine); the media MCP server is not on its path.
            let agent_dir = project.private_dir().join("pi-agent");
            let provider = config
                .provider
                .clone()
                .or_else(|| config.model.split_once('/').map(|(p, _)| p.to_owned()))
                .unwrap_or_else(|| "engine".to_owned());
            let model = tenaz_pi::models::AgentModel {
                provider: provider.clone(),
                id: config.id.clone(),
                base_url: harness_base_url,
                context_window: config.context_window,
                input: config.input.clone(),
                reasoning: reasoning_effort(config.thinking.as_deref()).is_some(),
            };
            tenaz_pi::write_agent_dir(&agent_dir, &model, pi_template(&provider, &config.id))
                .context("cannot write Pi's agent directory")?;
            let launch = PiLaunch::new(&options.pi_bin, format!("{provider}/{}", config.id))
                .thinking(
                    config
                        .thinking
                        .clone()
                        .unwrap_or_else(|| "medium".to_owned()),
                )
                .skills(
                    skills
                        .iter()
                        .map(|s| project.tenaz_dir().join("skills").join(&s.name))
                        .collect(),
                )
                .agent_dir(&agent_dir)
                .discover_skills(agent_skills);
            Ok(Arc::new(launch.harness(config.harness_version.clone())))
        }
        "fx" => {
            let home = project.private_dir().join("fx-home");
            let settings = FxSettings {
                provider: config
                    .provider
                    .clone()
                    .unwrap_or_else(|| "engine".to_owned()),
                base_url: harness_base_url,
                model: config.id.clone(),
                context_window: config.context_window,
                max_output_tokens: 16_384,
                supports_vision: config.supports_images(),
                permission_mode: "ask".to_owned(),
                effort: reasoning_effort(config.thinking.as_deref()).map(str::to_owned),
                skill_catalog: agent_skills || !skills.is_empty(),
            };
            let profile_mcp: Vec<ProfileMcpServer> = mcp_endpoints
                .iter()
                .map(|e| ProfileMcpServer {
                    name: e.name.clone(),
                    url: e.url.clone(),
                    operation_timeout_ms: u64::try_from(media_timeout.as_millis())
                        .unwrap_or(u64::MAX),
                })
                .collect();
            write_profile(&home, &settings, &profile_mcp).context("cannot write the fx profile")?;
            link_fx_skills(project, &home, skills)?;
            if !skills.is_empty() && !agent_skills {
                tracing::warn!(
                    "fx lists the pinned skills, and with them any skills it finds in the folders \
                     above the project; Pi loads only the pinned ones"
                );
            }
            let mut launch = FxLaunch::new(&options.fx_bin, &home);
            launch.args.clone_from(&options.fx_args);
            launch.log_dir = Some(project.private_dir());
            let mut acp = launch
                .harness(config.harness_version.clone())
                .with_observations(feed.clone());
            if !mcp_endpoints.is_empty() {
                // Profile servers accept a long operation timeout; servers passed in session/new are cut at 60 s.
                acp = acp.with_session_meta(
                    serde_json::json!({ "fx": { "profileMcpServers": true } })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            Ok(Arc::new(acp))
        }
        other => bail!("unknown harness {other} in the ledger"),
    }
}

/// The user's own settings for the model, when `~/.pi/agent/models.json`
/// lists it; the file is optional, so a broken one is only logged.
fn pi_template(provider: &str, id: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let path = tenaz_pi::models::default_models_path()?;
    tenaz_pi::models::provider_template(&path, provider, id)
        .inspect_err(|error| tracing::warn!(%error, "ignoring Pi's models.json"))
        .ok()
        .flatten()
}

/// Puts the pinned skills where fx looks for its own: `.agents/skills/` in
/// its isolated `HOME`, as links to `.tenaz/skills/<name>`. Links from an
/// earlier run are replaced.
fn link_fx_skills(project: &Project, home: &Path, skills: &[SkillPin]) -> anyhow::Result<()> {
    let dir = home.join(".agents/skills");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("cannot clear fx's skill folder")?;
    }
    if skills.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    for skill in skills {
        let target = project.tenaz_dir().join("skills").join(&skill.name);
        let target = std::path::absolute(&target).unwrap_or(target);
        std::os::unix::fs::symlink(&target, dir.join(&skill.name))
            .with_context(|| format!("cannot link the skill {} for fx", skill.name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reasoning_effort;

    #[test]
    fn thinking_level_becomes_a_reasoning_effort_unless_off() {
        assert_eq!(reasoning_effort(Some("medium")), Some("medium"));
        assert_eq!(reasoning_effort(Some(" high ")), Some("high"));
        assert_eq!(reasoning_effort(Some("off")), None);
        assert_eq!(reasoning_effort(Some("auto")), None);
        assert_eq!(reasoning_effort(None), None);
    }
}
