//! The engine proxy: an OpenAI-compatible reverse proxy between the harness
//! and the LLM engine. It holds requests while the lease has the LLM
//! unloaded, records every request's token usage (the harness reports usage
//! only once per turn), and classifies engine errors in one place.

mod events;
mod tap;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;
use smr_core::lease::Gate;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use events::{
    EngineErrorKind, Observer, ProxyEvent, ProxyUsage, classify_error, usage_from_json,
};

use tap::{Tap, TappedStream};

/// The proxy's shared state.
pub struct Proxy {
    upstream: String,
    gate: Gate,
    client: reqwest::Client,
    observer: Observer,
    sequence: AtomicU64,
    /// Add `stream_options.include_usage` to streaming chat requests, so the
    /// final chunk carries the usage. Engines ignore it when unsupported.
    pub include_usage: bool,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("upstream", &self.upstream)
            .finish_non_exhaustive()
    }
}

/// The running proxy.
#[derive(Debug)]
pub struct ProxyHandle {
    pub local_addr: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl ProxyHandle {
    /// `http://127.0.0.1:<port>/v1`, what the harness is pointed at.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.local_addr)
    }

    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

/// Headers that belong to one hop and are never forwarded.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

impl Proxy {
    /// `upstream` is the engine's origin (`http://127.0.0.1:12345`); paths are
    /// forwarded as received, so `/v1/...` stays `/v1/...`.
    pub fn new(upstream: impl Into<String>, gate: Gate, observer: Observer) -> Self {
        Self {
            upstream: upstream.into().trim_end_matches('/').to_owned(),
            gate,
            client: reqwest::Client::builder().build().unwrap_or_default(),
            observer,
            sequence: AtomicU64::new(0),
            include_usage: true,
        }
    }

    /// Binds `addr` (port 0 for an ephemeral port) and serves until stopped.
    pub async fn start(self: Arc<Self>, addr: SocketAddr) -> std::io::Result<ProxyHandle> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        let shutdown = CancellationToken::new();
        let signal = shutdown.clone();
        let app = axum::Router::new().fallback(handle).with_state(self);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { signal.cancelled().await })
                .await
        });
        Ok(ProxyHandle {
            local_addr,
            shutdown,
            task,
        })
    }

    fn observe(&self, event: ProxyEvent) {
        (self.observer)(event);
    }
}

fn forwardable(headers: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) || lower == "host" || lower == "content-length" {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_ref()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(n, v);
        }
    }
    out
}

/// Is this a request the engine answers with a completion (and usage)?
fn is_completion(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::POST
        && (path.ends_with("/chat/completions")
            || path.ends_with("/completions")
            || path.ends_with("/responses"))
}

/// Adds `stream_options.include_usage` to a streaming request that lacks it.
fn with_usage_option(body: &[u8]) -> Option<Vec<u8>> {
    let mut json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = json.as_object_mut()?;
    if object.get("stream").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    let options = object
        .entry("stream_options")
        .or_insert_with(|| serde_json::json!({}));
    let options = options.as_object_mut()?;
    if options.contains_key("include_usage") {
        return None;
    }
    options.insert("include_usage".to_owned(), serde_json::Value::Bool(true));
    serde_json::to_vec(&json).ok()
}

async fn handle(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
    let id = proxy.sequence.fetch_add(1, Ordering::SeqCst) + 1;
    let started = Instant::now();
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_owned(), ToString::to_string);
    let completion = is_completion(&method, request.uri().path());
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), 256 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("request body: {error}")).into_response();
        }
    };
    let rewritten = if completion && proxy.include_usage {
        with_usage_option(&body)
    } else {
        None
    };
    let (body, stream) = if let Some(rewritten) = rewritten {
        (bytes::Bytes::from(rewritten), true)
    } else {
        let stream = completion
            && serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|j| j.get("stream").and_then(serde_json::Value::as_bool))
                .unwrap_or(false);
        (body, stream)
    };
    // Hold while the lease has the LLM away.
    if !proxy.gate.is_open() {
        let waited = Instant::now();
        proxy.gate.wait_open().await;
        proxy.observe(ProxyEvent::Held {
            request: id,
            ms: elapsed_ms(waited),
        });
    }
    let upstream = match proxy
        .client
        .request(method.clone(), format!("{}{path}", proxy.upstream))
        .headers(forwardable(&headers))
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let message = format!("engine unreachable: {error}");
            proxy.observe(ProxyEvent::EngineError {
                request: id,
                status: None,
                message: message.clone(),
                kind: EngineErrorKind::Unreachable,
            });
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(
                    serde_json::json!({ "error": { "message": message, "type": "proxy_error" } }),
                ),
            )
                .into_response();
        }
    };
    let status = upstream.status();
    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) || lower == "content-length" {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_ref()),
            axum::http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            response_headers.insert(n, v);
        }
    }
    let event_stream = upstream
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("text/event-stream"));
    let tap = Tap::new(
        proxy.clone(),
        id,
        path,
        status.as_u16(),
        stream || event_stream,
        completion,
        started,
    );
    let tapped = TappedStream::new(
        upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other)),
        tap,
    );
    let mut response = Response::new(Body::from_stream(tapped));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_requests_get_the_usage_option() {
        let body = br#"{"model":"m","stream":true,"messages":[]}"#;
        let rewritten = with_usage_option(body).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert_eq!(json["messages"], serde_json::json!([]));
        assert!(with_usage_option(br#"{"stream":false}"#).is_none());
        assert!(
            with_usage_option(br#"{"stream":true,"stream_options":{"include_usage":false}}"#)
                .is_none()
        );
        assert!(with_usage_option(b"not json").is_none());
    }

    #[test]
    fn completion_paths() {
        let post = axum::http::Method::POST;
        assert!(is_completion(&post, "/v1/chat/completions"));
        assert!(is_completion(&post, "/v1/completions"));
        assert!(!is_completion(&axum::http::Method::GET, "/v1/models"));
        assert!(!is_completion(&post, "/v1/load-model"));
    }
}
