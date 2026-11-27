//! A request the engine refuses for its output limit, and names one it
//! has room for, is sent once more with that limit (Sushi in the Last
//! Delivery run of 2026-11-18).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::response::IntoResponse;
use tenaz_core::lease::Gate;
use tenaz_proxy::{Proxy, ProxyEvent, ProxyHandle};
use tokio::sync::mpsc;

/// Sushi's refusal in session 014 of that run, with room for `fits` of
/// the 16,384 output tokens asked.
fn sushi_refusal(fits: u64) -> String {
    format!(
        "Context 103038 does not fit in GPU memory: the request needs 3.6 GB and 3.4 GB is available. max_tokens {fits} fits (asked 16384); maximum context: 99840 tokens.\nTo serve it:\n- Lower max_tokens to {fits} or less.\n- Shorten the prompt.\n- Quit other apps or unload another model on the Models page, then retry.\n- Use a smaller pack of this model."
    )
}

/// What the fake engine was sent.
type Sent = Arc<Mutex<Vec<serde_json::Value>>>;

/// An engine that refuses its first `refusals` requests with
/// [`sushi_refusal`] of `fits`, then answers.
async fn refusing_engine(fits: u64, refusals: usize) -> (SocketAddr, Sent) {
    let sent: Sent = Arc::default();
    let seen = sent.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            move |axum::Json(body): axum::Json<serde_json::Value>| async move {
                let stream = body["stream"] == true;
                let mut seen = seen.lock().unwrap();
                seen.push(body);
                if seen.len() <= refusals {
                    let error = serde_json::json!({"error": {"message": sushi_refusal(fits)}});
                    return (axum::http::StatusCode::BAD_REQUEST, axum::Json(error))
                        .into_response();
                }
                if stream {
                    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":86654,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n";
                    return ([("content-type", "text/event-stream")], sse).into_response();
                }
                axum::Json(serde_json::json!({
                    "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 86654, "completion_tokens": 3}
                }))
                .into_response()
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, sent)
}

async fn start(upstream: SocketAddr) -> (ProxyHandle, mpsc::UnboundedReceiver<ProxyEvent>) {
    let (events, received) = mpsc::unbounded_channel();
    let proxy = Arc::new(Proxy::new(
        format!("http://{upstream}"),
        Gate::new(),
        Arc::new(move |event| {
            let _ = events.send(event);
        }),
    ));
    let handle = proxy.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    (handle, received)
}

