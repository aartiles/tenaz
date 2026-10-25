//! The media MCP server: tools the harness session can call that need
//! another engine. Each call takes the lease (unload the LLM when it must,
//! run the job, reload the LLM) and returns the output paths.

pub mod workflow;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tenaz_core::lease::LeaseManager;
use tenaz_core::ports::MediaJob;
use tokio_util::sync::CancellationToken;

use workflow::WorkflowTemplate;

/// What the tools need.
#[derive(Clone)]
pub struct MediaTools {
    lease: Arc<LeaseManager>,
    output_root: PathBuf,
    workflows: BTreeMap<String, WorkflowTemplate>,
    media_engine: String,
    sequence: Arc<AtomicU64>,
}

impl std::fmt::Debug for MediaTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaTools")
            .field("output_root", &self.output_root)
            .field("workflows", &self.workflows.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The width and height a workflow gets when the call gives none.
const DEFAULT_SIZE: u32 = 1024;

/// Arguments of `generate_image`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateImageArgs {
    /// What to draw.
    pub prompt: String,
    /// What to avoid.
    #[serde(default)]
    pub negative_prompt: Option<String>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// Pixels; 1024 when omitted.
    #[serde(default)]
    pub width: Option<u32>,
    /// Pixels; 1024 when omitted.
    #[serde(default)]
    pub height: Option<u32>,
    /// A configured `ComfyUI` workflow name; the default workflow otherwise.
    #[serde(default)]
    pub workflow: Option<String>,
}

#[tool_router]
impl MediaTools {
    /// `media_engine` names the lease's media adapter (`comfyui`,
    /// `openai-images`). With no `workflows` the request goes to the engine
    /// as it is; with them, it fills the chosen `ComfyUI` workflow.
    pub fn new(
        lease: Arc<LeaseManager>,
        output_root: impl Into<PathBuf>,
        workflows: BTreeMap<String, WorkflowTemplate>,
        media_engine: impl Into<String>,
    ) -> Self {
        Self {
            lease,
            output_root: output_root.into(),
            workflows,
            media_engine: media_engine.into(),
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    #[tool(
        description = "Generate an image with the local image engine. The call can take a few minutes. Returns the paths of the generated files."
    )]
    async fn generate_image(
        &self,
        Parameters(args): Parameters<GenerateImageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let seed = args.seed.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        });
        // A workflow's size placeholders need a number: left as text,
        // ComfyUI rejects the whole workflow.
        let vars = BTreeMap::from([
            ("prompt", args.prompt.clone()),
            ("negative", args.negative_prompt.clone().unwrap_or_default()),
            ("seed", seed.to_string()),
            ("width", args.width.unwrap_or(DEFAULT_SIZE).to_string()),
            ("height", args.height.unwrap_or(DEFAULT_SIZE).to_string()),
        ]);
        let request = serde_json::json!({
            "prompt": args.prompt,
            "negative_prompt": args.negative_prompt,
            "seed": seed,
            "width": args.width,
            "height": args.height,
        });
        let (params, name) = if self.workflows.is_empty() {
            (serde_json::json!({ "request": request }), None)
        } else {
            let name = args.workflow.unwrap_or_else(|| "default".to_owned());
            let template = self.workflows.get(&name).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!(
                        "no workflow named {name}; configured: {}",
                        self.workflows
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    None,
                )
            })?;
            let workflow = template.render(&vars);
            (
                serde_json::json!({ "workflow": workflow, "request": request }),
                Some(name),
            )
        };
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let output_dir = self.output_root.join(format!("{}-{sequence:04}", stamp()));
        let job = MediaJob {
            kind: "image".to_owned(),
            params,
            output_dir: output_dir.clone(),
        };
        let output = self
            .lease
            .run_media(&self.media_engine, job)
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("image generation failed: {e}"), None)
            })?;
        let paths: Vec<String> = output
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let text = if paths.is_empty() {
            format!(
                "The job finished but produced no files (output dir {}).",
                output_dir.display()
            )
        } else {
            format!("Generated {} file(s):\n{}", paths.len(), paths.join("\n"))
        };
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.structured_content =
            Some(serde_json::json!({ "paths": paths, "seed": seed, "workflow": name }));
        Ok(result)
    }

    #[tool(description = "List the media engines and image workflows this runner offers.")]
    fn list_media_engines(&self) -> CallToolResult {
        let engines = self.lease.media_names();
        let workflows: Vec<&String> = self.workflows.keys().collect();
        CallToolResult::success(vec![ContentBlock::text(format!(
            "engines: {}\nworkflows: {}",
            engines.join(", "),
            workflows
                .iter()
                .map(|w| w.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))])
    }
}

#[tool_handler]
impl ServerHandler for MediaTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tenaz-media", env!("CARGO_PKG_VERSION")))
            .with_instructions("Media tools of the single-model runner. generate_image renders with the local image engine and can take minutes (the runner may unload and reload the LLM around it), so call it once with a complete prompt.")
    }
}

fn stamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .replace([':', '.'], "-")
}

/// The running MCP server.
#[derive(Debug)]
pub struct McpHandle {
    pub local_addr: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl McpHandle {
    /// The streamable HTTP endpoint (`http://127.0.0.1:<port>/mcp`).
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.local_addr)
    }

    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

/// Serves the tools over streamable HTTP at `/mcp` on `addr` (port 0 for an ephemeral port).
pub async fn serve(tools: MediaTools, addr: SocketAddr) -> std::io::Result<McpHandle> {
    let shutdown = CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = shutdown.child_token();
    let service = StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let signal = shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { signal.cancelled().await })
            .await
    });
    Ok(McpHandle {
        local_addr,
        shutdown,
        task,
    })
}
