//! An image-to-3D API (`POST /v1/3d/generations`), as mlx-serve serves it
//! (ADR 0020). The picture goes as base64, the GLB comes back as base64
//! and is written where the job says. `unload` frees the model through
//! the server's `/v1/unload-model`, since mlx-serve keeps it loaded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use tenaz_core::ports::{MediaAdapter, MediaError, MediaJob, MediaOutput};

/// The model options a job may carry, sent as they are.
const OPTIONS: [&str; 4] = ["resolution", "seed", "steps", "mode"];

#[derive(Clone, Debug)]
pub struct MeshApi {
    base_url: String,
    model: String,
    client: reqwest::Client,
    job_timeout: Duration,
    shares_engine: bool,
}

impl MeshApi {
    /// `base_url` such as `http://127.0.0.1:11234/v1`.
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

    /// The request body: the model, the picture and the options given.
    fn body(&self, request: &serde_json::Value, image: &[u8]) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "image": base64::engine::general_purpose::STANDARD.encode(image),
        });
        for key in OPTIONS {
            if let Some(value) = request.get(key).filter(|v| !v.is_null()) {
                body[key] = value.clone();
            }
        }
        body
    }

    async fn post(&self, body: &serde_json::Value) -> Result<serde_json::Value, MediaError> {
        let response = self
            .client
            .post(self.url("/3d/generations"))
            .json(body)
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
                "/3d/generations returned {status}: {}",
                tenaz_core::util::clip(text.trim(), 500)
            )));
        }
        serde_json::from_str(&text).map_err(|e| MediaError::Failed(format!("/3d/generations: {e}")))
    }
}

/// The GLB in the answer: `format` must be `glb` and `data` a base64 GLB.
fn glb(answer: &serde_json::Value) -> Result<Vec<u8>, MediaError> {
    let format = answer.get("format").and_then(serde_json::Value::as_str);
    if format != Some("glb") {
        return Err(MediaError::Failed(format!(
            "/3d/generations returned format {format:?}, not glb"
        )));
    }
    let data = answer
        .get("data")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MediaError::Failed("/3d/generations returned no data".to_owned()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| MediaError::Failed(format!("/3d/generations data: {e}")))?;
    if tenaz_core::mesh::is_glb(&bytes) {
        Ok(bytes)
    } else {
        Err(MediaError::Failed(
            "/3d/generations returned data that is not a GLB".to_owned(),
        ))
    }
}

fn path_param(request: &serde_json::Value, key: &str) -> Result<PathBuf, MediaError> {
    request
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| MediaError::Failed(format!("the job has no `{key}`")))
}

/// Writes the GLB next to `target` and moves it in place, so a failed
/// write never leaves a truncated mesh where an earlier one was.
fn write(target: &Path, bytes: &[u8]) -> Result<(), MediaError> {
    let failed = |e: std::io::Error| MediaError::Failed(format!("{}: {e}", target.display()));
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    // A fresh file: an old partial, or a link put in its place, is removed
    // first and never written through.
    let partial = target.with_extension("glb.partial");
    match std::fs::remove_file(&partial) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(failed(e)),
        _ => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .and_then(|mut file| std::io::Write::write_all(&mut file, bytes))
        .map_err(failed)?;
    std::fs::rename(&partial, target).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        failed(e)
    })
}

