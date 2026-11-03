//! The media MCP server: tools the harness session can call that need
//! another engine, an image engine or a 3D one (ADR 0020). Each call takes
//! the lease (unload the LLM when it must, run the job, reload the LLM) and
//! returns the output paths.

pub mod workflow;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
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
    /// The lease's image adapter; `None` hides `generate_image`.
    image_engine: Option<String>,
    /// The lease's 3D adapter; `None` hides `generate_3d`.
    meshes: Option<Meshes>,
    sequence: Arc<AtomicU64>,
}

/// The 3D engine and the project its paths are in.
#[derive(Clone, Debug)]
struct Meshes {
    engine: String,
    root: PathBuf,
}

impl std::fmt::Debug for MediaTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaTools")
            .field("output_root", &self.output_root)
            .field("workflows", &self.workflows.keys().collect::<Vec<_>>())
            .field("image_engine", &self.image_engine)
            .field("meshes", &self.meshes)
            .finish_non_exhaustive()
    }
}

/// The width and height a workflow gets when the call gives none.
const DEFAULT_SIZE: u32 = 1024;

/// How long an MCP session may go without a request before the server ends
/// it. rmcp's default is 5 minutes, but fx opens its session when an agent
/// session starts and may make the first media call 20 minutes later; fx
/// then reopens the session yet fails that call (`McpSessionExpired`),
/// which costs the run one call of its image budget. A day outlasts any
/// agent session; the server itself stops with the run.
const SESSION_IDLE_LIMIT: Duration = Duration::from_hours(24);

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

/// Arguments of `generate_3d`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Generate3dArgs {
    /// A PNG or JPEG inside the project, of one object. TRELLIS.2 needs a
    /// cutout whose alpha is the object; Idol cuts the object out itself.
    pub image: String,
    /// Where to write the GLB, inside the project and ending in `.glb`.
    #[serde(default)]
    pub output: Option<String>,
    /// TRELLIS.2: 512, 1024 or 1536; higher is finer, slower and needs more memory.
    #[serde(default)]
    pub resolution: Option<u32>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// Sampling steps; the model's default when omitted.
    #[serde(default)]
    pub steps: Option<u32>,
    /// Idol: `synthetic` (renders, the object fills the frame) or `real` (photographs).
    #[serde(default)]
    pub mode: Option<String>,
}

/// Media tools whose engines the run has not configured.
impl MediaTools {
    /// No tool yet: add an image engine, a 3D engine or both. Outputs
    /// without a path of their own go under `output_root`.
    pub fn new(lease: Arc<LeaseManager>, output_root: impl Into<PathBuf>) -> Self {
        Self {
            lease,
            output_root: output_root.into(),
            workflows: BTreeMap::new(),
            image_engine: None,
            meshes: None,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// `generate_image` through the lease's media adapter `engine`
    /// (`comfyui`, `openai-images`). With no `workflows` the request goes
    /// to the engine as it is; with them, it fills the chosen `ComfyUI`
    /// workflow.
    #[must_use]
    pub fn with_images(
        mut self,
        engine: impl Into<String>,
        workflows: BTreeMap<String, WorkflowTemplate>,
    ) -> Self {
        self.image_engine = Some(engine.into());
        self.workflows = workflows;
        self
    }

    /// `generate_3d` through the lease's media adapter `engine`, reading
    /// and writing inside the project at `root` (absolute).
    #[must_use]
    pub fn with_meshes(mut self, engine: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        self.meshes = Some(Meshes {
            engine: engine.into(),
            root: root.into(),
        });
        self
    }

    /// The tools of the engines the run has.
    fn router(&self) -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        if self.image_engine.is_none() {
            router.remove_route("generate_image");
        }
        if self.meshes.is_none() {
            router.remove_route("generate_3d");
        }
        router
    }

    /// What the server tells the agent: one sentence per tool it has.
    fn instructions(&self) -> String {
        let mut text = "Media tools of the single-model runner. Each call can take minutes \
                        (the runner may unload and reload the LLM around it)."
            .to_owned();
        if self.image_engine.is_some() {
            text.push_str(
                " generate_image renders with the local image engine: call it once with a complete prompt.",
            );
        }
        if self.meshes.is_some() {
            text.push_str(
                " generate_3d makes a GLB from a picture with the local 3D engine: call it once per object.",
            );
        }
        text
    }

    /// A new directory name under `output_root` for one job's files.
    fn next_output_dir(&self) -> PathBuf {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        self.output_root.join(format!("{}-{sequence:04}", stamp()))
    }
}

#[tool_router]
impl MediaTools {
    #[tool(
        description = "Generate an image with the local image engine. The call can take a few minutes. Returns the paths of the generated files."
    )]
    async fn generate_image(
        &self,
        Parameters(args): Parameters<GenerateImageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let engine = self
            .image_engine
            .clone()
            .ok_or_else(|| ErrorData::invalid_request("this run has no image engine", None))?;
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
        let output_dir = self.next_output_dir();
        let job = MediaJob {
            kind: "image".to_owned(),
            params,
            output_dir: output_dir.clone(),
        };
        let output = self.lease.run_media(&engine, job).await.map_err(|e| {
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

    #[tool(
        description = "Make a 3D mesh (GLB) from a picture of one object with the local 3D engine. The call can take several minutes. Give `output`, a .glb path inside the project. Returns the path of the GLB."
    )]
    async fn generate_3d(
        &self,
        Parameters(args): Parameters<Generate3dArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let meshes = self
            .meshes
            .clone()
            .ok_or_else(|| ErrorData::invalid_request("this run has no 3D engine", None))?;
        let refused = |why: String| ErrorData::invalid_params(why, None);
        let image = tenaz_core::mesh::in_project(&meshes.root, &args.image).map_err(refused)?;
        // Checked before the lease, so a wrong path does not cost an LLM
        // reload, and the link check first, so nothing outside is read.
        not_linked_out(&meshes.root, &image, &args.image).map_err(refused)?;
        picture(&image, &args.image).map_err(refused)?;
        let output = match args.output.as_deref().filter(|o| !o.trim().is_empty()) {
            Some(requested) => {
                let output =
                    tenaz_core::mesh::mesh_output(&meshes.root, requested).map_err(refused)?;
                not_linked_out(&meshes.root, &output, requested).map_err(refused)?;
                output
            }
            None => self.next_output_dir().join("mesh.glb"),
        };
        let output_dir = output.parent().map(PathBuf::from).unwrap_or_default();
        let request = serde_json::json!({
            "image": image,
            "output": output,
            "resolution": args.resolution,
            "seed": args.seed,
            "steps": args.steps,
            "mode": args.mode,
        });
        let job = MediaJob {
            kind: "mesh".to_owned(),
            params: serde_json::json!({ "request": request }),
            output_dir,
        };
        let generated = self
            .lease
            .run_media(&meshes.engine, job)
            .await
            .map_err(|e| ErrorData::internal_error(format!("3D generation failed: {e}"), None))?;
        let path = output.display().to_string();
        let mut result =
            CallToolResult::success(vec![ContentBlock::text(format!("Wrote the GLB:\n{path}"))]);
        result.structured_content = Some(serde_json::json!({
            "path": path,
            "model": generated.detail.get("model"),
            "bytes": generated.detail.get("bytes"),
        }));
        Ok(result)
    }

    #[tool(
        description = "List the media engines (image, 3D) and image workflows this runner offers."
    )]
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

