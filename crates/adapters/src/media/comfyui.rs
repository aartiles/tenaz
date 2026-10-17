//! `ComfyUI`: queue a workflow through `/prompt`, poll `/history`, download
//! the outputs through `/view`, then free its models through `/free`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use smr_core::ports::{MediaAdapter, MediaError, MediaJob, MediaOutput};

#[derive(Clone, Debug)]
pub struct ComfyUi {
    base_url: String,
    client: reqwest::Client,
    client_id: String,
    poll: Duration,
    job_timeout: Duration,
}

impl ComfyUi {
    /// `base_url` such as `http://127.0.0.1:8188`.
    pub fn new(base_url: impl Into<String>, job_timeout: Duration) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: crate::http::local_client(),
            client_id: format!("smr-{}", std::process::id()),
            poll: Duration::from_secs(2),
            job_timeout,
        }
    }

    #[must_use]
    pub fn with_poll_interval(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn queue(&self, workflow: &serde_json::Value) -> Result<String, MediaError> {
        let body = serde_json::json!({ "prompt": workflow, "client_id": self.client_id });
        let response = self
            .client
            .post(self.url("/prompt"))
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| MediaError::Unreachable(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MediaError::Failed(format!(
                "/prompt returned {status}: {}",
                text.trim()
            )));
        }
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| MediaError::Failed(format!("/prompt: {e}")))?;
        value
            .get("prompt_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                MediaError::Failed(format!("/prompt gave no prompt_id: {}", text.trim()))
            })
    }

    /// Polls `/history/<id>` until the job completes or fails.
    async fn wait(&self, prompt_id: &str) -> Result<serde_json::Value, MediaError> {
        let started = Instant::now();
        loop {
            let response = self
                .client
                .get(self.url(&format!("/history/{prompt_id}")))
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| MediaError::Unreachable(e.to_string()))?;
            let history: serde_json::Value = response
                .json()
                .await
                .map_err(|e| MediaError::Failed(format!("/history: {e}")))?;
            if let Some(entry) = history.get(prompt_id) {
                let status = entry.get("status").cloned().unwrap_or_default();
                if status.get("status_str").and_then(serde_json::Value::as_str) == Some("error") {
                    return Err(MediaError::Failed(format!(
                        "ComfyUI reported an error: {}",
                        status
                            .get("messages")
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    )));
                }
                if status.get("completed").and_then(serde_json::Value::as_bool) == Some(true)
                    || entry
                        .get("outputs")
                        .is_some_and(|o| !o.as_object().is_none_or(serde_json::Map::is_empty))
                {
                    return Ok(entry.get("outputs").cloned().unwrap_or_default());
                }
            }
            if started.elapsed() >= self.job_timeout {
                return Err(MediaError::Timeout(self.job_timeout));
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Downloads every `images[]`/`gifs[]`/`audio[]` entry of the outputs.
    async fn download(
        &self,
        outputs: &serde_json::Value,
        output_dir: &PathBuf,
    ) -> Result<Vec<PathBuf>, MediaError> {
        std::fs::create_dir_all(output_dir)
            .map_err(|e| MediaError::Failed(format!("{}: {e}", output_dir.display())))?;
        let mut paths = Vec::new();
        let Some(nodes) = outputs.as_object() else {
            return Ok(paths);
        };
        for node in nodes.values() {
            for key in ["images", "gifs", "audio", "videos"] {
                for file in node
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let filename = file
                        .get("filename")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if filename.is_empty() {
                        continue;
                    }
                    let subfolder = file
                        .get("subfolder")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let kind = file
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("output");
                    let bytes = self
                        .client
                        .get(self.url("/view"))
                        .query(&[
                            ("filename", filename),
                            ("subfolder", subfolder),
                            ("type", kind),
                        ])
                        .timeout(Duration::from_secs(120))
                        .send()
                        .await
                        .map_err(|e| MediaError::Unreachable(e.to_string()))?
                        .bytes()
                        .await
                        .map_err(|e| MediaError::Failed(format!("/view {filename}: {e}")))?;
                    let target = output_dir.join(filename);
                    std::fs::write(&target, &bytes)
                        .map_err(|e| MediaError::Failed(format!("{}: {e}", target.display())))?;
                    paths.push(target);
                }
            }
        }
        Ok(paths)
    }
}

#[async_trait]
impl MediaAdapter for ComfyUi {
    fn name(&self) -> &str {
        "comfyui"
    }