/// Sends `request` through the proxy: the status and body the harness gets.
async fn send(handle: &ProxyHandle, request: serde_json::Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{}/chat/completions", handle.base_url()))
        .json(&request)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

/// The proxy's events up to the request's `Completed`, which comes last.
/// A proxy that never reports it fails the test instead of hanging it.
async fn until_completed(events: &mut mpsc::UnboundedReceiver<ProxyEvent>) -> Vec<ProxyEvent> {
    let mut seen = Vec::new();
    let collect = async {
        while let Some(event) = events.recv().await {
            let last = matches!(event, ProxyEvent::Completed { .. });
            seen.push(event);
            if last {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .expect("the request's Completed event");
    seen
}

/// The first request the engine got, with `max_tokens` set to `limit`:
/// what the second must be.
fn first_with_limit(sent: &Sent, limit: u64) -> serde_json::Value {
    let mut first = sent.lock().unwrap()[0].clone();
    first["max_tokens"] = limit.into();
    first
}

/// The second request the engine got. The lock is let go before the
/// caller locks `sent` again.
fn second(sent: &Sent) -> serde_json::Value {
    sent.lock().unwrap()[1].clone()
}

fn chat(limit: Option<u64>) -> serde_json::Value {
    let mut request = serde_json::json!({
        "messages": [{"role": "user", "content": "go on"}]
    });
    if let Some(limit) = limit {
        request["max_tokens"] = limit.into();
    }
    request
}

fn max_tokens(sent: &Sent) -> Vec<Option<u64>> {
    sent.lock()
        .unwrap()
        .iter()
        .map(|request| request["max_tokens"].as_u64())
        .collect()
}

/// The harness gets the reply to the request sent again, and never sees
/// the refusal: the session goes on.
#[tokio::test]
async fn a_request_refused_for_its_output_limit_is_sent_again_with_the_one_that_fits() {
    let (upstream, sent) = refusing_engine(13186, 1).await;
    let (handle, mut events) = start(upstream).await;
    let (status, body) = send(&handle, chat(Some(16384))).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("hello"), "{body}");
    assert_eq!(max_tokens(&sent), [Some(16384), Some(13186)]);
    assert_eq!(second(&sent), first_with_limit(&sent, 13186));
    let events = until_completed(&mut events).await;
    assert!(
        matches!(
            events.as_slice(),
            [
                ProxyEvent::OutputLimitLowered {
                    request: 1,
                    asked: 16384,
                    fits: 13186
                },
                ProxyEvent::Completed { status: 200, sent: Some(sent), .. },
            ] if sent.max_tokens == Some(13186)
        ),
        "{events:?}"
    );
    handle.stop().await;
}

/// fx and Pi stream: the harness gets the stream of the request sent
/// again, and its usage is reported.
#[tokio::test]
async fn a_streamed_request_is_sent_again_and_streamed() {
    let (upstream, sent) = refusing_engine(13186, 1).await;
    let (handle, mut events) = start(upstream).await;
    let mut request = chat(Some(16384));
    request["stream"] = true.into();
    let (status, body) = send(&handle, request).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("data: [DONE]"), "{body}");
    assert_eq!(second(&sent), first_with_limit(&sent, 13186));
    let events = until_completed(&mut events).await;
    assert!(
        matches!(
            events.as_slice(),
            [
                ProxyEvent::OutputLimitLowered { fits: 13186, .. },
                ProxyEvent::Completed { status: 200, usage: Some(usage), sent: Some(sent), .. },
            ] if usage.completion_tokens == 3 && sent.max_tokens == Some(13186)
        ),
        "{events:?}"
    );
    handle.stop().await;
}

/// A request that named no limit, as Qwen Code's, is sent again with one.
#[tokio::test]
async fn a_request_without_a_limit_is_sent_again_with_one() {
    let (upstream, sent) = refusing_engine(13186, 1).await;
    let (handle, mut events) = start(upstream).await;
    let (status, _) = send(&handle, chat(None)).await;
    assert_eq!(status, 200);
    assert_eq!(max_tokens(&sent), [None, Some(13186)]);
    // With no limit in the request, the one asked is the engine's own,
    // read from its refusal.
    let events = until_completed(&mut events).await;
    assert!(
        matches!(
            events.as_slice(),
            [
                ProxyEvent::OutputLimitLowered {
                    asked: 16384,
                    fits: 13186,
                    ..
                },
                ProxyEvent::Completed { status: 200, .. },
            ]
        ),
        "{events:?}"
    );
    handle.stop().await;
}

/// A second refusal is passed on: the proxy sends a request again only
/// once.
#[tokio::test]
async fn a_request_is_sent_again_once() {
    let (upstream, sent) = refusing_engine(13186, 2).await;
    let (handle, mut events) = start(upstream).await;
    let (status, body) = send(&handle, chat(Some(16384))).await;
    assert_eq!(status, 400);
    assert!(body.contains("max_tokens 13186 fits"), "{body}");
    assert_eq!(max_tokens(&sent), [Some(16384), Some(13186)]);
    let events = until_completed(&mut events).await;
    assert!(
        matches!(
            events.as_slice(),
            [
                ProxyEvent::OutputLimitLowered { fits: 13186, .. },
                ProxyEvent::EngineError { message, .. },
                ProxyEvent::Completed { status: 400, .. },
            ] if message.starts_with("400: Context 103038")
        ),
        "{events:?}"
    );
    handle.stop().await;
}

/// Too little room to be worth a reply (session 027 of the same run): the
/// refusal is passed on as it came, and the runner decides.
#[tokio::test]
async fn a_refusal_with_too_little_room_is_passed_on() {
    let (upstream, sent) = refusing_engine(3696, 1).await;
    let (handle, mut events) = start(upstream).await;
    let (status, body) = send(&handle, chat(Some(16384))).await;
    assert_eq!(status, 400);
    let error: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["message"], sushi_refusal(3696));
    assert_eq!(max_tokens(&sent), [Some(16384)]);
    let events = until_completed(&mut events).await;
    assert!(
        matches!(
            events.as_slice(),
            [
                ProxyEvent::EngineError { message, .. },
                ProxyEvent::Completed { status: 400, .. },
            ] if *message == format!("400: {}", sushi_refusal(3696))
        ),
        "{events:?}"
    );
    handle.stop().await;
}
