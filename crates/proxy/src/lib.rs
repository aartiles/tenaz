//! The engine proxy: an OpenAI-compatible reverse proxy between the harness
//! and the LLM engine. It holds requests while the lease has the LLM
//! unloaded, records every request's token usage (the harness reports usage
//! only once per turn), and reports each engine error as its message text.

mod events;
mod reasoning;
mod repair;
mod sse;
mod tap;
mod video;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;
use tenaz_core::effort::EffortDial;
use tenaz_core::lease::Gate;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use events::{Observer, ProxyEvent, ProxyUsage, usage_from_json};

pub use repair::{ObjectArgs, Repair};
use repair::{StreamRepair, repair_body};
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
    /// Fields added to a completion request that lacks them, such as the
    /// run's pinned `temperature`.
    pub request_defaults: serde_json::Map<String, serde_json::Value>,
    /// Parse back tool-call arguments the engine sent as JSON text where the
    /// tool's schema accepts only an object (see [`repair`]).
    pub repair_tool_args: bool,
    /// The session's `reasoning_effort`, added to a request that lacks one:
    /// fx does not forward a reasoning setting to a custom OpenAI-compatible
    /// provider, and Sushi's default effort is `off`. Each role may think
    /// at its own level (ADR 0042). A lowered session's effort
    /// ([`tenaz_core::effort::SessionEffort::lowered`]) replaces what the
    /// harness sent.
    pub effort: Option<EffortDial>,
    /// The output limit of a request that names none: a reply that
    /// reaches it is reported with `finish_reason: length`.
    pub output_limit: Option<OutputLimit>,
    /// The text that opens the harness's own request to summarize its
    /// conversation, for a harness whose summary requests Tenaz refuses.
    /// Such a request is answered with an error and never reaches the
    /// engine: the runner ends the session there instead, and a fresh one
    /// continues (ADRs 0022, 0030).
    pub compaction_prompt_opening: Option<String>,
    /// The reasoning of earlier assistant messages kept, in tokens, before
    /// all but the newest one's is removed in one step: a harness that sends
    /// it all back would fill the window with reasoning the model no longer
    /// needs (ADR 0023), and fewer steps keep the engine's prefix cache
    /// valid (ADR 0028). 0 removes it every turn; `None` keeps it all.
    pub past_reasoning_budget: Option<u64>,
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

/// The error message a refused compaction request is answered with.
const COMPACTION_REFUSED: &str = "Tenaz ends this session instead of compacting its context";

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
    /// Gives `request` the session's `reasoning_effort` from the dial: in
    /// place of the harness's while lowered, else when it names none; true
    /// when it changed.
    fn apply_effort(&self, request: &mut JsonObject) -> bool {
        let Some(current) = self.effort.as_ref().and_then(EffortDial::current) else {
            return false;
        };
        if !current.lowered && request.contains_key(REASONING_EFFORT) {
            return false;
        }
        set_effort(request, &current.effort)
    }

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
            request_defaults: serde_json::Map::new(),
            repair_tool_args: true,
            effort: None,
            output_limit: None,
            compaction_prompt_opening: None,
            past_reasoning_budget: tenaz_core::params::RunParams::default().past_reasoning_budget(),
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

    /// The answer to the harness's request to summarize its conversation,
    /// which is refused (see [`Proxy::compaction_prompt_opening`]); `None` for any
    /// other request.
    fn refuse_if_compaction(
        &self,
        id: u64,
        request: Option<&serde_json::Value>,
    ) -> Option<Response> {
        let opening = self.compaction_prompt_opening.as_deref()?;
        if !asks_to_compact(request?, opening) {
            return None;
        }
        self.observe(ProxyEvent::CompactionRefused { request: id });
        Some(
            (
                StatusCode::BAD_REQUEST,
                axum::Json(
                    serde_json::json!({ "error": { "message": COMPACTION_REFUSED, "type": "proxy_error" } }),
                ),
            )
                .into_response(),
        )
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

/// The output limit of the run's requests that name none (ADR 0041).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputLimit {
    /// The harness's, assumed: the request goes on without one, and the
    /// engine applies its own.
    Assumed(u64),
    /// The run's own, put into the request: a harness that sends none
    /// would leave the engine's default in its place.
    Imposed(u64),
}

impl OutputLimit {
    pub fn tokens(self) -> u64 {
        match self {
            OutputLimit::Assumed(tokens) | OutputLimit::Imposed(tokens) => tokens,
        }
    }
}

/// The API of a request the engine answers with a completion (and usage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    /// `/chat/completions` and `/completions`.
    Completions,
    /// `/responses`.
    Responses,
}

