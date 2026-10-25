//! An `OpenAI` Images API (`POST /v1/images/generations`), as local servers
//! such as `Rapid-MLX`, mlx-omni-server and `LocalAI` serve it. The image comes
//! back as base64 and is written to the job's output directory. The API has
//! no way to free the model, so `unload` does nothing.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use tenaz_core::ports::{MediaAdapter, MediaError, MediaJob, MediaOutput};

#[derive(Clone, Debug)]
pub struct OpenAiImages {
    base_url: String,
    model: String,
    client: reqwest::Client,
    job_timeout: Duration,
    shares_engine: bool,
}

impl OpenAiImages {
    /// `base_url` such as `http://127.0.0.1:8000/v1`.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        job_timeout: Duration,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            client: crate::http::local_client(),
            job_timeout,
            shares_engine: false,
        }
    }

    /// The LLM's own engine serves this API: the lease keeps the LLM loaded.
    #[must_use]
    pub fn sharing_the_engine(mut self, shares: bool) -> Self {
        self.shares_engine = shares;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// The request body: the standard fields, plus a seed and a negative
    /// prompt that servers which do not know them ignore.
    fn body(&self, request: &serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "prompt": request.get("prompt").cloned().unwrap_or_default(),
            "n": 1,
            "response_format": "b64_json",
        });
        let dimension = |key: &str| request.get(key).and_then(serde_json::Value::as_u64);
        if let (Some(width), Some(height)) = (dimension("width"), dimension("height")) {
            body["size"] = format!("{width}x{height}").into();
        }
        if let Some(seed) = request.get("seed").filter(|s| s.is_u64()) {
            body["seed"] = seed.clone();
        }
        if let Some(negative) = request
            .get("negative_prompt")
            .and_then(serde_json::Value::as_str)
            .filter(|n| !n.is_empty())
        {
            body["negative_prompt"] = negative.into();
        }
        body
    }

    /// Writes each `data[].b64_json`, or downloads each `data[].url`.
    async fn save(
        &self,
        data: &[serde_json::Value],
        output_dir: &Path,
    ) -> Result<Vec<std::path::PathBuf>, MediaError> {
        std::fs::create_dir_all(output_dir)
            .map_err(|e| MediaError::Failed(format!("{}: {e}", output_dir.display())))?;
        let mut paths = Vec::new();
        for (n, item) in data.iter().enumerate() {
            let bytes = if let Some(b64) = item.get("b64_json").and_then(serde_json::Value::as_str)
            {
                base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|e| MediaError::Failed(format!("b64_json: {e}")))?
            } else if let Some(url) = item.get("url").and_then(serde_json::Value::as_str) {
                self.client
                    .get(url)
                    .timeout(Duration::from_secs(120))
                    .send()
                    .await
                    .map_err(|e| MediaError::Unreachable(e.to_string()))?
                    .bytes()
                    .await
                    .map_err(|e| MediaError::Failed(format!("{url}: {e}")))?
                    .to_vec()
            } else {
                continue;
            };
            let target = output_dir.join(format!("image-{}.{}", n + 1, extension(&bytes)));
            std::fs::write(&target, &bytes)
                .map_err(|e| MediaError::Failed(format!("{}: {e}", target.display())))?;
            paths.push(target);
        }
        Ok(paths)
    }
}

/// The file extension the image's first bytes say it has.
fn extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        "jpg"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "webp"
    } else {
        "png"
    }
}

#[async_trait]
impl MediaAdapter for OpenAiImages {
    fn name(&self) -> &str {
        "openai-images"
    }

