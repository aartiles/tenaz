//! Composition: builds the adapters, the lease, the proxy, the MCP server and
//! the harness for a run from the ledger and the command line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use tenaz_acp::{EngineObservation, McpEndpoint, stop_reason_for_finish};
use tenaz_adapters::clock::SystemClock;
use tenaz_adapters::engine::generic::{Commands, GenericEngine};
use tenaz_adapters::engine::load_api::LoadApiEngine;
use tenaz_adapters::git::GitRepo;
use tenaz_adapters::job::JobProcess;
use tenaz_adapters::media::comfyui::ComfyUi;
use tenaz_adapters::media::meshes::MeshApi;
use tenaz_adapters::media::openai_images::OpenAiImages;
use tenaz_adapters::metrics::PrometheusMetrics;
use tenaz_adapters::notify::DesktopNotifier;
use tenaz_adapters::probe_macos::MacMemoryProbe;
use tenaz_adapters::sandbox::{Guard, GuardedHarness, GuardedJobs, GuardedVerify, Policy};
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::cache_report::CacheReport;
use tenaz_core::effort::EffortDial;
use tenaz_core::lease::{Gate, LeaseManager, LeaseTransition};
use tenaz_core::ledger::{HarnessKind, JournalRecord, ModelConfig};
use tenaz_core::mcp_servers::MEDIA_SERVER;
use tenaz_core::orchestrator::Deps;
use tenaz_core::params::RunParams;
use tenaz_core::ports::{EngineAdapter, MediaAdapter};
use tenaz_core::project::Project;
use tenaz_core::session::Usage;
use tenaz_core::write_scope::WriteScope;
use tenaz_mcp::workflow::WorkflowTemplate;
use tenaz_mcp::{MediaTools, serve};
use tenaz_proxy::{Proxy, ProxyEvent};

use harness::{HarnessInputs, build_harness, user_caches};

mod harness;

pub enum EngineChoice {
    LoadApi,
    Generic {
        start: Option<String>,
        stop: Option<String>,
    },
}

/// The lease over the run's models on `adapter`, its transitions
/// journaled, before media and swapping are added.
fn lease_over(
    project: &Project,
    adapter: Arc<dyn EngineAdapter>,
    config: &ModelConfig,
    gate: &Gate,
) -> LeaseManager {
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
    LeaseManager::new(adapter, config.model_ids(), gate.clone(), observer)
}

/// `lease`, keeping one model loaded at a time when `swap` asks; an engine
/// or a run that cannot swap is refused rather than run with every model
/// loaded.
fn swapping(lease: LeaseManager, swap: bool) -> anyhow::Result<LeaseManager> {
    if !swap {
        return Ok(lease);
    }
    lease.with_swap().map_err(|why| {
        anyhow::anyhow!("--swap-models: {why}; tenaz run --no-swap-models turns swapping off")
    })
}

pub struct EngineOptions {
    pub kind: EngineChoice,
    pub proxy: bool,
    /// Keep one of the run's models loaded at a time (ADR 0015).
    pub swap: bool,
}

pub struct HarnessOptions {
    /// The executable of the run's harness.
    pub bin: PathBuf,
    /// The arguments that put fx in ACP mode; only fx reads them.
    pub fx_args: Vec<String>,
    /// The arguments that put the generic agent in ACP mode.
    pub acp_args: Vec<String>,
}