impl Api {
    /// The name this API gives a request's output limit.
    fn output_limit_field(self) -> &'static str {
        match self {
            Api::Completions => "max_tokens",
            Api::Responses => "max_output_tokens",
        }
    }
}

/// The API of a request to `path` the engine answers with a completion,
/// `None` for any other request.
fn completion_api(method: &axum::http::Method, path: &str) -> Option<Api> {
    if method != axum::http::Method::POST {
        return None;
    }
    if path.ends_with("/completions") {
        Some(Api::Completions)
    } else if path.ends_with("/responses") {
        Some(Api::Responses)
    } else {
        None
    }
}

/// Is a chat request the harness's own request to summarize its
/// conversation? Qwen Code 0.26.6 and dsh 0.2.2-rc.1 send it as the last
/// message, from the user, opening with `opening` (the system prompt is
/// the agent's own).
fn asks_to_compact(request: &serde_json::Value, opening: &str) -> bool {
    let Some(last) = request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|m| m.last())
    else {
        return false;
    };
    if last.get("role").and_then(serde_json::Value::as_str) != Some("user") {
        return false;
    }
    let text = match last.get("content") {
        Some(serde_json::Value::String(text)) => Some(text.as_str()),
        Some(serde_json::Value::Array(parts)) => parts
            .first()
            .and_then(|p| p.get("text"))
            .and_then(serde_json::Value::as_str),
        _ => None,
    };
    text.is_some_and(|t| t.trim_start().starts_with(opening))
}

impl Proxy {
    /// The output limit of a request that names none, in tokens.
    fn output_limit_tokens(&self) -> Option<u64> {
        self.output_limit.map(OutputLimit::tokens)
    }

    /// Rewrites a completion request: the earlier reasoning dropped unless
    /// kept, the session's `reasoning_effort` (see [`Proxy::apply_effort`]),
    /// the defaults for absent fields, the run's output limit on a request
    /// without one, `stream_options.include_usage` on a
    /// streaming request that lacks it, and a note in place of each video
    /// file (`video`). `None` when nothing changed.
    fn rewrite_request(&self, api: Api, body: &[u8]) -> Option<Vec<u8>> {
        let mut json: serde_json::Value = serde_json::from_slice(body).ok()?;
        let object = json.as_object_mut()?;
        let dropped = self
            .past_reasoning_budget
            .is_some_and(|budget| reasoning::drop_past_reasoning(object, budget));
        let effort = self.apply_effort(object);
        let filled = fill_defaults(object, &self.request_defaults);
        let limited = add_output_limit(object, self.output_limit, api);
        let usage = self.include_usage && ask_for_usage(object);
        let videos = video::replace_video_files(object);
        if ![dropped, effort, filled, limited, usage, videos].contains(&true) {
            return None;
        }
        serde_json::to_vec(&json).ok()
    }
}

type JsonObject = serde_json::Map<String, serde_json::Value>;

/// Gives a request of `api` that names no output limit an imposed
/// `limit`, under the name `api` gives it; true when it did.
fn add_output_limit(request: &mut JsonObject, limit: Option<OutputLimit>, api: Api) -> bool {
    let Some(OutputLimit::Imposed(limit)) = limit else {
        return false;
    };
    if events::output_limit_in(request).is_some() {
        return false;
    }
    request.insert(api.output_limit_field().to_owned(), limit.into());
    true
}

const REASONING_EFFORT: &str = "reasoning_effort";

/// Sets `reasoning_effort`; true when it was different.
fn set_effort(request: &mut JsonObject, effort: &str) -> bool {
    let current = request
        .get(REASONING_EFFORT)
        .and_then(serde_json::Value::as_str);
    if current == Some(effort) {
        return false;
    }
    request.insert(REASONING_EFFORT.to_owned(), effort.into());
    true
}

/// Adds each default the request lacks; true when any was added.
fn fill_defaults(request: &mut JsonObject, defaults: &JsonObject) -> bool {
    let mut filled = false;
    for (key, value) in defaults {
        if !request.contains_key(key) {
            request.insert(key.clone(), value.clone());
            filled = true;
        }
    }
    filled
}

/// Asks a streaming request for the usage in its last chunk; true when it
/// had not asked.
fn ask_for_usage(request: &mut JsonObject) -> bool {
    if request.get("stream").and_then(serde_json::Value::as_bool) != Some(true) {
        return false;
    }
    let options = request
        .entry("stream_options")
        .or_insert_with(|| serde_json::json!({}));
    match options.as_object_mut() {
        Some(options) if !options.contains_key("include_usage") => {
            options.insert("include_usage".to_owned(), serde_json::Value::Bool(true));
            true
        }
        _ => false,
    }
}

