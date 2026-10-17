//! A body stream that watches what passes: the first byte, SSE chunks with
//! usage, and error bodies.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use futures::Stream;

use crate::events::{ProxyEvent, classify_error, usage_from_json};
use crate::{Proxy, elapsed_ms};

/// Accumulates what the proxy needs from a response.
pub(crate) struct Tap {
    proxy: Arc<Proxy>,
    id: u64,
    path: String,
    status: u16,
    stream: bool,
    completion: bool,
    started: Instant,
    first_byte: Option<Instant>,
    /// Pending SSE text not yet terminated by a blank line.
    sse_buffer: String,
    /// The whole body of a non-streaming or error response (bounded).
    body: Vec<u8>,
    usage: Option<crate::ProxyUsage>,
    done: bool,
}

const BODY_LIMIT: usize = 16 * 1024 * 1024;

impl Tap {
    pub(crate) fn new(
        proxy: Arc<Proxy>,
        id: u64,
        path: String,
        status: u16,
        stream: bool,
        completion: bool,
        started: Instant,
    ) -> Self {
        Self {
            proxy,
            id,
            path,
            status,
            stream,
            completion,
            started,
            first_byte: None,
            sse_buffer: String::new(),
            body: Vec::new(),
            usage: None,
            done: false,
        }
    }

    fn chunk(&mut self, chunk: &[u8]) {
        if self.first_byte.is_none() {
            self.first_byte = Some(Instant::now());
        }
        if self.status >= 400 || !self.stream {
            if self.body.len() < BODY_LIMIT {
                self.body
                    .extend_from_slice(&chunk[..chunk.len().min(BODY_LIMIT - self.body.len())]);
            }
            return;
        }
        self.sse_buffer.push_str(&String::from_utf8_lossy(chunk));
        while let Some(at) = self.sse_buffer.find("\n\n") {
            let event = self.sse_buffer[..at].to_owned();
            self.sse_buffer.drain(..at + 2);
            self.sse_event(&event);
        }
    }

    fn sse_event(&mut self, event: &str) {
        for line in event.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
                && let Some(usage) = usage_from_json(&json)
            {
                self.usage = Some(usage);
            }
        }
    }

    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if !self.sse_buffer.is_empty() {
            let rest = std::mem::take(&mut self.sse_buffer);
            self.sse_event(&rest);
        }
        if self.status >= 400 {
            let (message, kind) = classify_error(&self.body);
            self.proxy.observe(ProxyEvent::EngineError {
                request: self.id,
                status: Some(self.status),
                message,
                kind,
            });
        } else if !self.stream
            && self.completion
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&self.body)
        {
            self.usage = usage_from_json(&json);
        }
        self.proxy.observe(ProxyEvent::Completed {
            request: self.id,
            path: std::mem::take(&mut self.path),
            status: self.status,
            stream: self.stream,
            usage: self.usage,
            first_byte_ms: self.first_byte.map(|t| {
                u64::try_from(t.duration_since(self.started).as_millis()).unwrap_or(u64::MAX)
            }),
            total_ms: elapsed_ms(self.started),
        });
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Passes chunks through while the tap watches them.
pub(crate) struct TappedStream<S> {
    inner: S,
    tap: Tap,
}

impl<S> TappedStream<S> {
    pub(crate) fn new(inner: S, tap: Tap) -> Self {
        Self { inner, tap }
    }
}

impl<S> Stream for TappedStream<S>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.tap.chunk(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(other) => {
                this.tap.finish();
                Poll::Ready(other)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