pub struct MediaOptions {
    pub comfyui: Option<String>,
    pub workflows: BTreeMap<String, WorkflowTemplate>,
    /// An `OpenAI` Images API base URL and the model to ask it for.
    pub images_url: Option<String>,
    pub image_model: Option<String>,
    /// An image-to-3D API base URL and the model to ask it for (ADR 0020).
    pub meshes_url: Option<String>,
    pub mesh_model: Option<String>,
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

/// The run's image engine, if it has one: `ComfyUI` with its workflows, or
/// an `OpenAI` Images API, which shares the LLM's engine when it has the same
/// origin as `llm_base_url`.
fn media_adapter(
    media: &MediaOptions,
    llm_base_url: &str,
) -> anyhow::Result<Option<Arc<dyn MediaAdapter>>> {
    match (&media.comfyui, &media.images_url) {
        (Some(_), Some(_)) => {
            bail!("--comfyui and --images each give the run an image engine; keep one")
        }
        (Some(comfy_url), None) => {
            if media.workflows.is_empty() {
                bail!("--comfyui needs at least one --workflow name=path.json");
            }
            Ok(Some(Arc::new(ComfyUi::new(comfy_url, media.timeout))))
        }
        (None, Some(images_url)) => {
            let model = media
                .image_model
                .clone()
                .context("--images needs --image-model, the image model to ask it for")?;
            let shares = origin(images_url)? == origin(llm_base_url)?;
            Ok(Some(Arc::new(
                OpenAiImages::new(images_url, model, media.timeout).sharing_the_engine(shares),
            )))
        }
        (None, None) => Ok(None),
    }
}

/// The run's 3D engine, if it has one; it shares the LLM's engine when it
/// has the same origin as `llm_base_url`.
fn mesh_adapter(
    media: &MediaOptions,
    llm_base_url: &str,
) -> anyhow::Result<Option<Arc<dyn MediaAdapter>>> {
    let Some(meshes_url) = &media.meshes_url else {
        return Ok(None);
    };
    let model = media
        .mesh_model
        .clone()
        .context("--meshes needs --mesh-model, the 3D model to ask it for")?;
    let shares = origin(meshes_url)? == origin(llm_base_url)?;
    Ok(Some(Arc::new(
        MeshApi::new(meshes_url, model, media.timeout).sharing_the_engine(shares),
    )))
}

/// The `reasoning_effort` the engine is asked for, from the ledger's
/// thinking level; `None` leaves the engine's default (`off`, `none`, unset).
fn reasoning_effort(thinking: Option<&str>) -> Option<&str> {
    match thinking.map(str::trim) {
        None | Some("" | "off" | "none" | "auto") => None,
        Some(level) => Some(level),
    }
}

/// The fields the proxy adds to completion requests that lack them: the
/// `reasoning_effort` of the thinking level (fx does not forward its effort
/// to a custom provider) and the run's pinned `temperature`.
fn request_defaults(
    thinking: Option<&str>,
    temperature: Option<f64>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut defaults = serde_json::Map::new();
    if let Some(effort) = reasoning_effort(thinking) {
        defaults.insert("reasoning_effort".to_owned(), effort.into());
    }
    if let Some(temperature) = temperature {
        defaults.insert("temperature".to_owned(), temperature.into());
    }
    defaults
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

/// The run's media engines: an image engine, a 3D engine, both or neither.
struct MediaEngines {
    image: Option<Arc<dyn MediaAdapter>>,
    mesh: Option<Arc<dyn MediaAdapter>>,
}

/// The media MCP server with the tools of the run's engines; `None` when
/// it has none.
async fn media_server(
    lease: &Arc<LeaseManager>,
    project: &Project,
    media: &MediaOptions,
    engines: &MediaEngines,
) -> anyhow::Result<Option<tenaz_mcp::McpHandle>> {
    if engines.image.is_none() && engines.mesh.is_none() {
        return Ok(None);
    }
    let mut tools = MediaTools::new(lease.clone(), project.private_dir().join("media"));
    if let Some(adapter) = &engines.image {
        tools = tools.with_images(adapter.name(), media.workflows.clone());
    }
    if let Some(adapter) = &engines.mesh {
        tools = tools.with_meshes(adapter.name(), project.root());
    }
    let handle = serve(tools, "127.0.0.1:0".parse()?)
        .await
        .context("cannot start the media MCP server")?;
    tracing::info!("media MCP server at {}", handle.url());
    Ok(Some(handle))
}

/// The MCP servers every session is offered: Tenaz's media server when the
/// run has one, then the user's own by name (ADR 0032).
fn mcp_endpoints(media_url: Option<String>, own: &BTreeMap<String, String>) -> Vec<McpEndpoint> {
    let media = media_url.map(|url| McpEndpoint {
        name: MEDIA_SERVER.to_owned(),
        url,
    });
    media
        .into_iter()
        .chain(own.iter().map(|(name, url)| McpEndpoint {
            name: name.clone(),
            url: url.clone(),
        }))
        .collect()
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
        EngineChoice::LoadApi => Arc::new(LoadApiEngine::new(&base_url)),
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
    let lease = lease_over(project, engine_adapter.clone(), config, &gate);
    let mut lease = swapping(lease, engine.swap)?;
    let mut services = Services::default();
    let engines = MediaEngines {
        image: media_adapter(media, &base_url)?,
        mesh: mesh_adapter(media, &base_url)?,
    };
    for adapter in engines.image.iter().chain(&engines.mesh) {
        lease = lease.with_media(adapter.clone());
    }
    let lease = Arc::new(lease);
    services.mcp = media_server(&lease, project, media, &engines).await?;
    let mcp_endpoints = mcp_endpoints(
        services.mcp.as_ref().map(tenaz_mcp::McpHandle::url),
        &ledger.launch.mcp_servers,
    );
    // The proxy sits between the harness and the engine; the harness is pointed at it.
    let (feed, _) = tokio::sync::broadcast::channel::<EngineObservation>(256);
    let effort = engine.proxy.then(EffortDial::new);
    let cache_report = engine.proxy.then(CacheReport::new);
    let harness_base_url = if let Some(report) = &cache_report {
        let handle = start_proxy(
            &base_url,
            gate.clone(),
            feed.clone(),
            report.clone(),
            effort.clone(),
            project,
            &ledger,
        )
        .await?;
        let url = handle.base_url();
        services.proxy = Some(handle);
        url
    } else {
        base_url.clone()
    };
    let sandbox = sandbox(project, &ledger.params)?;
    let wrapper = sandbox
        .as_ref()
        .map_or_else(Vec::new, |s| s.wrapper.clone());
    let mut harness = build_harness(&HarnessInputs {
        project,
        config,
        options: harness,
        media_timeout: media.timeout,
        base_url: harness_base_url,
        mcp_endpoints: &mcp_endpoints,
        skills: &ledger.skills,
        agent_skills: ledger.params.agent_skills,
        verify_path: ledger.params.verify_path(),
        feed: &feed,
        wrapper: &wrapper,
        caches: &user_caches(),
    })?;
    let (verify, jobs) = runners(project, &ledger.params, sandbox.as_ref());
    if let Some(sandbox) = sandbox {
        harness = Arc::new(GuardedHarness::new(harness, sandbox.guard));
    }
    let port = reqwest_url(&base_url)?.2;
    let deps = Deps {
        harness,
        vcs: Arc::new(GitRepo::new(project.root())),
        verify,
        jobs,
        clock: Arc::new(SystemClock::new()),
        notifier: Arc::new(notifier(project)),
        engine: (config.harness != HarnessKind::Fake).then_some(engine_adapter),
        lease: (config.harness != HarnessKind::Fake).then_some(lease),
        engine_metrics: PrometheusMetrics::from_base_url(&base_url)
            .map(|m| Arc::new(m) as Arc<dyn tenaz_core::ports::EngineMetricsSource>),
        memory: cfg!(target_os = "macos").then(|| {
            Arc::new(MacMemoryProbe::new(port)) as Arc<dyn tenaz_core::ports::MemoryProbe>
        }),
        effort,
        cache_report,
        write_scope: outside_write_scope(project, &ledger.params),
    };
    Ok(Wired { deps, services })
}

/// The desktop notifier, titled with the folder that holds the project.
fn notifier(project: &Project) -> DesktopNotifier {
    DesktopNotifier::new(
        project
            .root()
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

/// Where the sessions of a run without the sandbox may write; `None` in a
/// sandboxed run, which refuses the other writes itself (ADR 0008).
fn outside_write_scope(project: &Project, params: &RunParams) -> Option<WriteScope> {
    if params.sandbox {
        return None;
    }
    tenaz_adapters::sandbox::write_scope(project.root(), &params.sandbox_allow_write)
        .inspect_err(|e| tracing::warn!("writes outside the project will not be noted: {e}"))
        .ok()
}

/// `verify` and the heavy-job runner, both started in the sandbox and
/// followed by its guard's restore when the run has one.
fn runners(
    project: &Project,
    params: &RunParams,
    sandbox: Option<&Sandbox>,
) -> (
    Arc<dyn tenaz_core::ports::VerifyRunner>,
    Arc<dyn tenaz_core::ports::JobRunner>,
) {
    let wrapper = sandbox.map(|s| s.wrapper.clone()).unwrap_or_default();
    let verify = Arc::new(
        VerifyProcess::new(project.root())
            .at(params.verify_path())
            .wrapped(wrapper.clone()),
    );
    let jobs = Arc::new(JobProcess::new(project.root()).wrapped(wrapper));
    match sandbox {
        Some(sandbox) => (
            Arc::new(GuardedVerify::new(verify, sandbox.guard.clone())),
            Arc::new(GuardedJobs::new(jobs, sandbox.guard.clone())),
        ),
        None => (verify, jobs),
    }
}

/// A sandboxed run's launcher prefix and the guard of its protected paths.
struct Sandbox {
    wrapper: Vec<String>,
    guard: Arc<Guard>,
}

/// The sandbox when the run asks for one (`sandbox` in run.json), checked
/// by starting a program in it; an error when this machine cannot give one.
fn sandbox(project: &Project, params: &RunParams) -> anyhow::Result<Option<Sandbox>> {
    if !params.sandbox {
        return Ok(None);
    }
    let launcher = std::env::current_exe().context("cannot find the tenaz binary")?;
    let policy = Policy::for_project(
        project.root(),
        &params.sandbox_allow_write,
        &params.sandbox_deny_read,
    )
    .context("cannot build the sandbox policy")?;
    policy.probe(&launcher).context(
        "the run asks for a sandbox (\"sandbox\": true in the params of .tenaz/run.json) and \
         this machine cannot give one; fix that, or set it to false to run without one",
    )?;
    let show = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    tracing::info!(
        "sandbox: the agent and verify write only to {}",
        show(&policy.write)
    );
    tracing::info!("sandbox: protected {}", show(&policy.deny_write));
    if !policy.deny_read.is_empty() {
        tracing::info!("sandbox: unreadable {}", show(&policy.deny_read));
    }
    Ok(Some(Sandbox {
        wrapper: policy.wrapper(&launcher),
        guard: Arc::new(Guard::new(&policy, project)),
    }))
}

/// Appends one repair to `log`, as a JSON line.
fn log_repair(log: &Path, request: u64, tool: &str, fields: &[String]) {
    use tenaz_core::ports::Clock as _;
    tracing::debug!(
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

/// The engine's usage as the harness sees it; cached tokens the engine
/// did not name count as 0.
fn harness_usage(u: &tenaz_proxy::ProxyUsage) -> Usage {
    let cached = u.cached_tokens.unwrap_or(0);
    Usage {
        input: u.prompt_tokens.saturating_sub(cached),
        output: u.completion_tokens,
        cache_read: cached,
        cache_write: 0,
        reasoning: u.reasoning_tokens,
    }
}

/// The output limit fx and `OpenCode` are configured with, and what the
/// proxy takes as the limit of a request that does not say.
const HARNESS_MAX_OUTPUT_TOKENS: u64 = 16_384;

/// What the runner does with each proxy event: usage, engine errors and
/// refused compactions go to the sessions' `feed`, whether the usage names
/// cached tokens to `cache_report`, and repaired tool arguments to
/// `repair_log`.
fn proxy_observer(
    feed: tokio::sync::broadcast::Sender<EngineObservation>,
    cache_report: CacheReport,
    repair_log: Option<std::path::PathBuf>,
) -> tenaz_proxy::Observer {
    let first_repair = std::sync::atomic::AtomicBool::new(true);
    Arc::new(move |event: ProxyEvent| match event {
        ProxyEvent::Completed {
            usage: Some(u),
            finish_reason,
            timings,
            sent,
            ..
        } => {
            cache_report.note(u.cached_tokens.is_some());
            let _ = feed.send(EngineObservation::Completed {
                usage: harness_usage(&u),
                stop_reason: stop_reason_for_finish(finish_reason.as_deref()),
                server: timings,
                sent,
            });
        }
        ProxyEvent::EngineError { message, .. } => {
            let _ = feed.send(EngineObservation::Error { message });
        }
        ProxyEvent::CompactionRefused { .. } => {
            tracing::info!("the proxy refused the agent's request to compact its context");
            let _ = feed.send(EngineObservation::CompactionRefused);
        }
        ProxyEvent::ToolArgsRepaired {
            request,
            tool,
            fields,
        } => {
            if let Some(log) = &repair_log {
                if first_repair.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(
                        "the engine sends some tool-call arguments as text; the proxy repairs them \
                         (each one in {}, the count in tenaz status)",
                        log.display()
                    );
                }
                log_repair(log, request, &tool, &fields);
            }
        }
        ProxyEvent::Completed { .. } | ProxyEvent::Held { .. } => {}
    })
}

/// The engine proxy for `project`'s run (`ledger`): per-request usage and
/// errors go to `feed`, whether the usage names cached tokens to
/// `cache_report`, requests are given the `reasoning_effort` the thinking
/// level asks for and the run's pinned temperature (see
/// [`request_defaults`]), and with `repairToolArgs`, tool-call arguments
/// sent as text are repaired and logged.
async fn start_proxy(
    base_url: &str,
    gate: Gate,
    feed: tokio::sync::broadcast::Sender<EngineObservation>,
    cache_report: CacheReport,
    effort: Option<EffortDial>,
    project: &Project,
    ledger: &tenaz_core::ledger::Ledger,
) -> anyhow::Result<tenaz_proxy::ProxyHandle> {
    let repairs = ledger.params.repair_tool_args.then(|| {
        project
            .private_dir()
            .join(tenaz_core::orchestrator::TOOL_REPAIRS)
    });
    let mut proxy = Proxy::new(
        origin(base_url)?,
        gate,
        proxy_observer(feed, cache_report, repairs.clone()),
    );
    proxy.repair_tool_args = repairs.is_some();
    proxy.effort = effort;
    set_up_for_run(&mut proxy, ledger);
    let handle = Arc::new(proxy)
        .start("127.0.0.1:0".parse()?)
        .await
        .context("cannot start the engine proxy")?;
    tracing::info!("engine proxy at {} -> {}", handle.base_url(), base_url);
    Ok(handle)
}

/// What the engine proxy does for the run in `ledger`: the output cap,
/// how much earlier reasoning it keeps, the summary request it refuses
/// for the agent (ADRs 0022 and 0030), and the request defaults.
fn set_up_for_run(proxy: &mut Proxy, ledger: &tenaz_core::ledger::Ledger) {
    proxy.max_output_tokens = Some(HARNESS_MAX_OUTPUT_TOKENS);
    proxy.past_reasoning_budget = ledger.params.past_reasoning_budget();
    proxy.compaction_prompt_opening =
        tenaz_acp::compaction_prompt(ledger.config.harness).map(str::to_owned);
    proxy.request_defaults =
        request_defaults(ledger.config.thinking.as_deref(), ledger.params.temperature);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tenaz_adapters::engine::generic::{Commands, GenericEngine};
    use tenaz_adapters::engine::load_api::LoadApiEngine;
    use tenaz_core::lease::{Gate, LeaseManager};
    use tenaz_core::ports::EngineAdapter;

    use super::{
        BTreeMap, Duration, MediaOptions, harness_usage, mcp_endpoints, media_adapter,
        mesh_adapter, reasoning_effort, request_defaults, swapping,
    };

    /// Pi's agent directory lists the run's model first, then each role's
    /// model once with its own window and inputs, keeping the user's
    /// settings for it; every role's session finds its model there.
    #[test]
    fn pi_s_agent_directory_lists_each_model_of_the_run_once() {
        use tenaz_core::ledger::{HarnessKind, ModelConfig};
        use tenaz_core::role::Role;
        use tenaz_core::role_models::RoleModel;
        let dir = tempfile::tempdir().unwrap();
        let user_models = dir.path().join("user-models.json");
        let user = r#"{"providers": {"sushi": {"apiKey": "user-key", "models": [
            {"id": "critic", "thinkingLevelMap": {"off": "none"}}]}}}"#;
        std::fs::write(&user_models, user).unwrap();
        let critic = RoleModel {
            model: "sushi/critic".into(),
            id: "critic".into(),
            context_window: 32768,
            input: vec!["text".into()],
        };
        let config = ModelConfig {
            harness: HarnessKind::Pi,
            harness_version: "1.1.0".into(),
            model: "sushi/m".into(),
            provider: Some("sushi".into()),
            id: "m".into(),
            base_url: None,
            context_window: 131_072,
            input: vec!["text".into(), "image".into()],
            thinking: None,
            role_models: BTreeMap::from([(Role::Reviewer, critic.clone()), (Role::Split, critic)]),
        };
        let agent = dir.path().join("pi-agent");
        let base_url = "http://127.0.0.1:4001/v1";
        let models =
            super::harness::write_pi_agent(&agent, &config, base_url, Some(&user_models)).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(agent.join("models.json")).unwrap())
                .unwrap();
        let sushi = &written["providers"]["sushi"];
        assert_eq!(
            (&sushi["apiKey"], &sushi["baseUrl"]),
            (&"user-key".into(), &base_url.into())
        );
        let listed: Vec<String> = sushi["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| format!("{} {} {}", m["id"], m["contextWindow"], m["input"]))
            .collect();
        assert_eq!(
            listed,
            [
                r#""m" 131072 ["text","image"]"#,
                r#""critic" 32768 ["text"]"#
            ]
        );
        assert_eq!(sushi["models"][1]["thinkingLevelMap"]["off"], "none");
        let launch =
            tenaz_pi::PiLaunch::new("pi", models.run_name()).other_models(models.other_names());
        for role in Role::ALL {
            let id = config.model_for(role).id;
            assert!(launch.for_model(&id).is_some(), "{role}: {id}");
        }
    }

    /// A lease over two models on `engine`, swapped when `swap` asks.
    fn lease_on(engine: Arc<dyn EngineAdapter>, swap: bool) -> anyhow::Result<LeaseManager> {
        let models = vec!["m".to_owned(), "r".to_owned()];
        let lease = LeaseManager::new(engine, models, Gate::new(), Arc::new(|_| {}));
        swapping(lease, swap)
    }

    /// `--swap-models` reaches the lease, which a load-api engine lets swap;
    /// a generic engine's lease is refused instead of keeping every model.
    #[test]
    fn swap_models_makes_the_lease_swap_or_refuses() {
        let load_api = || Arc::new(LoadApiEngine::new("http://127.0.0.1:1/v1"));
        assert!(lease_on(load_api(), true).unwrap().swaps());
        assert!(!lease_on(load_api(), false).unwrap().swaps());
        let commands = Commands {
            start: Some(vec!["start".into()]),
            stop: Some(vec!["stop".into()]),
        };
        let generic = GenericEngine::new(
            "g",
            "http://127.0.0.1:1/v1",
            commands,
            Duration::from_secs(1),
        );
        let refused = lease_on(Arc::new(generic), true)
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            refused.as_deref(),
            Some(
                "--swap-models: the engine cannot unload one model and keep answering; \
                 tenaz run --no-swap-models turns swapping off"
            )
        );
    }

