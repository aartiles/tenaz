//! Composition: builds the adapters, the lease, the proxy, the MCP server and
//! the harness for a run from the ledger and the command line.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use smr_acp::fx::{FxLaunch, FxSettings, ProfileMcpServer, write_profile};
use smr_acp::{EngineObservation, McpEndpoint};
use smr_adapters::clock::SystemClock;
use smr_adapters::engine::generic::{Commands, GenericEngine};
use smr_adapters::engine::sushi::SushiEngine;
use smr_adapters::git::GitRepo;
use smr_adapters::media::comfyui::ComfyUi;
use smr_adapters::metrics::PrometheusMetrics;
use smr_adapters::notify::DesktopNotifier;
use smr_adapters::probe_macos::MacMemoryProbe;
use smr_adapters::verify::VerifyProcess;
use smr_core::lease::{Gate, LeaseManager, LeaseTransition};
use smr_core::ledger::JournalRecord;
use smr_core::orchestrator::Deps;
use smr_core::ports::{EngineAdapter, Harness, MediaAdapter};
use smr_core::project::Project;
use smr_core::session::Usage;
use smr_core::testing::ScriptedHarness;
use smr_mcp::workflow::WorkflowTemplate;
use smr_mcp::{MediaTools, serve};
use smr_proxy::{Proxy, ProxyEvent};

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
}

pub struct MediaOptions {
    pub comfyui: Option<String>,
    pub workflows: BTreeMap<String, WorkflowTemplate>,
    pub timeout: Duration,
}

/// Background services a run holds open.
#[derive(Default)]
pub struct Services {
    proxy: Option<smr_proxy::ProxyHandle>,
    mcp: Option<smr_mcp::McpHandle>,
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
        .context("No .agent/run.json; run init first")?;
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
    let observer: smr_core::lease::Observer = Arc::new(move |t: LeaseTransition| {
        use smr_core::ports::Clock as _;
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
        let sink = feed.clone();
        let proxy = Arc::new(Proxy::new(
            origin(&base_url)?,
            gate.clone(),
            Arc::new(move |event: ProxyEvent| match event {
                ProxyEvent::Completed { usage: Some(u), .. } => {
                    let _ = sink.send(EngineObservation::Completed {
                        usage: Usage {
                            input: u.prompt_tokens.saturating_sub(u.cached_tokens),
                            output: u.completion_tokens,
                            cache_read: u.cached_tokens,
                            cache_write: 0,
                            reasoning: u.reasoning_tokens,
                        },
                    });
                }
                ProxyEvent::EngineError { message, .. } => {
                    let _ = sink.send(EngineObservation::Error { message });
                }
                ProxyEvent::Completed { .. } | ProxyEvent::Held { .. } => {}
            }),
        ));
        let handle = proxy
            .start("127.0.0.1:0".parse()?)
            .await
            .context("cannot start the engine proxy")?;
        tracing::info!("engine proxy at {} -> {}", handle.base_url(), base_url);
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
        &feed,
    )?;
    let port = reqwest_url(&base_url)?.2;
    let deps = Deps {
        harness,
        vcs: Arc::new(GitRepo::new(project.root())),
        verify: Arc::new(VerifyProcess::new(project.root())),
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
            .map(|m| Arc::new(m) as Arc<dyn smr_core::ports::EngineMetricsSource>),
        memory: cfg!(target_os = "macos")
            .then(|| Arc::new(MacMemoryProbe::new(port)) as Arc<dyn smr_core::ports::MemoryProbe>),
    };
    Ok(Wired { deps, services })
}

/// The harness named in the ledger: fx over ACP with its profile written
/// into the project, or the scripted fake.
fn build_harness(
    project: &Project,
    config: &smr_core::ledger::ModelConfig,
    options: &HarnessOptions,
    media_timeout: &Duration,
    harness_base_url: String,
    mcp_endpoints: &[McpEndpoint],
    feed: &tokio::sync::broadcast::Sender<EngineObservation>,
) -> anyhow::Result<Arc<dyn Harness>> {
    match config.harness.as_str() {
        "fake" => Ok(Arc::new(ScriptedHarness::default())),
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
            let mut launch = FxLaunch::new(&options.fx_bin, &home);
            launch.args.clone_from(&options.fx_args);
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
