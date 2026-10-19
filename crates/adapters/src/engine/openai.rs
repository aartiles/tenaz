//! What every OpenAI-compatible engine offers: `GET /models`.

use std::time::Duration;

use serde::Deserialize;
use tenaz_core::ports::{EngineError, ModelState};

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelItem>,
}

#[derive(Deserialize)]
struct ModelItem {
    id: String,
    loaded: Option<bool>,
    state: Option<String>,
    context_length: Option<u64>,
    max_model_len: Option<u64>,
}

/// Lists the models at `<base_url>/models`. Engines that do not report a
/// load state are taken as loaded and ready.
pub async fn list_models(
    client: &reqwest::Client,
    base_url: &str,
    timeout: Duration,
) -> Result<Vec<ModelState>, EngineError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| EngineError::Unreachable(e.to_string()))?;
    if !response.status().is_success() {
        return Err(EngineError::Refused(format!(
            "{url} returned {}",
            response.status()
        )));
    }
    let list: ModelList = response
        .json()
        .await
        .map_err(|e| EngineError::Refused(format!("{url}: {e}")))?;
    Ok(list
        .data
        .into_iter()
        .map(|m| {
            let loaded = m.loaded.unwrap_or(true);
            let ready = m.state.as_deref().map_or(loaded, |s| s == "ready");
            ModelState {
                id: m.id,
                loaded,
                ready,
                context_length: m.context_length.or(m.max_model_len),
            }
        })
        .collect())
}

/// Polls `/models` until `predicate` holds for the model, or `wait` passes.
pub async fn wait_for_model(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    wait: Duration,
    predicate: impl Fn(Option<&ModelState>) -> bool,
) -> Result<(), EngineError> {
    let started = std::time::Instant::now();
    loop {
        let state = list_models(client, base_url, Duration::from_secs(5))
            .await
            .ok();
        let found = state
            .as_ref()
            .and_then(|models| models.iter().find(|m| m.id == model));
        if predicate(found) {
            return Ok(());
        }
        if started.elapsed() >= wait {
            return Err(EngineError::Timeout(format!(
                "{model} did not reach the wanted state within {wait:?}"
            )));
        }
        tokio::time::sleep(Duration::from_secs(2).min(wait)).await;
    }
}