    #[test]
    fn cached_tokens_the_engine_did_not_name_count_as_zero() {
        let usage = |cached_tokens| tenaz_proxy::ProxyUsage {
            prompt_tokens: 2796,
            cached_tokens,
            completion_tokens: 9,
            reasoning_tokens: 0,
        };
        let unnamed = harness_usage(&usage(None));
        assert_eq!((unnamed.input, unnamed.cache_read), (2796, 0));
        let named = harness_usage(&usage(Some(2135)));
        assert_eq!((named.input, named.cache_read), (661, 2135));
        assert_eq!(named.output, 9);
    }

    #[test]
    fn mcp_endpoints_put_the_media_server_first_then_the_user_s_by_name() {
        let own = BTreeMap::from([
            ("docs".to_owned(), "http://d/".to_owned()),
            ("blender".to_owned(), "http://b/".to_owned()),
        ]);
        let names = |media: Option<&str>| {
            mcp_endpoints(media.map(str::to_owned), &own)
                .into_iter()
                .map(|e| format!("{}={}", e.name, e.url))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(Some("http://m/")),
            ["media=http://m/", "blender=http://b/", "docs=http://d/"]
        );
        assert_eq!(names(None), ["blender=http://b/", "docs=http://d/"]);
        assert!(mcp_endpoints(None, &BTreeMap::new()).is_empty());
    }

