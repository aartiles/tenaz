//! Media engines that borrow the memory for one job.

use std::time::Duration;

use tenaz_core::ports::MediaError;

pub mod comfyui;
pub mod meshes;
pub mod openai_images;

/// Asks a server with a load API, as mlx-serve has, to free `model`:
/// `POST {base_url}/unload-model {"model": ...}`. mlx-serve keeps a media
/// model loaded after its job, so without this the LLM reloads next to it.
/// A server without the route (404, 405 or 501) has nothing Tenaz can free,
/// and that is not an error; a 404 for a model the server does not know
/// (mlx-serve's `model_not_found`) is.
pub(crate) async fn unload_model(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
) -> Result<(), MediaError> {
    let response = client
        .post(format!("{base_url}/unload-model"))
        .json(&serde_json::json!({ "model": model }))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| MediaError::Unreachable(e.to_string()))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let text = response.text().await.unwrap_or_default();
    if is_missing_route(status) && !text.contains("model_not_found") {
        return Ok(());
    }
    Err(MediaError::Failed(format!(
        "/unload-model {model} returned {status}: {}",
        tenaz_core::util::clip(text.trim(), 500)
    )))
}

fn is_missing_route(status: reqwest::StatusCode) -> bool {
    use reqwest::StatusCode;
    matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
    )
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::routing::post;

    use super::*;

    /// A server whose `/v1/unload-model` answers `status` with `reason`.
    async fn server(status: StatusCode, reason: &'static str) -> String {
        let app = axum::Router::new().route(
            "/v1/unload-model",
            post(move || async move { (status, reason) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}/v1")
    }

    #[tokio::test]
    async fn a_refused_unload_returns_the_server_s_reason() {
        let url = server(StatusCode::SERVICE_UNAVAILABLE, "busy generating").await;
        let error = unload_model(&crate::http::local_client(), &url, "TRELLIS.2")
            .await
            .unwrap_err();
        assert!(
            matches!(error, MediaError::Failed(ref m) if m.contains("TRELLIS.2") && m.contains("503") && m.contains("busy generating")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_server_without_the_route_has_nothing_to_free() {
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::NOT_IMPLEMENTED,
        ] {
            let url = server(status, "").await;
            unload_model(&crate::http::local_client(), &url, "m")
                .await
                .unwrap_or_else(|e| panic!("{status}: {e}"));
        }
    }

    /// mlx-serve answers 404 for a model it does not know, too.
    #[tokio::test]
    async fn an_unknown_model_is_an_error() {
        let url = server(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"Unknown model id","type":"model_not_found"}}"#,
        )
        .await;
        let error = unload_model(&crate::http::local_client(), &url, "Idoll")
            .await
            .unwrap_err();
        assert!(
            matches!(error, MediaError::Failed(ref m) if m.contains("Unknown model id")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_server_that_is_gone_is_unreachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}/v1");
        let error = unload_model(&crate::http::local_client(), &url, "m")
            .await
            .unwrap_err();
        assert!(matches!(error, MediaError::Unreachable(_)), "{error}");
    }
}