#[tool_handler(router = self.router())]
impl ServerHandler for MediaTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "tenaz-media",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.instructions())
    }
}

/// Whether `path`, inside `root` as written, stays inside it and out of
/// `.git` and `.tenaz` once its symbolic links are followed: the deepest
/// part of it that exists is resolved and compared with the resolved root.
/// A root that cannot be resolved refuses everything.
fn not_linked_out(
    root: &std::path::Path,
    path: &std::path::Path,
    given: &str,
) -> Result<(), String> {
    let real_root = std::fs::canonicalize(root).map_err(|e| format!("the project root: {e}"))?;
    let real = path
        .ancestors()
        .find_map(|a| std::fs::canonicalize(a).ok())
        .ok_or_else(|| format!("{given} cannot be resolved"))?;
    let inside = real
        .strip_prefix(&real_root)
        .map_err(|_| format!("{given} leads outside the project through a symbolic link"))?;
    let relative = inside.to_string_lossy();
    tenaz_core::mesh::in_project(&real_root, &relative)
        .map(|_| ())
        .or_else(|why| {
            if relative.is_empty() {
                Ok(())
            } else {
                Err(why)
            }
        })
}

/// Whether the file at `path` (named `given` by the agent) is a PNG or a JPEG.
fn picture(path: &std::path::Path, given: &str) -> Result<(), String> {
    use std::io::Read as _;
    let mut head = [0_u8; 8];
    let read = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .map_err(|e| format!("{given}: {e}"))?;
    if tenaz_core::mesh::is_picture(&head[..read]) {
        Ok(())
    } else {
        Err(format!("{given} is not a PNG or JPEG"))
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

/// Sessions that stay open while the agent works without media calls.
fn session_manager() -> LocalSessionManager {
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = Some(SESSION_IDLE_LIMIT);
    manager
}

/// Serves the tools over streamable HTTP at `/mcp` on `addr` (port 0 for an ephemeral port).
pub async fn serve(tools: MediaTools, addr: SocketAddr) -> std::io::Result<McpHandle> {
    let shutdown = CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = shutdown.child_token();
    let service = StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(session_manager()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_outlives_a_long_stretch_without_media_calls() {
        let keep_alive = session_manager().session_config.keep_alive;
        assert!(
            keep_alive.is_some_and(|k| k >= Duration::from_hours(4)),
            "{keep_alive:?}"
        );
    }
}