    fn images(url: &str, model: Option<&str>) -> MediaOptions {
        MediaOptions {
            comfyui: None,
            workflows: BTreeMap::new(),
            images_url: Some(url.to_owned()),
            image_model: model.map(str::to_owned),
            meshes_url: None,
            mesh_model: None,
            timeout: Duration::from_mins(1),
        }
    }

    #[test]
    fn a_mesh_api_needs_its_model_and_may_share_the_llms_engine() {
        let llm = "http://127.0.0.1:11234/v1";
        let meshes = |url: &str, model: Option<&str>| MediaOptions {
            images_url: None,
            meshes_url: Some(url.to_owned()),
            mesh_model: model.map(str::to_owned),
            ..images("", None)
        };
        let same = mesh_adapter(&meshes(llm, Some("Idol")), llm)
            .unwrap()
            .unwrap();
        assert_eq!(same.name(), "mesh-api");
        assert!(same.shares_engine());
        let other = mesh_adapter(&meshes("http://127.0.0.1:18000/v1", Some("Idol")), llm)
            .unwrap()
            .unwrap();
        assert!(!other.shares_engine());
        let error = mesh_adapter(&meshes(llm, None), llm).err().unwrap();
        assert!(error.to_string().contains("--mesh-model"), "{error}");
        assert!(mesh_adapter(&images("", None), llm).unwrap().is_none());
    }