    /// Checks that the server answers; it loads the model on the first request.
    async fn load(&self) -> Result<(), MediaError> {
        self.client
            .get(self.url("/models"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| MediaError::Unreachable(e.to_string()))?
            .error_for_status()
            .map(|_| ())
            .map_err(|e| MediaError::Unreachable(e.to_string()))
    }

    /// `job.params.request` holds `prompt`, and optionally `negative_prompt`,
    /// `seed`, `width` and `height`.
    async fn run(&self, job: MediaJob) -> Result<MediaOutput, MediaError> {
        let request = job
            .params
            .get("request")
            .ok_or_else(|| MediaError::Failed("the job has no `request`".to_owned()))?;
        let response = self
            .client
            .post(self.url("/images/generations"))
            .json(&self.body(request))
            .timeout(self.job_timeout)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    MediaError::Timeout(self.job_timeout)
                } else {
                    MediaError::Unreachable(e.to_string())
                }
            })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MediaError::Failed(format!(
                "/images/generations returned {status}: {}",
                text.trim()
            )));
        }
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| MediaError::Failed(format!("/images/generations: {e}")))?;
        let data = value
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let paths = self.save(&data, &job.output_dir).await?;
        if paths.is_empty() {
            return Err(MediaError::Failed(format!(
                "/images/generations returned no image: {}",
                tenaz_core::util::clip(text.trim(), 500)
            )));
        }
        Ok(MediaOutput {
            paths,
            detail: serde_json::json!({ "model": self.model }),
        })
    }

    async fn unload(&self) -> Result<(), MediaError> {
        Ok(())
    }

    fn shares_engine(&self) -> bool {
        self.shares_engine
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::routing::{get, post};

    use super::*;

    /// A one-pixel PNG.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    #[derive(Clone, Default)]
    struct Fake {
        bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    async fn server(reply: serde_json::Value) -> (String, Fake) {
        let fake = Fake::default();
        let app = axum::Router::new()
            .route(
                "/v1/models",
                get(|| async { axum::Json(serde_json::json!({"data": [{"id": "flux2-klein"}]})) }),
            )
            .route(
                "/v1/images/generations",
                post(
                    move |State(f): State<Fake>,
                          axum::Json(body): axum::Json<serde_json::Value>| {
                        let reply = reply.clone();
                        async move {
                            f.bodies.lock().unwrap().push(body);
                            axum::Json(reply)
                        }
                    },
                ),
            )
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://127.0.0.1:{port}/v1"), fake)
    }

    fn job(dir: &Path, request: &serde_json::Value) -> MediaJob {
        MediaJob {
            kind: "image".into(),
            params: serde_json::json!({ "request": request }),
            output_dir: dir.join("out"),
        }
    }

    #[tokio::test]
    async fn generates_an_image_and_writes_it() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        let (url, fake) = server(serde_json::json!({"data": [{"b64_json": b64}]})).await;
        let dir = tempfile::tempdir().unwrap();
        let images = OpenAiImages::new(&url, "flux2-klein", Duration::from_secs(10));
        images.load().await.unwrap();
        let output = images
            .run(job(
                dir.path(),
                &serde_json::json!({"prompt": "a cowboy at noon", "negative_prompt": "blur", "seed": 7, "width": 512, "height": 768}),
            ))
            .await
            .unwrap();
        assert_eq!(output.paths, vec![dir.path().join("out/image-1.png")]);
        assert_eq!(std::fs::read(&output.paths[0]).unwrap(), PNG);
        assert_eq!(
            fake.bodies.lock().unwrap()[0],
            serde_json::json!({
                "model": "flux2-klein",
                "prompt": "a cowboy at noon",
                "n": 1,
                "response_format": "b64_json",
                "size": "512x768",
                "seed": 7,
                "negative_prompt": "blur"
            })
        );
        images.unload().await.unwrap();
        assert!(!images.shares_engine());
        assert!(images.sharing_the_engine(true).shares_engine());
    }

    #[tokio::test]
    async fn only_the_prompt_is_required_and_an_empty_reply_fails() {
        let (url, fake) = server(serde_json::json!({"data": []})).await;
        let dir = tempfile::tempdir().unwrap();
        let images = OpenAiImages::new(&url, "m", Duration::from_secs(10));
        let error = images
            .run(job(dir.path(), &serde_json::json!({"prompt": "a horse"})))
            .await
            .unwrap_err();
        assert!(
            matches!(error, MediaError::Failed(ref m) if m.contains("no image")),
            "{error}"
        );
        assert_eq!(
            fake.bodies.lock().unwrap()[0],
            serde_json::json!({"model": "m", "prompt": "a horse", "n": 1, "response_format": "b64_json"})
        );
        let missing = images
            .run(MediaJob {
                kind: "image".into(),
                params: serde_json::json!({}),
                output_dir: dir.path().to_path_buf(),
            })
            .await
            .unwrap_err();
        assert!(matches!(missing, MediaError::Failed(_)));
    }

    #[test]
    fn the_extension_follows_the_image_format() {
        assert_eq!(extension(PNG), "png");
        assert_eq!(extension(b"\xFF\xD8\xFF\xE0rest"), "jpg");
        assert_eq!(extension(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
    }
}