#[async_trait]
impl MediaAdapter for MeshApi {
    fn name(&self) -> &str {
        "mesh-api"
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

    /// `job.params.request` holds `image` and `output` (absolute paths),
    /// and optionally `resolution`, `seed`, `steps` and `mode`.
    async fn run(&self, job: MediaJob) -> Result<MediaOutput, MediaError> {
        let request = job
            .params
            .get("request")
            .ok_or_else(|| MediaError::Failed("the job has no `request`".to_owned()))?;
        let image_path = path_param(request, "image")?;
        let output = path_param(request, "output")?;
        let image = std::fs::read(&image_path)
            .map_err(|e| MediaError::Failed(format!("{}: {e}", image_path.display())))?;
        if !tenaz_core::mesh::is_picture(&image) {
            return Err(MediaError::Failed(format!(
                "{} is not a PNG or JPEG",
                image_path.display()
            )));
        }
        let answer = self.post(&self.body(request, &image)).await?;
        let bytes = glb(&answer)?;
        write(&output, &bytes)?;
        Ok(MediaOutput {
            paths: vec![output],
            detail: serde_json::json!({ "model": self.model, "bytes": bytes.len() }),
        })
    }

    /// Frees the model, also when the LLM's own server serves it: that
    /// server keeps it loaded otherwise, next to the LLM.
    async fn unload(&self) -> Result<(), MediaError> {
        super::unload_model(&self.client, &self.base_url, &self.model).await
    }

    fn shares_engine(&self) -> bool {
        self.shares_engine
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};

    use super::*;

    const GLB: &[u8] = b"glTF\x02\0\0\0fake mesh";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    #[derive(Clone, Default)]
    struct Fake {
        bodies: Arc<Mutex<Vec<serde_json::Value>>>,
        unloads: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    async fn server(status: StatusCode, reply: serde_json::Value) -> (String, Fake) {
        let fake = Fake::default();
        let app = axum::Router::new()
            .route(
                "/v1/models",
                get(|| async { axum::Json(serde_json::json!({"data": [{"id": "Idol"}]})) }),
            )
            .route(
                "/v1/3d/generations",
                post(
                    move |State(f): State<Fake>,
                          axum::Json(body): axum::Json<serde_json::Value>| {
                        let reply = reply.clone();
                        async move {
                            f.bodies.lock().unwrap().push(body);
                            (status, axum::Json(reply))
                        }
                    },
                ),
            )
            .route(
                "/v1/unload-model",
                post(
                    |State(f): State<Fake>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                        f.unloads.lock().unwrap().push(body.clone());
                        axum::Json(serde_json::json!({"model": {"id": body["model"], "state": "unloaded"}}))
                    },
                ),
            )
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://127.0.0.1:{port}/v1"), fake)
    }

    /// A job for the picture `cabinet.png` in `dir`, writing `out/cabinet.glb`.
    fn job(dir: &Path, options: &serde_json::Value) -> MediaJob {
        std::fs::write(dir.join("cabinet.png"), PNG).unwrap();
        let mut request = serde_json::json!({
            "image": dir.join("cabinet.png"),
            "output": dir.join("out/cabinet.glb"),
        });
        for (key, value) in options.as_object().unwrap() {
            request[key] = value.clone();
        }
        MediaJob {
            kind: "mesh".into(),
            params: serde_json::json!({ "request": request }),
            output_dir: dir.join("out"),
        }
    }

    fn reply(data: &[u8]) -> serde_json::Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        serde_json::json!({"created": 0, "format": "glb", "data": b64})
    }

    #[tokio::test]
    async fn sends_the_picture_and_writes_the_glb() {
        let (url, fake) = server(StatusCode::OK, reply(GLB)).await;
        let dir = tempfile::tempdir().unwrap();
        let meshes = MeshApi::new(&url, "TRELLIS.2", Duration::from_secs(10));
        meshes.load().await.unwrap();
        let output = meshes
            .run(job(
                dir.path(),
                &serde_json::json!({"resolution": 512, "seed": 7, "steps": 12, "mode": null}),
            ))
            .await
            .unwrap();
        let target = dir.path().join("out/cabinet.glb");
        assert_eq!(output.paths, vec![target.clone()]);
        assert_eq!(std::fs::read(&target).unwrap(), GLB);
        assert!(!target.with_extension("glb.partial").exists());
        assert_eq!(
            fake.bodies.lock().unwrap()[0],
            serde_json::json!({
                "model": "TRELLIS.2",
                "image": base64::engine::general_purpose::STANDARD.encode(PNG),
                "resolution": 512,
                "seed": 7,
                "steps": 12
            })
        );
    }

    /// mlx-serve keeps a 3D model loaded after its job; `unload` frees the
    /// run's model, also on the LLM's own server.
    #[tokio::test]
    async fn unload_frees_the_run_s_model_on_the_server() {
        let (url, fake) = server(StatusCode::OK, reply(GLB)).await;
        let meshes =
            MeshApi::new(&url, "TRELLIS.2", Duration::from_secs(10)).sharing_the_engine(true);
        meshes.unload().await.unwrap();
        assert_eq!(
            *fake.unloads.lock().unwrap(),
            vec![serde_json::json!({"model": "TRELLIS.2"})]
        );
    }