    #[test]
    fn an_images_api_on_the_llms_origin_shares_its_engine() {
        let llm = "http://127.0.0.1:12345/v1";
        let same = media_adapter(&images("http://127.0.0.1:12345/v1", Some("flux")), llm)
            .unwrap()
            .unwrap();
        assert_eq!(same.name(), "openai-images");
        assert!(same.shares_engine());
        let other = media_adapter(&images("http://127.0.0.1:1337/v1", Some("flux")), llm)
            .unwrap()
            .unwrap();
        assert!(!other.shares_engine());
        let error = media_adapter(&images("http://127.0.0.1:1337/v1", None), llm)
            .err()
            .unwrap();
        assert!(error.to_string().contains("--image-model"), "{error}");
        let both = MediaOptions {
            comfyui: Some("http://127.0.0.1:8188".into()),
            ..images("http://127.0.0.1:1337/v1", Some("flux"))
        };
        assert!(media_adapter(&both, llm).is_err());
        let none = MediaOptions {
            images_url: None,
            ..images("", None)
        };
        assert!(media_adapter(&none, llm).unwrap().is_none());
    }

    /// The proxy of a dsh or Qwen Code run refuses that agent's summary
    /// request before it reaches the engine (there is none here).
    #[tokio::test]
    async fn a_run_s_proxy_refuses_its_agent_s_summary_request() {
        use tenaz_core::ledger::HarnessKind;
        let dir = tempfile::tempdir().unwrap();
        let project = tenaz_core::project::Project::new(dir.path());
        for (harness, prompt) in [
            (HarnessKind::Dsh, tenaz_acp::dsh::COMPACTION_PROMPT),
            (HarnessKind::Qwen, tenaz_acp::qwen::COMPACTION_PROMPT),
        ] {
            let (feed, _) = tokio::sync::broadcast::channel(8);
            let handle = super::start_proxy(
                "http://127.0.0.1:1/v1",
                Gate::new(),
                feed,
                tenaz_core::cache_report::CacheReport::new(),
                None,
                &project,
                &crate::testing::ledger_for(harness),
            )
            .await
            .unwrap();
            let summarize = serde_json::json!({"messages": [
                {"role": "user", "content": "build the map"},
                {"role": "user", "content": [{"type": "text", "text": format!("{prompt} …")}]}
            ]});
            let refused = reqwest::Client::new()
                .post(format!("{}/chat/completions", handle.base_url()))
                .json(&summarize)
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), 400, "{harness}");
            handle.stop().await;
        }
    }

    /// Only a dsh or Qwen Code run's proxy refuses a summary request.
    #[test]
    fn other_agents_runs_refuse_no_summary_request() {
        use tenaz_core::ledger::HarnessKind;
        for harness in [
            HarnessKind::Fx,
            HarnessKind::Pi,
            HarnessKind::OpenCode,
            HarnessKind::Acp,
            HarnessKind::Fake,
        ] {
            let mut proxy =
                tenaz_proxy::Proxy::new("http://127.0.0.1:1", Gate::new(), Arc::new(|_| {}));
            super::set_up_for_run(&mut proxy, &crate::testing::ledger_for(harness));
            assert_eq!(proxy.compaction_prompt_opening, None, "{harness}");
        }
    }

    #[test]
    fn the_proxy_fills_in_the_effort_and_a_pinned_temperature() {
        let defaults = request_defaults(Some("medium"), Some(0.0));
        assert_eq!(
            serde_json::Value::Object(defaults),
            serde_json::json!({"reasoning_effort": "medium", "temperature": 0.0})
        );
        assert!(request_defaults(Some("off"), None).is_empty());
    }

    #[test]
    fn thinking_level_becomes_a_reasoning_effort_unless_off() {
        assert_eq!(reasoning_effort(Some("medium")), Some("medium"));
        assert_eq!(reasoning_effort(Some(" high ")), Some("high"));
        assert_eq!(reasoning_effort(Some("off")), None);
        assert_eq!(reasoning_effort(Some("auto")), None);
        assert_eq!(reasoning_effort(None), None);
    }
}
