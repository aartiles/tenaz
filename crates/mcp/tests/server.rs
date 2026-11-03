//! The media MCP server over streamable HTTP, called with the rmcp client,
//! against fake engines under a real lease manager.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use tenaz_core::lease::{Gate, LeaseManager};
use tenaz_core::ports::{
    EngineAdapter, EngineError, MediaAdapter, MediaError, MediaJob, MediaOutput, ModelState,
};
use tenaz_mcp::workflow::WorkflowTemplate;
use tenaz_mcp::{MediaTools, serve};

#[derive(Default)]
struct Log(Mutex<Vec<String>>);

/// An engine with the one model `m`, loaded until it is unloaded.
struct FakeLlm {
    log: Arc<Log>,
    loaded: AtomicBool,
}

impl FakeLlm {
    fn new(log: Arc<Log>) -> Self {
        Self {
            log,
            loaded: AtomicBool::new(true),
        }
    }
}

#[async_trait]
impl EngineAdapter for FakeLlm {
    fn name(&self) -> &str {
        "fake"
    }
    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        let loaded = self.loaded.load(Ordering::SeqCst);
        Ok(vec![ModelState {
            id: "m".into(),
            loaded,
            ready: loaded,
            context_length: None,
        }])
    }
    async fn load(&self, _model: &str) -> Result<(), EngineError> {
        self.log.0.lock().unwrap().push("llm load".into());
        self.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn unload(&self, _model: &str) -> Result<(), EngineError> {
        self.log.0.lock().unwrap().push("llm unload".into());
        self.loaded.store(false, Ordering::SeqCst);
        Ok(())
    }
}

struct FakeComfy(Arc<Log>);
#[async_trait]
impl MediaAdapter for FakeComfy {
    fn name(&self) -> &str {
        "comfyui"
    }
    async fn load(&self) -> Result<(), MediaError> {
        Ok(())
    }
    async fn run(&self, job: MediaJob) -> Result<MediaOutput, MediaError> {
        // A ComfyUI job carries the filled workflow; any other, the request.
        let (text, seed, size) = match job.params.get("workflow") {
            Some(workflow) => (
                &workflow["1"]["inputs"]["text"],
                &workflow["1"]["inputs"]["seed"],
                format!(
                    " size={}x{}",
                    workflow["2"]["inputs"]["width"], workflow["2"]["inputs"]["height"]
                ),
            ),
            None => (
                &job.params["request"]["prompt"],
                &job.params["request"]["seed"],
                String::new(),
            ),
        };
        let text = text.as_str().unwrap_or_default().to_owned();
        self.0
            .0
            .lock()
            .unwrap()
            .push(format!("job: {text} seed={seed}{size}"));
        if text.contains("fail") {
            return Err(MediaError::Failed("bad prompt".into()));
        }
        std::fs::create_dir_all(&job.output_dir).unwrap();
        let path = job.output_dir.join("cat.png");
        std::fs::write(&path, b"png").unwrap();
        Ok(MediaOutput {
            paths: vec![path],
            detail: serde_json::json!({}),
        })
    }
    async fn unload(&self) -> Result<(), MediaError> {
        self.0.0.lock().unwrap().push("comfy unload".into());
        Ok(())
    }
}