    /// `ComfyUI` loads models lazily; `load` only checks that it answers.
    async fn load(&self) -> Result<(), MediaError> {
        self.client
            .get(self.url("/system_stats"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| MediaError::Unreachable(e.to_string()))?
            .error_for_status()
            .map(|_| ())
            .map_err(|e| MediaError::Unreachable(e.to_string()))
    }

    /// `job.params.workflow` is the API-format workflow JSON.
    async fn run(&self, job: MediaJob) -> Result<MediaOutput, MediaError> {
        let workflow = job
            .params
            .get("workflow")
            .ok_or_else(|| MediaError::Failed("the job has no `workflow`".to_owned()))?;
        let prompt_id = self.queue(workflow).await?;
        let outputs = self.wait(&prompt_id).await?;
        let paths = self.download(&outputs, &job.output_dir).await?;
        Ok(MediaOutput {
            paths,
            detail: serde_json::json!({ "prompt_id": prompt_id, "outputs": outputs }),
        })
    }

    /// Frees every model and the cached memory so the LLM can reload.
    async fn unload(&self) -> Result<(), MediaError> {
        self.client
            .post(self.url("/free"))
            .json(&serde_json::json!({ "unload_models": true, "free_memory": true }))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| MediaError::Unreachable(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, Query, State};
    use axum::routing::{get, post};

    use super::*;

    #[derive(Clone, Default)]
    struct Fake {
        polls: Arc<Mutex<u32>>,
        calls: Arc<Mutex<Vec<String>>>,
        never_completes: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn server() -> (u16, Fake) {
        let fake = Fake::default();
        let app = axum::Router::new()
            .route("/system_stats", get(|| async { axum::Json(serde_json::json!({"system": {}})) }))
            .route("/prompt", post(|State(f): State<Fake>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                f.calls.lock().unwrap().push(format!("prompt {}", body["prompt"]["1"]["inputs"]["text"]));
                axum::Json(serde_json::json!({"prompt_id": "p1", "number": 1}))
            }))
            .route("/history/{id}", get(|State(f): State<Fake>, Path(id): Path<String>| async move {
                let mut polls = f.polls.lock().unwrap();
                *polls += 1;
                if *polls < 3 || f.never_completes.load(std::sync::atomic::Ordering::SeqCst) {
                    axum::Json(serde_json::json!({}))
                } else {
                    axum::Json(serde_json::json!({ id: { "status": {"completed": true, "status_str": "success"}, "outputs": {"9": {"images": [{"filename": "cat.png", "subfolder": "", "type": "output"}]}} } }))
                }
            }))
            .route("/view", get(|Query(q): Query<std::collections::HashMap<String, String>>| async move { format!("PNG:{}", q["filename"]) }))
            .route("/free", post(|State(f): State<Fake>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                f.calls.lock().unwrap().push(format!("free {}", body["unload_models"]));
                "ok"
            }))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, fake)
    }

    #[tokio::test]
    async fn runs_a_workflow_and_downloads_the_output() {
        let (port, fake) = server().await;
        let dir = tempfile::tempdir().unwrap();
        let comfy = ComfyUi::new(format!("http://127.0.0.1:{port}"), Duration::from_secs(10))
            .with_poll_interval(Duration::from_millis(10));
        comfy.load().await.unwrap();
        let job = MediaJob {
            kind: "image".into(),
            params: serde_json::json!({"workflow": {"1": {"inputs": {"text": "a cat"}}}}),
            output_dir: dir.path().join("out"),
        };
        let output = comfy.run(job).await.unwrap();
        assert_eq!(output.paths, vec![dir.path().join("out/cat.png")]);
        assert_eq!(
            std::fs::read_to_string(&output.paths[0]).unwrap(),
            "PNG:cat.png"
        );
        assert_eq!(output.detail["prompt_id"], "p1");
        comfy.unload().await.unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec!["prompt \"a cat\"", "free true"]
        );
        let bad = comfy
            .run(MediaJob {
                kind: "image".into(),
                params: serde_json::json!({}),
                output_dir: dir.path().to_path_buf(),
            })
            .await
            .unwrap_err();
        assert!(matches!(bad, MediaError::Failed(_)));
    }

    #[tokio::test]
    async fn times_out_when_the_job_never_completes() {
        let (port, fake) = server().await;
        fake.never_completes
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let comfy = ComfyUi::new(
            format!("http://127.0.0.1:{port}"),
            Duration::from_millis(50),
        )
        .with_poll_interval(Duration::from_millis(10));
        let job = MediaJob {
            kind: "image".into(),
            params: serde_json::json!({"workflow": {}}),
            output_dir: std::env::temp_dir(),
        };
        assert!(matches!(
            comfy.run(job).await.unwrap_err(),
            MediaError::Timeout(_)
        ));
    }
}