    #[test]
    fn it_shares_the_engine_only_when_told() {
        let meshes = MeshApi::new("http://127.0.0.1:9/v1", "Idol", Duration::from_secs(1));
        assert!(!meshes.shares_engine());
        assert!(meshes.sharing_the_engine(true).shares_engine());
    }

    #[tokio::test]
    async fn a_server_that_is_gone_is_unreachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let meshes = MeshApi::new(
            format!("http://127.0.0.1:{port}/v1"),
            "Idol",
            Duration::from_secs(5),
        );
        assert!(matches!(
            meshes.load().await,
            Err(MediaError::Unreachable(_))
        ));
        let run = meshes.run(job(dir.path(), &serde_json::json!({}))).await;
        assert!(matches!(run, Err(MediaError::Unreachable(_))), "{run:?}");
    }

    /// A link where the partial file goes is replaced, not written through.
    #[tokio::test]
    async fn a_link_in_place_of_the_partial_file_is_not_followed() {
        let (url, _) = server(StatusCode::OK, reply(GLB)).await;
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::fs::create_dir_all(dir.path().join("out")).unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("out/cabinet.glb.partial")).unwrap();
        let meshes = MeshApi::new(&url, "Idol", Duration::from_secs(10));
        meshes
            .run(job(dir.path(), &serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(
            std::fs::read(dir.path().join("out/cabinet.glb")).unwrap(),
            GLB
        );
    }

    #[tokio::test]
    async fn a_refused_request_returns_the_server_s_reason() {
        let (url, _) = server(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "resolution 512 needs the 512 texture model"}),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let meshes = MeshApi::new(&url, "TRELLIS.2", Duration::from_secs(10));
        let error = meshes
            .run(job(dir.path(), &serde_json::json!({"resolution": 512})))
            .await
            .unwrap_err();
        assert!(
            matches!(error, MediaError::Failed(ref m) if m.contains("400") && m.contains("512 texture model")),
            "{error}"
        );
        assert!(!dir.path().join("out").exists());
    }

    #[tokio::test]
    async fn an_answer_that_is_not_a_glb_writes_nothing() {
        for (answer, why) in [
            (reply(b"{\"asset\":{}}"), "not a GLB"),
            (serde_json::json!({"format": "ply", "data": ""}), "not glb"),
            (serde_json::json!({"format": "glb"}), "no data"),
        ] {
            let (url, _) = server(StatusCode::OK, answer.clone()).await;
            let dir = tempfile::tempdir().unwrap();
            let meshes = MeshApi::new(&url, "Idol", Duration::from_secs(10));
            let error = meshes
                .run(job(dir.path(), &serde_json::json!({})))
                .await
                .unwrap_err();
            assert!(
                matches!(error, MediaError::Failed(ref m) if m.contains(why)),
                "{answer}: {error}"
            );
            assert!(!dir.path().join("out").exists(), "{answer}");
        }
    }

    #[tokio::test]
    async fn a_missing_picture_fails_before_the_request() {
        let (url, fake) = server(StatusCode::OK, reply(GLB)).await;
        let dir = tempfile::tempdir().unwrap();
        let meshes = MeshApi::new(&url, "Idol", Duration::from_secs(10));
        let mut missing = job(dir.path(), &serde_json::json!({}));
        missing.params["request"]["image"] =
            dir.path().join("none.png").display().to_string().into();
        let error = meshes.run(missing).await.unwrap_err();
        assert!(
            matches!(error, MediaError::Failed(ref m) if m.contains("none.png")),
            "{error}"
        );
        assert!(fake.bodies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_job_without_a_request_fails() {
        let meshes = MeshApi::new("http://127.0.0.1:9/v1", "Idol", Duration::from_secs(1));
        let no_request = MediaJob {
            kind: "mesh".into(),
            params: serde_json::json!({}),
            output_dir: std::env::temp_dir(),
        };
        let error = meshes.run(no_request).await.unwrap_err();
        assert!(
            matches!(error, MediaError::Failed(ref m) if m.contains("`request`")),
            "{error}"
        );
    }
}