async fn handle(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
    let id = proxy.sequence.fetch_add(1, Ordering::SeqCst) + 1;
    let started = Instant::now();
    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_owned(), ToString::to_string);
    let completion = completion_api(&method, request.uri().path());
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), 256 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("request body: {error}")).into_response();
        }
    };
    let rewritten = completion.and_then(|api| proxy.rewrite_request(api, &body));
    let body = rewritten.map_or(body, bytes::Bytes::from);
    let request_json =
        completion.and_then(|_| serde_json::from_slice::<serde_json::Value>(&body).ok());
    if let Some(refused) = proxy.refuse_if_compaction(id, request_json.as_ref()) {
        return refused;
    }
    let stream = request_json
        .as_ref()
        .and_then(|j| j.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false);
    let watched = request_json
        .as_ref()
        .filter(|_| proxy.repair_tool_args)
        .map(ObjectArgs::from_request)
        .filter(|w| !w.is_empty());
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
            let message = tenaz_core::session::engine_unreachable_text(&error);
            proxy.observe(ProxyEvent::EngineError {
                request: id,
                message: message.clone(),
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
        completion.is_some(),
        started,
    )
    .for_request(request_json.as_ref(), proxy.output_limit_tokens());
    let tapped = TappedStream::new(
        upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other)),
        tap,
    );
    let body = match watched.filter(|_| status.is_success()) {
        None => Body::from_stream(tapped),
        Some(watched) if stream || event_stream => {
            Body::from_stream(RepairingStream::new(tapped, watched, proxy.clone(), id))
        }
        Some(watched) => {
            // Non-streaming: the whole body, repaired when it needs to be.
            let mut whole = Vec::new();
            let mut tapped = tapped;
            while let Some(chunk) = tapped.next().await {
                match chunk {
                    Ok(chunk) => whole.extend_from_slice(&chunk),
                    Err(_) => break,
                }
            }
            drop(tapped);
            match repair_body(&whole, &watched) {
                Some((fixed, repairs)) => {
                    report_repairs(&proxy, id, repairs);
                    Body::from(fixed)
                }
                None => Body::from(whole),
            }
        }
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

fn report_repairs(proxy: &Proxy, request: u64, repairs: Vec<Repair>) {
    for repair in repairs {
        proxy.observe(ProxyEvent::ToolArgsRepaired {
            request,
            tool: repair.tool,
            fields: repair.fields,
        });
    }
}

/// An SSE body passed through [`StreamRepair`].
struct RepairingStream<S> {
    inner: S,
    repair: StreamRepair,
    proxy: Arc<Proxy>,
    request: u64,
    ended: bool,
}

impl<S> RepairingStream<S> {
    fn new(inner: S, watched: ObjectArgs, proxy: Arc<Proxy>, request: u64) -> Self {
        Self {
            inner,
            repair: StreamRepair::new(watched),
            proxy,
            request,
            ended: false,
        }
    }

    fn report(&mut self) {
        let repairs = std::mem::take(&mut self.repair.repairs);
        report_repairs(&self.proxy, self.request, repairs);
    }
}

impl<S> futures::Stream for RepairingStream<S>
where
    S: futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = &mut *self;
        loop {
            if this.ended {
                return Poll::Ready(None);
            }
            match std::pin::Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    let out = this.repair.push(&chunk);
                    this.report();
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(bytes::Bytes::from(out))));
                    }
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(None) => {
                    this.ended = true;
                    let out = this.repair.finish();
                    this.report();
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(bytes::Bytes::from(out))));
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tenaz_core::effort::SessionEffort;

    /// A proxy with its defaults, for the rewrite.
    fn proxy() -> Proxy {
        Proxy::new("http://127.0.0.1:1", Gate::new(), Arc::new(|_| {}))
    }

    fn json(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn streaming_requests_get_the_usage_option() {
        let mut proxy = proxy();
        let body = br#"{"model":"m","stream":true,"messages":[]}"#;
        let rewritten = json(&proxy.rewrite_request(Api::Completions, body).unwrap());
        assert_eq!(rewritten["stream_options"]["include_usage"], true);
        assert_eq!(rewritten["messages"], serde_json::json!([]));
        assert!(
            proxy
                .rewrite_request(Api::Completions, br#"{"stream":false}"#)
                .is_none()
        );
        let asked = br#"{"stream":true,"stream_options":{"include_usage":false}}"#;
        assert!(proxy.rewrite_request(Api::Completions, asked).is_none());
        assert!(
            proxy
                .rewrite_request(Api::Completions, b"not json")
                .is_none()
        );
        proxy.include_usage = false;
        assert!(proxy.rewrite_request(Api::Completions, body).is_none());
    }

    /// With the run's own limit, a request that names none is given it,
    /// under the name its API uses; a request's own limit, under any name,
    /// is left as it is.
    #[test]
    fn a_request_without_an_output_limit_is_given_the_run_s() {
        let mut proxy = proxy();
        let bare = br#"{"model":"m"}"#;
        assert!(
            proxy.rewrite_request(Api::Completions, bare).is_none(),
            "nothing without the run's own"
        );
        proxy.output_limit = Some(OutputLimit::Assumed(65_536));
        assert!(
            proxy.rewrite_request(Api::Completions, bare).is_none(),
            "an assumed limit is not put in"
        );
        proxy.output_limit = Some(OutputLimit::Imposed(65_536));
        let chat = json(&proxy.rewrite_request(Api::Completions, bare).unwrap());
        assert_eq!(chat["max_tokens"], 65_536);
        let responses = json(&proxy.rewrite_request(Api::Responses, bare).unwrap());
        assert_eq!(responses["max_output_tokens"], 65_536);
        assert!(responses.get("max_tokens").is_none(), "{responses}");
        for own in [
            br#"{"max_tokens":100}"#.as_slice(),
            br#"{"max_completion_tokens":100}"#,
            br#"{"max_output_tokens":100}"#,
        ] {
            assert!(proxy.rewrite_request(Api::Completions, own).is_none());
        }
    }

    /// A request whose only change is a video file is rewritten.
    #[test]
    fn a_request_with_only_a_video_file_is_rewritten() {
        let body = serde_json::to_vec(
            &serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "video_url", "video_url": {"url": "data:video/mp4;base64,AAAA"}}
            ]}]}),
        )
        .unwrap();
        let rewritten = json(&proxy().rewrite_request(Api::Completions, &body).unwrap());
        assert_eq!(
            rewritten["messages"][0]["content"][0]["text"],
            video::VIDEO_NOTE
        );
    }

    /// An agent's request between two tool calls: it ends with a tool
    /// result, and holds items that are not messages.
    fn agent_request() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "reasoning_content": "old", "reasoning": "old",
             "tool_calls": [{"id": "1"}]},
            1,
            {"content": "no role", "reasoning_content": "kept"},
            {"role": "tool", "tool_call_id": "1", "content": "a.txt"},
            {"role": "assistant", "content": "", "reasoning_content": "new",
             "tool_calls": [{"id": "2"}]},
            {"role": "tool", "tool_call_id": "2", "content": "b.txt"},
        ]}))
        .unwrap()
    }

    #[test]
    fn a_zero_budget_keeps_only_the_last_assistant_messages_reasoning() {
        let mut proxy = proxy();
        proxy.past_reasoning_budget = Some(0);
        let rewritten = json(
            &proxy
                .rewrite_request(Api::Completions, &agent_request())
                .unwrap(),
        );
        let messages = &rewritten["messages"];
        assert_eq!(
            messages[1],
            serde_json::json!({"role": "assistant", "content": "", "tool_calls": [{"id": "1"}]})
        );
        assert_eq!(messages[2], 1);
        assert_eq!(messages[3]["reasoning_content"], "kept");
        assert_eq!(messages[5]["reasoning_content"], "new");
    }

    #[test]
    fn earlier_reasoning_within_the_budget_is_kept() {
        let body = agent_request();
        let sent = json(
            &proxy()
                .rewrite_request(Api::Completions, &body)
                .unwrap_or(body),
        );
        assert_eq!(sent["messages"][1]["reasoning_content"], "old");
        assert_eq!(sent["messages"][1]["reasoning"], "old");
    }

    #[test]
    fn a_single_assistant_message_keeps_its_reasoning() {
        let one = br#"{"messages":[{"role":"assistant","reasoning_content":"r"}]}"#;
        assert!(proxy().rewrite_request(Api::Completions, one).is_none());
    }

    /// Two assistant messages, the first with reasoning just past the
    /// default budget.
    fn request_past_the_default_budget() -> Vec<u8> {
        let tokens = usize::try_from(tenaz_core::params::PAST_REASONING_TOKENS).unwrap() + 1;
        serde_json::to_vec(&serde_json::json!({"messages": [
            {"role": "assistant", "reasoning_content": "r".repeat(4 * tokens)},
            {"role": "assistant", "reasoning_content": "new"},
        ]}))
        .unwrap()
    }

    #[test]
    fn earlier_reasoning_past_the_default_budget_is_removed() {
        let rewritten = json(
            &proxy()
                .rewrite_request(Api::Completions, &request_past_the_default_budget())
                .unwrap(),
        );
        assert!(rewritten["messages"][0].get("reasoning_content").is_none());
        assert_eq!(rewritten["messages"][1]["reasoning_content"], "new");
    }

    #[test]
    fn no_budget_sends_all_earlier_reasoning() {
        let mut proxy = proxy();
        proxy.past_reasoning_budget = None;
        assert!(
            proxy
                .rewrite_request(Api::Completions, &request_past_the_default_budget())
                .is_none()
        );
    }

    #[test]
    fn request_defaults_fill_only_absent_fields() {
        let mut proxy = proxy();
        proxy
            .request_defaults
            .insert("temperature".to_owned(), 0.0.into());
        let body = br#"{"model":"m","stream":false,"messages":[]}"#;
        let rewritten = json(&proxy.rewrite_request(Api::Completions, body).unwrap());
        assert_eq!(rewritten["temperature"], 0.0);
        assert!(rewritten.get("stream_options").is_none());
        let explicit = br#"{"model":"m","temperature":1.0,"stream":true,"stream_options":{"include_usage":true}}"#;
        assert!(proxy.rewrite_request(Api::Completions, explicit).is_none());
    }

    #[test]
    fn a_lowered_effort_replaces_the_harness_s() {
        let mut proxy = proxy();
        let absent = br#"{"model":"m","stream":false}"#;
        assert!(
            proxy.rewrite_request(Api::Completions, absent).is_none(),
            "no dial, no effort"
        );
        let dial = EffortDial::new();
        proxy.effort = Some(dial.clone());
        // A session at `high`, stepped down to `medium`: that replaces
        // what the harness sent, and fills in a request without one.
        dial.set(SessionEffort::for_session(Some("high"), true));
        let sent = br#"{"model":"m","reasoning_effort":"low","stream":false}"#;
        assert_eq!(
            json(&proxy.rewrite_request(Api::Completions, sent).unwrap())["reasoning_effort"],
            "medium"
        );
        assert_eq!(
            json(&proxy.rewrite_request(Api::Completions, absent).unwrap())["reasoning_effort"],
            "medium"
        );
    }

    #[test]
    fn a_request_already_at_the_lowered_effort_is_left_alone() {
        let mut proxy = proxy();
        let dial = EffortDial::new();
        proxy.effort = Some(dial.clone());
        dial.set(SessionEffort::for_session(Some("high"), true));
        let lowered = br#"{"model":"m","reasoning_effort":"medium","stream":false}"#;
        assert!(proxy.rewrite_request(Api::Completions, lowered).is_none());
    }

    /// A proxy whose dial is at a session of `thinking`, not lowered.
    fn proxy_at(thinking: Option<&str>) -> Proxy {
        let mut proxy = proxy();
        let dial = EffortDial::new();
        dial.set(SessionEffort::for_session(thinking, false));
        proxy.effort = Some(dial);
        proxy
    }

    #[test]
    fn a_request_without_an_effort_gets_the_session_s() {
        let absent = br#"{"model":"m","stream":false}"#;
        let rewritten = proxy_at(Some("high")).rewrite_request(Api::Completions, absent);
        assert_eq!(json(&rewritten.unwrap())["reasoning_effort"], "high");
    }

    #[test]
    fn a_session_at_off_adds_no_effort() {
        // Streaming, so the request is rewritten (`include_usage`) and its
        // fields can be read back.
        let streaming = br#"{"model":"m","stream":true}"#;
        let rewritten = proxy_at(Some("off")).rewrite_request(Api::Completions, streaming);
        let rewritten = json(&rewritten.unwrap());
        assert!(rewritten.get("reasoning_effort").is_none(), "{rewritten}");
    }

    #[test]
    fn the_effort_the_harness_sent_stays_unless_lowered() {
        let sent = br#"{"model":"m","reasoning_effort":"low","stream":false}"#;
        assert!(
            proxy_at(Some("high"))
                .rewrite_request(Api::Completions, sent)
                .is_none()
        );
    }

    #[test]
    fn completion_paths() {
        let post = axum::http::Method::POST;
        let get = axum::http::Method::GET;
        assert_eq!(
            completion_api(&post, "/v1/chat/completions"),
            Some(Api::Completions)
        );
        assert_eq!(
            completion_api(&post, "/v1/completions"),
            Some(Api::Completions)
        );
        assert_eq!(completion_api(&post, "/v1/responses"), Some(Api::Responses));
        assert_eq!(completion_api(&get, "/v1/models"), None);
        assert_eq!(completion_api(&post, "/v1/load-model"), None);
    }
}
