//! The proxy against a fake engine: usage from JSON and SSE answers, engine
//! errors (also one sent inside a stream), holding requests while the gate
//! is closed, and replies cut at the output limit.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::response::IntoResponse;
use futures::StreamExt;
use tenaz_core::lease::Gate;
use tenaz_core::session::{ENGINE_UNREACHABLE, is_engine_unavailable};
use tenaz_proxy::{Proxy, ProxyEvent, ProxyHandle, ProxyUsage};

/// The id a fresh proxy gives its first request.
const FIRST_REQUEST: u64 = 1;

/// The fake engine's answer to a prompt too large for it.
const TOO_BIG: &str =
    "Prompt too large. Maximum context at KV8 and prefill chunk 512: 61488 tokens.";

/// The SSE error chunk read from Sushi 1.4.0's binary, without `type`,
/// `param` and `code` (the whole chunk is in "out of memory mid stream").
fn sse_error_chunk(message: &str) -> String {
    let chunk = serde_json::json!({
        "error": {"message": message},
        "choices": [{"index": 0, "delta": {}, "finish_reason": "error"}]
    });
    format!("data: {chunk}\n\n")
}

/// The fake engine's answers with an error status, by prompt.
fn error_reply(content: &str) -> Option<axum::response::Response> {
    use axum::http::StatusCode;
    let reply = match content {
        "too big" => (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({"error": {"message": TOO_BIG}}))).into_response(),
        "busy" => (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response(),
        // mlx-serve 26.10.3 with no memory left (2026-11-02).
        "no memory" => (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"error": {"message": "not enough memory: available=0 MB", "type": "out_of_memory"}}))).into_response(),
        // No engine has been seen to send this: an error status with an SSE
        // body that carries an error chunk.
        "busy, as a stream" => (StatusCode::SERVICE_UNAVAILABLE, [("content-type", "text/event-stream")], sse_error_chunk("busy in a chunk")).into_response(),
        _ => return None,
    };
    Some(reply)
}

/// The body of a stream the fake engine keeps open, for the prompts that
/// ask for one: an out-of-memory error chunk, framed with `\n` or `\r\n`.
fn kept_open_error(content: &str) -> Option<String> {
    let chunk = sse_error_chunk("error.GenerationOutOfMemory");
    match content {
        "out of memory, stream kept open" => Some(chunk),
        "out of memory, crlf stream kept open" => Some(chunk.replace('\n', "\r\n")),
        _ => None,
    }
}

