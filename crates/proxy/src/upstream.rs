//! Sending a request to the engine: held while the lease has the LLM
//! away, and a completion that Sushi refuses for its output limit sent
//! once more with the limit it has room for (ADR 0046).

use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;

use crate::{Api, Proxy, ProxyEvent, elapsed_ms, set_output_limit};

/// A completion request of an API, as the proxy forwards it.
pub(crate) struct Completion {
    pub(crate) api: Api,
    pub(crate) request: serde_json::Value,
}

/// Where the proxy sends a request, and how.
pub(crate) struct Forward {
    /// The proxy's id of the request.
    pub(crate) request: u64,
    pub(crate) method: axum::http::Method,
    pub(crate) url: String,
    pub(crate) headers: reqwest::header::HeaderMap,
}

/// An engine the proxy could not reach: its error, as the harness is
/// answered with it.
pub(crate) struct Unreachable(String);

impl IntoResponse for Unreachable {
    fn into_response(self) -> Response {
        let error = serde_json::json!({ "error": { "message": self.0, "type": "proxy_error" } });
        (StatusCode::BAD_GATEWAY, axum::Json(error)).into_response()
    }
}

/// The engine's answer, before the proxy passes it on.
pub(crate) struct Upstream {
    pub(crate) status: reqwest::StatusCode,
    pub(crate) headers: reqwest::header::HeaderMap,
    pub(crate) body: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>>,
}

impl Upstream {
    /// The answer as the engine streams it.
    fn streamed(response: reqwest::Response) -> Self {
        Self {
            status: response.status(),
            headers: response.headers().clone(),
            body: response
                .bytes_stream()
                .map(|chunk| chunk.map_err(std::io::Error::other))
                .boxed(),
        }
    }
}

impl Proxy {
    /// The engine's answer to `body`, sent as `forward` says, and, for a
    /// completion of an API, the request as last sent ([`Proxy::refit`]).
    pub(crate) async fn answer(
        &self,
        forward: &Forward,
        body: bytes::Bytes,
        completion: Option<Completion>,
    ) -> Result<(Upstream, Option<serde_json::Value>), Unreachable> {
        let first = self.send(forward, body).await?;
        let Some(completion) = completion else {
            return Ok((Upstream::streamed(first), None));
        };
        if first.status() != StatusCode::BAD_REQUEST {
            return Ok((Upstream::streamed(first), Some(completion.request)));
        }
        let (upstream, sent) = self.refit(forward, first, completion).await?;
        Ok((upstream, Some(sent)))
    }

    /// Sends `body` as `forward` says, once the lease has the LLM back. An
    /// engine not reached is reported.
    async fn send(
        &self,
        forward: &Forward,
        body: bytes::Bytes,
    ) -> Result<reqwest::Response, Unreachable> {
        if !self.gate.is_open() {
            let waited = Instant::now();
            self.gate.wait_open().await;
            self.observe(ProxyEvent::Held {
                request: forward.request,
                ms: elapsed_ms(waited),
            });
        }
        let sent = self
            .client
            .request(forward.method.clone(), &forward.url)
            .headers(forward.headers.clone())
            .body(body)
            .send()
            .await;
        sent.map_err(|error| {
            let message = tenaz_core::session::engine_unreachable_text(&error);
            self.observe(ProxyEvent::EngineError {
                request: forward.request,
                message: message.clone(),
            });
            Unreachable(message)
        })
    }

    /// The engine's answer to `completion`, after the 400 it gave
    /// `first`, and the request as last sent. A 400 that names an output
    /// limit the engine has room for
    /// ([`tenaz_core::session::fitting_output_limit`]) is not passed on: the
    /// request is sent once more with that limit. Sushi refuses a request
    /// whose prompt and output limit together do not fit its memory, and
    /// the harness would end the session on the error.
    async fn refit(
        &self,
        forward: &Forward,
        first: reqwest::Response,
        completion: Completion,
    ) -> Result<(Upstream, serde_json::Value), Unreachable> {
        let Completion { api, request } = completion;
        let status = first.status();
        let headers = first.headers().clone();
        let error = first.bytes().await;
        let resend = error
            .as_ref()
            .ok()
            .and_then(|error| Resend::of(error, &request, api));
        let Some(resend) = resend else {
            let body =
                futures::stream::once(std::future::ready(error.map_err(std::io::Error::other)));
            let refused = Upstream {
                status,
                headers,
                body: body.boxed(),
            };
            return Ok((refused, request));
        };
        self.observe(ProxyEvent::OutputLimitLowered {
            request: forward.request,
            asked: resend.fit.asked,
            fits: resend.fit.fits,
        });
        let second = self.send(forward, resend.body.into()).await?;
        Ok((Upstream::streamed(second), resend.request))
    }
}

/// A request sent again with the output limit the engine has room for.
struct Resend {
    fit: tenaz_core::session::OutputFit,
    /// The request with the lowered limit, and its body.
    request: serde_json::Value,
    body: Vec<u8>,
}

impl Resend {
    /// The resend of `request` of `api` that the engine's `error` calls for.
    /// `None` when it names no limit that fits, or for a request that is not
    /// a JSON object.
    fn of(error: &[u8], request: &serde_json::Value, api: Api) -> Option<Self> {
        let fit = tenaz_core::session::fitting_output_limit(&String::from_utf8_lossy(error))?;
        let (request, body) = lowered(request, api, fit.fits)?;
        Some(Self { fit, request, body })
    }
}

/// `request` of `api` with an output limit of `limit` ([`set_output_limit`]),
/// and its body. `None` for a request that is not a JSON object.
fn lowered(
    request: &serde_json::Value,
    api: Api,
    limit: u64,
) -> Option<(serde_json::Value, Vec<u8>)> {
    let mut lowered = request.clone();
    set_output_limit(lowered.as_object_mut()?, api, limit);
    let body = serde_json::to_vec(&lowered).ok()?;
    Some((lowered, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lowered_to_13186(request: &serde_json::Value, api: Api) -> serde_json::Value {
        lowered(request, api, 13186).unwrap().0
    }

    #[test]
    fn every_limit_the_request_names_is_lowered() {
        let both = serde_json::json!({"max_tokens": 16384, "max_completion_tokens": 16384, "stream": true});
        assert_eq!(
            lowered_to_13186(&both, Api::Completions),
            serde_json::json!({"max_tokens": 13186, "max_completion_tokens": 13186, "stream": true})
        );
    }

    #[test]
    fn a_request_without_a_limit_gets_one_under_its_api_s_name() {
        assert_eq!(
            lowered_to_13186(&serde_json::json!({}), Api::Completions),
            serde_json::json!({"max_tokens": 13186})
        );
        assert_eq!(
            lowered_to_13186(&serde_json::json!({}), Api::Responses),
            serde_json::json!({"max_output_tokens": 13186})
        );
    }

    #[test]
    fn a_request_that_is_not_an_object_is_not_sent_again() {
        assert_eq!(
            lowered(&serde_json::json!([1]), Api::Completions, 13186),
            None
        );
    }
}