#[tokio::test]
async fn generate_image_takes_the_lease_and_returns_paths() {
    let log = Arc::new(Log::default());
    let lease = Arc::new(
        LeaseManager::new(
            Arc::new(FakeLlm::new(log.clone())),
            vec!["m".to_owned()],
            Gate::new(),
            Arc::new(|_| {}),
        )
        .with_media(Arc::new(FakeComfy(log.clone()))),
    );
    let out = tempfile::tempdir().unwrap();
    let workflows = BTreeMap::from([(
        "default".to_owned(),
        WorkflowTemplate::new(serde_json::json!({
            "1": {"inputs": {"text": "{{prompt}}", "seed": "{{seed}}"}},
            "2": {"inputs": {"width": "{{width}}", "height": "{{height}}"}}
        })),
    )]);
    let handle = serve(
        MediaTools::new(lease, out.path()).with_images("comfyui", workflows),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let transport = StreamableHttpClientTransport::from_uri(handle.url());
    let client = ().serve(transport).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["generate_image", "list_media_engines"]);

    let result = client
        .call_tool(
            CallToolRequestParams::new("generate_image").with_arguments(
                serde_json::json!({"prompt": "a cat", "seed": 7, "width": 768})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let text = serde_json::to_value(&result.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.starts_with("Generated 1 file(s):\n"), "{text}");
    assert!(text.contains("cat.png"));
    assert_eq!(result.structured_content.as_ref().unwrap()["seed"], 7);
    assert_eq!(
        *log.0.lock().unwrap(),
        vec![
            "llm unload",
            // A size the call leaves out is 1024, not the bare placeholder.
            "job: a cat seed=7 size=768x1024",
            "comfy unload",
            "llm load"
        ]
    );

    let failed = client
        .call_tool(
            CallToolRequestParams::new("generate_image").with_arguments(
                serde_json::json!({"prompt": "please fail"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await;
    match failed {
        Ok(result) => assert_eq!(result.is_error, Some(true), "{result:?}"),
        Err(error) => assert!(error.to_string().contains("bad prompt"), "{error}"),
    }
    let unknown = client
        .call_tool(
            CallToolRequestParams::new("generate_image").with_arguments(
                serde_json::json!({"prompt": "x", "workflow": "nope"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await;
    assert!(unknown.is_err() || unknown.unwrap().is_error == Some(true));

    let listed = client
        .call_tool(CallToolRequestParams::new("list_media_engines"))
        .await
        .unwrap();
    let text = serde_json::to_value(&listed.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(text, "engines: comfyui\nworkflows: default");
    client.cancel().await.unwrap();
    handle.stop().await;
}

#[tokio::test]
async fn without_workflows_the_request_goes_to_the_engine_as_it_is() {
    let log = Arc::new(Log::default());
    let lease = Arc::new(
        LeaseManager::new(
            Arc::new(FakeLlm::new(log.clone())),
            vec!["m".to_owned()],
            Gate::new(),
            Arc::new(|_| {}),
        )
        .with_media(Arc::new(FakeComfy(log.clone()))),
    );
    let out = tempfile::tempdir().unwrap();
    let handle = serve(
        MediaTools::new(lease, out.path()).with_images("comfyui", BTreeMap::new()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let transport = StreamableHttpClientTransport::from_uri(handle.url());
    let client = ().serve(transport).await.unwrap();
    let result = client
        .call_tool(
            CallToolRequestParams::new("generate_image").with_arguments(
                serde_json::json!({"prompt": "a horse", "seed": 3, "workflow": "ignored"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        log.0
            .lock()
            .unwrap()
            .contains(&"job: a horse seed=3".to_owned())
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["workflow"],
        serde_json::Value::Null
    );
    client.cancel().await.unwrap();
    handle.stop().await;
}

/// A 3D engine that writes a GLB where the job says, and records the request.
struct FakeMeshes(Arc<Log>);
#[async_trait]
impl MediaAdapter for FakeMeshes {
    fn name(&self) -> &str {
        "mesh-api"
    }
    async fn load(&self) -> Result<(), MediaError> {
        Ok(())
    }
    async fn run(&self, job: MediaJob) -> Result<MediaOutput, MediaError> {
        let request = &job.params["request"];
        self.0.0.lock().unwrap().push(format!(
            "mesh: {} -> {} resolution={} seed={} steps={} mode={}",
            request["image"].as_str().unwrap(),
            request["output"].as_str().unwrap(),
            request["resolution"],
            request["seed"],
            request["steps"],
            request["mode"]
        ));
        if request["image"].as_str().unwrap().contains("fail") {
            return Err(MediaError::Failed("boom".into()));
        }
        let output = std::path::PathBuf::from(request["output"].as_str().unwrap());
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, b"glTF").unwrap();
        Ok(MediaOutput {
            paths: vec![output],
            detail: serde_json::json!({"model": "Idol", "bytes": 4}),
        })
    }
    async fn unload(&self) -> Result<(), MediaError> {
        Ok(())
    }
}

fn arguments(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let serde_json::Value::Object(arguments) = value else {
        panic!("arguments are an object: {value}");
    };
    arguments
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

/// A server with an image engine and a 3D engine, over a project in a
/// temporary directory with two pictures and a secret.
struct MeshServer {
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    handle: tenaz_mcp::McpHandle,
    log: Arc<Log>,
    root: std::path::PathBuf,
    _project: tempfile::TempDir,
}

impl MeshServer {
    async fn start() -> Self {
        let log = Arc::new(Log::default());
        let lease = Arc::new(
            LeaseManager::new(
                Arc::new(FakeLlm::new(log.clone())),
                vec!["m".to_owned()],
                Gate::new(),
                Arc::new(|_| {}),
            )
            .with_media(Arc::new(FakeMeshes(log.clone())))
            .with_media(Arc::new(FakeComfy(log.clone()))),
        );
        let project = tempfile::tempdir().unwrap();
        let root = project.path().to_path_buf();
        std::fs::create_dir_all(root.join("photos")).unwrap();
        for name in ["cabinet.png", "fail.png"] {
            std::fs::write(root.join("photos").join(name), PNG).unwrap();
        }
        std::fs::write(root.join(".env"), b"API_KEY=secret").unwrap();
        let tools = MediaTools::new(lease, root.join(".tenaz/private/media"))
            .with_images("comfyui", BTreeMap::new())
            .with_meshes("mesh-api", &root);
        let handle = serve(tools, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let transport = StreamableHttpClientTransport::from_uri(handle.url());
        let client = ().serve(transport).await.unwrap();
        Self {
            client,
            handle,
            log,
            root,
            _project: project,
        }
    }

    async fn generate_3d(
        &self,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ServiceError> {
        self.client
            .call_tool(CallToolRequestParams::new("generate_3d").with_arguments(arguments(args)))
            .await
    }

    async fn stop(self) {
        self.client.cancel().await.unwrap();
        self.handle.stop().await;
    }
}

#[tokio::test]
async fn each_tool_reaches_its_own_engine() {
    let server = MeshServer::start().await;
    let tools = server.client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["generate_3d", "generate_image", "list_media_engines"]
    );
    server
        .client
        .call_tool(
            CallToolRequestParams::new("generate_image")
                .with_arguments(arguments(serde_json::json!({"prompt": "a cat", "seed": 1}))),
        )
        .await
        .unwrap();
    server
        .generate_3d(serde_json::json!({"image": "photos/cabinet.png"}))
        .await
        .unwrap();
    let log = server.log.0.lock().unwrap().clone();
    let jobs: Vec<&String> = log
        .iter()
        .filter(|l| l.starts_with("job:") || l.starts_with("mesh:"))
        .collect();
    assert_eq!(jobs.len(), 2, "{log:?}");
    assert_eq!(jobs[0], "job: a cat seed=1");
    assert!(
        jobs[1].starts_with("mesh: ") && jobs[1].contains("photos/cabinet.png"),
        "{log:?}"
    );
    server.stop().await;
}

/// A run with only a 3D engine lists only `generate_3d`, and tells the
/// agent only about it.
#[tokio::test]
async fn a_run_with_only_a_3d_engine_offers_only_generate_3d() {
    let log = Arc::new(Log::default());
    let lease = Arc::new(LeaseManager::new(
        Arc::new(FakeLlm::new(log.clone())),
        vec!["m".to_owned()],
        Gate::new(),
        Arc::new(|_| {}),
    ));
    let root = tempfile::tempdir().unwrap();
    let handle = serve(
        MediaTools::new(lease, root.path()).with_meshes("mesh-api", root.path()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let transport = StreamableHttpClientTransport::from_uri(handle.url());
    let client = ().serve(transport).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["generate_3d", "list_media_engines"]);
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    assert!(
        instructions.contains("generate_3d") && !instructions.contains("generate_image"),
        "{instructions}"
    );
    client.cancel().await.unwrap();
    handle.stop().await;
}

#[tokio::test]
async fn a_failed_mesh_job_says_so_and_brings_the_llm_back() {
    let server = MeshServer::start().await;
    let answer = server
        .generate_3d(serde_json::json!({"image": "photos/fail.png"}))
        .await;
    let message = match answer {
        Ok(result) => {
            assert_eq!(result.is_error, Some(true), "{result:?}");
            format!("{:?}", result.content)
        }
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("3D generation failed") && message.contains("boom"),
        "{message}"
    );
    assert_eq!(
        server.log.0.lock().unwrap().last().map(String::as_str),
        Some("llm load")
    );
    server.stop().await;
}

#[tokio::test]
async fn generate_3d_writes_the_glb_where_asked_under_the_lease() {
    let server = MeshServer::start().await;
    let result = server
        .generate_3d(serde_json::json!({"image": "photos/cabinet.png", "output": "assets/cabinet.glb", "resolution": 512, "seed": 7, "steps": 12, "mode": "real"}))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let glb = server.root.join("assets/cabinet.glb");
    assert_eq!(
        result.structured_content.as_ref().unwrap()["path"],
        glb.display().to_string()
    );
    assert_eq!(std::fs::read(&glb).unwrap(), b"glTF");
    assert_eq!(
        *server.log.0.lock().unwrap(),
        vec![
            "llm unload".to_owned(),
            format!(
                "mesh: {} -> {} resolution=512 seed=7 steps=12 mode=\"real\"",
                server.root.join("photos/cabinet.png").display(),
                glb.display()
            ),
            "llm load".to_owned(),
        ]
    );
    server.stop().await;
}

#[tokio::test]
async fn generate_3d_without_an_output_goes_with_the_runs_media() {
    let server = MeshServer::start().await;
    let result = server
        .generate_3d(serde_json::json!({"image": "photos/cabinet.png"}))
        .await
        .unwrap();
    let path = result.structured_content.unwrap()["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let media = server
        .root
        .join(".tenaz/private/media")
        .display()
        .to_string();
    assert!(
        path.starts_with(&media) && path.ends_with("/mesh.glb"),
        "{path}"
    );
    server.stop().await;
}

#[tokio::test]
async fn generate_3d_refuses_paths_outside_the_project_without_the_lease() {
    let server = MeshServer::start().await;
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("cabinet.glb").display().to_string();
    std::os::unix::fs::symlink(elsewhere.path(), server.root.join("linked")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), server.root.join("outside")).unwrap();
    std::fs::write(elsewhere.path().join("cabinet.png"), PNG).unwrap();
    std::fs::write(elsewhere.path().join(".env"), b"API_KEY=secret").unwrap();
    std::fs::create_dir_all(server.root.join(".tenaz")).unwrap();
    std::os::unix::fs::symlink(server.root.join(".tenaz"), server.root.join("state")).unwrap();
    for (refused, why) in [
        (
            serde_json::json!({"image": "../elsewhere.png"}),
            "outside the project",
        ),
        (
            serde_json::json!({"image": "photos/none.png"}),
            "photos/none.png: No such file",
        ),
        (serde_json::json!({"image": ".env"}), "not a PNG or JPEG"),
        (
            serde_json::json!({"image": "photos/cabinet.png", "output": "assets/cabinet.obj"}),
            ".glb",
        ),
        (
            serde_json::json!({"image": "photos/cabinet.png", "output": outside.clone()}),
            "outside the project",
        ),
        (
            serde_json::json!({"image": "photos/cabinet.png", "output": "linked/cabinet.glb"}),
            "symbolic link",
        ),
        (
            serde_json::json!({"image": "photos/cabinet.png", "output": ".tenaz/run.glb"}),
            ".tenaz",
        ),
        (
            serde_json::json!({"image": "photos/cabinet.png", "output": "state/run.glb"}),
            ".tenaz",
        ),
        (
            serde_json::json!({"image": "outside/.env"}),
            "symbolic link",
        ),
        (
            serde_json::json!({"image": "outside/cabinet.png"}),
            "symbolic link",
        ),
    ] {
        let message = match server.generate_3d(refused.clone()).await {
            Ok(result) => {
                assert_eq!(result.is_error, Some(true), "{result:?}");
                format!("{:?}", result.content)
            }
            Err(error) => error.to_string(),
        };
        assert!(message.contains(why), "{refused}: {message}");
    }
    assert!(
        server.log.0.lock().unwrap().is_empty(),
        "a refused call took the lease"
    );
    server.stop().await;
}
