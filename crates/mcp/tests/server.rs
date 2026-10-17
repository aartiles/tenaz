//! The media MCP server over streamable HTTP, called with the rmcp client,
//! against fake engines under a real lease manager.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use smr_core::lease::{Gate, LeaseManager};
use smr_core::ports::{
    EngineAdapter, EngineError, MediaAdapter, MediaError, MediaJob, MediaOutput, ModelState,
};
use smr_mcp::workflow::WorkflowTemplate;
use smr_mcp::{MediaTools, serve};

#[derive(Default)]
struct Log(Mutex<Vec<String>>);

struct FakeLlm(Arc<Log>);
#[async_trait]
impl EngineAdapter for FakeLlm {
    fn name(&self) -> &str {
        "fake"
    }
    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        Ok(vec![ModelState {
            id: "m".into(),
            loaded: true,
            ready: true,
            context_length: None,
        }])
    }
    async fn load(&self, _model: &str) -> Result<(), EngineError> {
        self.0.0.lock().unwrap().push("llm load".into());
        Ok(())
    }
    async fn unload(&self, _model: &str) -> Result<(), EngineError> {
        self.0.0.lock().unwrap().push("llm unload".into());
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
        let text = job.params["workflow"]["1"]["inputs"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.0.0.lock().unwrap().push(format!(
            "job: {text} seed={}",
            job.params["workflow"]["1"]["inputs"]["seed"]
        ));
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
            Arc::new(FakeLlm(log.clone())),
            "m",
            Gate::new(),
            Arc::new(|_| {}),
        )
        .with_media(Arc::new(FakeComfy(log.clone()))),
    );
    let out = tempfile::tempdir().unwrap();
    let workflows = BTreeMap::from([(
        "default".to_owned(),
        WorkflowTemplate::new(
            serde_json::json!({"1": {"inputs": {"text": "{{prompt}}", "seed": "{{seed}}"}}}),
        ),
    )]);
    let handle = serve(
        MediaTools::new(lease, out.path(), workflows, "comfyui"),
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
                serde_json::json!({"prompt": "a cat", "seed": 7})
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
            "job: a cat seed=7",
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