async fn fake_engine() -> SocketAddr {
    let app = axum::Router::new()
        .route("/v1/models", axum::routing::get(|| async { axum::Json(serde_json::json!({"data": [{"id": "m"}]})) }))
        .route(
            "/v1/chat/completions",
            axum::routing::post(|axum::Json(body): axum::Json<serde_json::Value>| async move {
                let content = body["messages"][0]["content"].as_str().unwrap_or("").to_owned();
                if let Some(reply) = error_reply(&content) {
                    return reply;
                }
                if let Some(error) = kept_open_error(&content) {
                    let body = futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(error))])
                        .chain(futures::stream::pending());
                    return ([("content-type", "text/event-stream")], axum::body::Body::from_stream(body)).into_response();
                }
                if content == "out of memory mid stream" {
                    // Sushi 1.4.0 when Metal cannot get the memory part way through.
                    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"error\":{\"message\":\"error.GenerationOutOfMemory\",\"type\":\"server_error\",\"param\":null,\"code\":null},\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"error\"}]}\n\ndata: {\"error\":{\"message\":\"a later error\"}}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":67373,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
                    return ([("content-type", "text/event-stream")], sse).into_response();
                }
                if content == "cut in a tool call" {
                    let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":16}}\n\ndata: [DONE]\n\n";
                    return ([("content-type", "text/event-stream")], sse).into_response();
                }
                if body["stream"] == true {
                    assert_eq!(body["stream_options"]["include_usage"], true, "the proxy asks for usage in streams");
                    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":40}},\"timings\":{\"prompt_n\":50,\"cached_n\":40,\"prompt_ms\":12.5,\"predicted_ms\":30.0,\"draft_n\":8,\"draft_n_accepted\":6}}\n\ndata: [DONE]\n\n";
                    return ([("content-type", "text/event-stream")], sse).into_response();
                }
                axum::Json(serde_json::json!({"choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 3}, "timings": {"prompt_n": 10, "prompt_ms": 4.0, "predicted_ms": 9.5}})).into_response()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn start(
    upstream: SocketAddr,
    gate: Gate,
) -> (tenaz_proxy::ProxyHandle, Arc<Mutex<Vec<ProxyEvent>>>) {
    let events: Arc<Mutex<Vec<ProxyEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let proxy = Arc::new(Proxy::new(
        format!("http://{upstream}"),
        gate,
        Arc::new(move |e| sink.lock().unwrap().push(e)),
    ));
    let handle = proxy.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    (handle, events)
}

async fn settle(events: &Arc<Mutex<Vec<ProxyEvent>>>, count: usize) -> Vec<ProxyEvent> {
    for _ in 0..200 {
        if events.lock().unwrap().len() >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    events.lock().unwrap().clone()
}

#[tokio::test]
async fn forwards_requests_and_records_usage_and_errors() {
    let upstream = fake_engine().await;
    let (handle, events) = start(upstream, Gate::new()).await;
    let client = reqwest::Client::new();
    let base = handle.base_url();

    let models: serde_json::Value = client
        .get(format!("{base}/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "m");

    let plain: serde_json::Value = client
        .post(format!("{base}/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(plain["choices"][0]["message"]["content"], "hello");

    let streamed = client
        .post(format!("{base}/chat/completions"))
        .json(&serde_json::json!({"stream": true, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert!(
        streamed.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = streamed.text().await.unwrap();
    assert!(text.contains("data: [DONE]"));

    let error = client
        .post(format!("{base}/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "too big"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(error.status(), 400);

    let events = settle(&events, 5).await;
    let timings: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ProxyEvent::Completed { timings, .. } => Some(*timings),
            _ => None,
        })
        .collect();
    assert_eq!(timings[0], None, "/v1/models carries no timings");
    assert_eq!(
        timings[1],
        Some(tenaz_core::session::ServerTimings {
            prompt_ms: Some(4.0),
            decode_ms: Some(9.5),
            ..Default::default()
        }),
        "timings from a JSON response"
    );
    assert_eq!(
        timings[2],
        Some(tenaz_core::session::ServerTimings {
            prompt_ms: Some(12.5),
            decode_ms: Some(30.0),
            cached_tokens: Some(40),
            drafted: Some(8),
            accepted: Some(6)
        }),
        "timings, with drafts, from the last SSE chunk"
    );
    let completed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ProxyEvent::Completed {
                path,
                status,
                stream,
                usage,
                finish_reason,
                ..
            } => Some((
                path.clone(),
                *status,
                *stream,
                *usage,
                finish_reason.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed[0],
        ("/v1/models".to_owned(), 200, false, None, None)
    );
    assert_eq!(
        completed[1],
        (
            "/v1/chat/completions".to_owned(),
            200,
            false,
            Some(ProxyUsage {
                prompt_tokens: 10,
                cached_tokens: None,
                completion_tokens: 3,
                reasoning_tokens: 0
            }),
            Some("stop".to_owned())
        )
    );
    assert_eq!(
        completed[2],
        (
            "/v1/chat/completions".to_owned(),
            200,
            true,
            Some(ProxyUsage {
                prompt_tokens: 50,
                cached_tokens: Some(40),
                completion_tokens: 2,
                reasoning_tokens: 0
            }),
            Some("length".to_owned())
        )
    );
    assert_eq!(completed[3].1, 400);
    assert!(
        events.iter().any(|e| matches!(
            e,
            ProxyEvent::EngineError { message, .. } if *message == format!("400: {TOO_BIG}")
        )),
        "{events:?}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn holds_requests_while_the_gate_is_closed() {
    let upstream = fake_engine().await;
    let gate = Gate::new();
    gate.close();
    let (handle, events) = start(upstream, gate.clone()).await;
    let base = handle.base_url();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("{base}/models"))
            .send()
            .await
            .unwrap()
            .status()
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !request.is_finished(),
        "the request went through a closed gate"
    );
    gate.open();
    assert_eq!(request.await.unwrap(), 200);
    // The proxy reports a hold only for a request that met the closed gate.
    // How long it held depends on when the request reached the proxy, which
    // on a slow runner can be well into the 100 ms above, so it is not
    // asserted.
    let events = settle(&events, 2).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProxyEvent::Held { request, .. } if *request == FIRST_REQUEST)),
        "{events:?}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn an_unreachable_engine_is_a_502_with_an_event() {
    let (handle, events) = start("127.0.0.1:1".parse().unwrap(), Gate::new()).await;
    let response = reqwest::Client::new()
        .get(format!("{}/models", handle.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let events = settle(&events, 1).await;
    assert!(
        matches!(
            &events[0],
            ProxyEvent::EngineError { message, .. }
                if message.starts_with(&format!("{ENGINE_UNREACHABLE}: "))
        ),
        "{events:?}"
    );
    handle.stop().await;
}

/// An engine's error after an error status starts with that status. The
/// preflight reads a busy engine, and mlx-serve's answer when it has no
/// memory left, as an engine that could not take the request; a prompt too
/// large for it is not.
#[tokio::test]
async fn an_error_status_leads_the_engine_error() {
    let (handle, events) = start(fake_engine().await, Gate::new()).await;
    let client = reqwest::Client::new();
    let capacity = format!("400: {TOO_BIG}");
    for (content, expected, unavailable) in [
        ("busy", "503: busy", true),
        ("no memory", "503: not enough memory: available=0 MB", true),
        ("too big", capacity.as_str(), false),
    ] {
        events.lock().unwrap().clear();
        client
            .post(format!("{}/chat/completions", handle.base_url()))
            .json(&serde_json::json!({"messages": [{"role": "user", "content": content}]}))
            .send()
            .await
            .unwrap();
        let seen = settle(&events, 2).await;
        let message = seen
            .iter()
            .find_map(|e| match e {
                ProxyEvent::EngineError { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{content}: no engine error in {seen:?}"));
        assert_eq!(message, expected, "{content}");
        assert_eq!(is_engine_unavailable(message), unavailable, "{message}");
    }
    handle.stop().await;
}

/// An engine that runs out of memory part way through a stream (status
/// 200) is reported as an engine error, and the request's partial usage is
/// not reported as a reply: the runner sees one failed request.
#[tokio::test]
async fn an_error_inside_a_stream_is_an_engine_error() {
    let upstream = fake_engine().await;
    let (handle, events) = start(upstream, Gate::new()).await;
    let response = reqwest::Client::new()
        .post(format!("{}/chat/completions", handle.base_url()))
        .json(&serde_json::json!({
            "stream": true,
            "messages": [{"role": "user", "content": "out of memory mid stream"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();
    let events = settle(&events, 2).await;
    let errors = events
        .iter()
        .filter(|e| matches!(e, ProxyEvent::EngineError { .. }))
        .count();
    assert_eq!(errors, 1, "only the first error is reported: {events:?}");
    assert_eq!(
        events[0],
        ProxyEvent::EngineError {
            request: FIRST_REQUEST,
            message: "error.GenerationOutOfMemory".to_owned()
        }
    );
    assert!(
        matches!(&events[1], ProxyEvent::Completed { usage: None, .. }),
        "{events:?}"
    );
    handle.stop().await;
}

/// An error status with an SSE body that carries an error chunk is one
/// engine error: the status, then the chunk's message. The preflight reads
/// it as an engine that could not take the request, and the chunk is not
/// reported as a second error.
#[tokio::test]
async fn an_error_status_with_an_error_chunk_is_one_engine_error() {
    let (handle, events) = start(fake_engine().await, Gate::new()).await;
    let response = reqwest::Client::new()
        .post(format!("{}/chat/completions", handle.base_url()))
        .json(&serde_json::json!({
            "stream": true,
            "messages": [{"role": "user", "content": "busy, as a stream"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    response.text().await.unwrap();
    let events = settle(&events, 2).await;
    let [
        ProxyEvent::EngineError {
            request: FIRST_REQUEST,
            message,
        },
        ProxyEvent::Completed {
            status: 503,
            stream: true,
            usage: None,
            ..
        },
    ] = events.as_slice()
    else {
        panic!("one engine error, then the completion: {events:?}");
    };
    assert_eq!(message, "503: busy in a chunk");
    assert!(is_engine_unavailable(message), "{message}");
    handle.stop().await;
}

/// The error is reported when its chunk arrives, while the stream is still
/// open: fx may exit on it before the stream ends.
#[tokio::test]
async fn an_error_inside_a_stream_is_reported_before_the_stream_ends() {
    error_is_reported_while_the_stream_is_open("out of memory, stream kept open").await;
}

/// The same when the engine ends its events with `\r\n\r\n`.
#[tokio::test]
async fn an_error_inside_a_crlf_stream_is_reported_before_the_stream_ends() {
    error_is_reported_while_the_stream_is_open("out of memory, crlf stream kept open").await;
}

/// Sends `content` to the fake engine, which answers with an error chunk
/// and keeps the stream open.
async fn error_is_reported_while_the_stream_is_open(content: &str) {
    let upstream = fake_engine().await;
    let (handle, events) = start(upstream, Gate::new()).await;
    let mut response = reqwest::Client::new()
        .post(format!("{}/chat/completions", handle.base_url()))
        .json(&serde_json::json!({
            "stream": true,
            "messages": [{"role": "user", "content": content}]
        }))
        .send()
        .await
        .unwrap();
    assert!(response.chunk().await.unwrap().is_some());
    let open = settle(&events, 1).await;
    assert_eq!(
        open,
        [ProxyEvent::EngineError {
            request: FIRST_REQUEST,
            message: "error.GenerationOutOfMemory".to_owned()
        }]
    );
    drop(response);
    let closed = settle(&events, 2).await;
    assert_eq!(
        closed.len(),
        2,
        "no completion once the client left: {closed:?}"
    );
    assert!(
        matches!(&closed[1], ProxyEvent::Completed { usage: None, .. }),
        "{closed:?}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn a_reply_at_the_requested_output_limit_is_reported_as_cut() {
    let upstream = fake_engine().await;
    let (handle, events) = start(upstream, Gate::new()).await;
    reqwest::Client::new()
        .post(format!("{}/chat/completions", handle.base_url()))
        .json(&serde_json::json!({
            "stream": true,
            "max_tokens": 16,
            "temperature": 0.8,
            "messages": [{"role": "user", "content": "cut in a tool call"}]
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let events = settle(&events, 1).await;
    assert!(
        matches!(
            &events[0],
            ProxyEvent::Completed { finish_reason: Some(reason), .. } if reason == "length"
        ),
        "the engine said tool_calls at 16 of 16 tokens: {events:?}"
    );
    assert!(
        matches!(
            &events[0],
            ProxyEvent::Completed { sent: Some(sent), .. }
                if sent.temperature == Some(0.8) && sent.max_tokens == Some(16)
        ),
        "the request's settings are reported with its completion: {events:?}"
    );
}

/// The opening of Qwen Code 0.26.6's summarizing prompt
/// (`tenaz_acp::qwen::COMPACTION_PROMPT`).
const COMPACTION_OPENING: &str = "You are the component that summarizes a conversation when its context window is about to overflow.";

/// A chat request whose last message is from the user and says `last`.
fn chat(last: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"messages": [
        {"role": "system", "content": "You are a coding agent."},
        {"role": "user", "content": "hi"},
        {"role": "user", "content": last}
    ]})
}

/// A proxy in front of a fake engine that refuses requests opening with
/// `opening`, and the events it reports.
async fn refusing_proxy(opening: &str) -> (ProxyHandle, Arc<Mutex<Vec<ProxyEvent>>>) {
    let upstream = fake_engine().await;
    let events: Arc<Mutex<Vec<ProxyEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let mut proxy = Proxy::new(
        format!("http://{upstream}"),
        Gate::new(),
        Arc::new(move |e| sink.lock().unwrap().push(e)),
    );
    proxy.compaction_prompt_opening = Some(opening.to_owned());
    let handle = Arc::new(proxy)
        .start("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    (handle, events)
}

/// Qwen Code's request to summarize its conversation is answered with an
/// error and never reaches the engine; a turn that only quotes the prompt
/// passes.
#[tokio::test]
async fn refuses_the_harness_compaction_request() {
    let (handle, events) = refusing_proxy(COMPACTION_OPENING).await;
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", handle.base_url());
    let summarize = chat(&serde_json::json!([
        {"type": "text", "text": format!("\n{COMPACTION_OPENING} The summary you produce …")}
    ]));
    let refused = client.post(&url).json(&summarize).send().await.unwrap();
    assert_eq!(refused.status(), 400);
    let body: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["type"], "proxy_error", "{body}");
    let quoted = chat(&serde_json::json!(format!(
        "Read crates/acp/src/qwen.rs; it holds \"{COMPACTION_OPENING}\""
    )));
    let passed = client.post(&url).json(&quoted).send().await.unwrap();
    assert_eq!(
        passed.status(),
        200,
        "a turn that quotes the prompt is not a compaction"
    );
    let events = settle(&events, 2).await;
    assert_eq!(
        events[0],
        ProxyEvent::CompactionRefused {
            request: FIRST_REQUEST
        }
    );
    assert!(
        matches!(&events[1], ProxyEvent::Completed { status: 200, .. }),
        "only the agent's turn reached the engine: {events:?}"
    );
    handle.stop().await;
}

/// The opening of dsh 0.2.2-rc.1's summarizing prompt
/// (`tenaz_acp::dsh::COMPACTION_PROMPT`).
const DSH_COMPACTION_OPENING: &str =
    "You are now acting as a compaction engine for this AI coding assistant.";

/// dsh sends its summarizing prompt as one text part after a tool result,
/// which pi-ai may mark for caching; the proxy refuses it as Qwen Code's.
#[tokio::test]
async fn refuses_dsh_s_compaction_request() {
    let (handle, events) = refusing_proxy(DSH_COMPACTION_OPENING).await;
    let summarize = serde_json::json!({"messages": [
        {"role": "system", "content": "You are a coding agent."},
        {"role": "user", "content": "build the map"},
        {"role": "assistant", "content": null, "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{}"}}
        ]},
        {"role": "tool", "tool_call_id": "c1", "content": "map.js"},
        {"role": "user", "content": [{
            "type": "text",
            "text": format!("{DSH_COMPACTION_OPENING} Condense the conversation ABOVE …"),
            "cache_control": {"type": "ephemeral"}
        }]}
    ]});
    let refused = reqwest::Client::new()
        .post(format!("{}/chat/completions", handle.base_url()))
        .json(&summarize)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    assert_eq!(
        settle(&events, 1).await,
        [ProxyEvent::CompactionRefused {
            request: FIRST_REQUEST
        }]
    );
    handle.stop().await;
}
