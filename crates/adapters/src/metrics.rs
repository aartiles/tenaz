//! The engine's Prometheus `/metrics` endpoint, next to its `OpenAI` base URL.

use std::time::Duration;

use async_trait::async_trait;
use tenaz_core::metrics::{EngineCounters, parse_prometheus};
use tenaz_core::ports::EngineMetricsSource;

/// Reads `/metrics` on the engine's origin.
#[derive(Clone, Debug)]
pub struct PrometheusMetrics {
    url: String,
    client: reqwest::Client,
}

impl PrometheusMetrics {
    /// `base_url` is the OpenAI-compatible base (`http://127.0.0.1:18080/v1`);
    /// the metrics live at the origin's `/metrics`.
    pub fn from_base_url(base_url: &str) -> Option<Self> {
        let url = reqwest::Url::parse(base_url).ok()?;
        let origin = format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str()?,
            url.port().map(|p| format!(":{p}")).unwrap_or_default()
        );
        Some(Self {
            url: format!("{origin}/metrics"),
            client: crate::http::local_client(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

#[async_trait]
impl EngineMetricsSource for PrometheusMetrics {
    async fn counters(&self) -> Option<EngineCounters> {
        // Bounded as a whole: a stalled read must never hold the session loop.
        let fetch = async {
            let response = self
                .client
                .get(&self.url)
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            parse_prometheus(&response.text().await.ok()?)
        };
        tokio::time::timeout(Duration::from_secs(4), fetch)
            .await
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_url_is_on_the_origin() {
        assert_eq!(
            PrometheusMetrics::from_base_url("http://127.0.0.1:18080/v1")
                .unwrap()
                .url(),
            "http://127.0.0.1:18080/metrics"
        );
        assert!(PrometheusMetrics::from_base_url("not a url").is_none());
    }

    #[tokio::test]
    async fn reads_counters_from_a_server() {
        let app = axum::Router::new().route(
            "/metrics",
            axum::routing::get(|| async {
                "time_to_first_token_seconds_count 2\nprompt_tokens_total 10\n"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let source =
            PrometheusMetrics::from_base_url(&format!("http://127.0.0.1:{port}/v1")).unwrap();
        let counters = source.counters().await.unwrap();
        assert_eq!(counters["requests"], 2.0);
        assert_eq!(counters["prompt_tokens"], 10.0);
    }
}
