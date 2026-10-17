//! Sushi: `POST /v1/load-model` and `POST /v1/unload-model`, and a `/v1/models`
//! that reports `loaded`, `state` and `context_length`.

use std::time::Duration;

use async_trait::async_trait;
use smr_core::ports::{EngineAdapter, EngineError, ModelState};

use super::openai::list_models;

#[derive(Clone, Debug)]
pub struct SushiEngine {
    base_url: String,
    client: reqwest::Client,
    load_timeout: Duration,
}

impl SushiEngine {
    /// `base_url` such as `http://127.0.0.1:12345/v1`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: crate::http::local_client(),
            load_timeout: Duration::from_mins(20),
        }
    }

    async fn post(&self, path: &str, model: &str, timeout: Duration) -> Result<(), EngineError> {
        let url = format!("{}/{path}", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "model": model }))
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    EngineError::Timeout(format!("{path} {model}"))
                } else {
                    EngineError::Unreachable(e.to_string())
                }
            })?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(EngineError::Refused(format!(
                "{path} returned {status}: {}",
                body.trim()
            )))
        }
    }
}

#[async_trait]
impl EngineAdapter for SushiEngine {
    fn name(&self) -> &str {
        "sushi"
    }

    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        list_models(&self.client, &self.base_url, Duration::from_secs(5)).await
    }

    /// Returns when the model is ready (Sushi answers once the weights are mapped).
    async fn load(&self, model: &str) -> Result<(), EngineError> {
        self.post("load-model", model, self.load_timeout).await
    }

    /// Returns once the memory is unwired.
    async fn unload(&self, model: &str) -> Result<(), EngineError> {
        self.post("unload-model", model, Duration::from_secs(120))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::routing::{get, post};

    use super::*;

    #[tokio::test]
    async fn loads_and_unloads_through_the_sushi_api() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/v1/models", get(|| async { axum::Json(serde_json::json!({"data": [{"id": "q", "loaded": true, "state": "ready", "context_length": 131072}, {"id": "other", "loaded": false}]})) }))
            .route("/v1/load-model", post(|State(calls): State<Arc<Mutex<Vec<String>>>>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                calls.lock().unwrap().push(format!("load {}", body["model"]));
                axum::Json(serde_json::json!({"ok": true}))
            }))
            .route("/v1/unload-model", post(|State(calls): State<Arc<Mutex<Vec<String>>>>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                calls.lock().unwrap().push(format!("unload {}", body["model"]));
                if body["model"] == "bad" { (axum::http::StatusCode::BAD_REQUEST, "no such model") } else { (axum::http::StatusCode::OK, "ok") }
            }))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let engine = SushiEngine::new(format!("http://127.0.0.1:{port}/v1/"));
        let models = engine.models().await.unwrap();
        assert_eq!(
            models[0],
            ModelState {
                id: "q".into(),
                loaded: true,
                ready: true,
                context_length: Some(131072)
            }
        );
        assert_eq!(
            models[1],
            ModelState {
                id: "other".into(),
                loaded: false,
                ready: false,
                context_length: None
            }
        );
        engine.load("q").await.unwrap();
        engine.unload("q").await.unwrap();
        let error = engine.unload("bad").await.unwrap_err();
        assert!(
            matches!(error, EngineError::Refused(ref m) if m.contains("400") && m.contains("no such model")),
            "{error}"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["load \"q\"", "unload \"q\"", "unload \"bad\""]
        );
        assert!(matches!(
            SushiEngine::new("http://127.0.0.1:1/v1")
                .models()
                .await
                .unwrap_err(),
            EngineError::Unreachable(_)
        ));
    }
}
