//! A generic engine: an OpenAI-compatible URL plus start and stop commands.
//! Loading runs the start command and waits for the model to be listed;
//! unloading runs the stop command and waits for it to disappear.

use std::time::Duration;

use async_trait::async_trait;
use tenaz_core::ports::{EngineAdapter, EngineError, ModelState};

use super::openai::{list_models, wait_for_model};

/// How to start and stop the engine process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commands {
    pub start: Option<Vec<String>>,
    pub stop: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct GenericEngine {
    name: String,
    base_url: String,
    commands: Commands,
    /// How long a load may take (cold weights from SSD).
    load_wait: Duration,
    client: reqwest::Client,
}

impl GenericEngine {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        commands: Commands,
        load_wait: Duration,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            commands,
            load_wait,
            client: crate::http::local_client(),
        }
    }

    async fn run(command: &[String]) -> Result<(), EngineError> {
        let (program, args) = command
            .split_first()
            .ok_or_else(|| EngineError::Refused("empty command".to_owned()))?;
        let status = tokio::process::Command::new(program)
            .args(args)
            .status()
            .await
            .map_err(|e| EngineError::Refused(format!("{program}: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(EngineError::Refused(format!(
                "{} exited with {status}",
                command.join(" ")
            )))
        }
    }
}

#[async_trait]
impl EngineAdapter for GenericEngine {
    fn name(&self) -> &str {
        &self.name
    }

    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        list_models(&self.client, &self.base_url, Duration::from_secs(5)).await
    }

    async fn load(&self, model: &str) -> Result<(), EngineError> {
        let start = self.commands.start.as_ref().ok_or_else(|| {
            EngineError::Refused("no start command configured for this engine".to_owned())
        })?;
        Self::run(start).await?;
        wait_for_model(&self.client, &self.base_url, model, self.load_wait, |m| {
            m.is_some_and(|m| m.loaded && m.ready)
        })
        .await
    }

    async fn unload(&self, model: &str) -> Result<(), EngineError> {
        let stop = self.commands.stop.as_ref().ok_or_else(|| {
            EngineError::Refused("no stop command configured for this engine".to_owned())
        })?;
        Self::run(stop).await?;
        wait_for_model(
            &self.client,
            &self.base_url,
            model,
            Duration::from_secs(120),
            |m| m.is_none_or(|m| !m.loaded),
        )
        .await
    }

    /// Only with both commands: a stop without a start could not bring the
    /// LLM back.
    fn can_unload(&self) -> bool {
        self.commands.start.is_some() && self.commands.stop.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_runs_the_command_then_waits_for_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"data": [{"id": "m"}]})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let commands = Commands {
            start: Some(vec!["touch".into(), marker.to_string_lossy().into_owned()]),
            stop: None,
        };
        let engine = GenericEngine::new(
            "generic",
            format!("http://127.0.0.1:{port}/v1"),
            commands,
            Duration::from_secs(5),
        );
        engine.load("m").await.unwrap();
        assert!(marker.exists());
        assert!(matches!(
            engine.unload("m").await.unwrap_err(),
            EngineError::Refused(_)
        ));
        assert_eq!(
            engine.models().await.unwrap()[0],
            ModelState {
                id: "m".into(),
                loaded: true,
                ready: true,
                context_length: None
            }
        );
        let failing = GenericEngine::new(
            "g",
            format!("http://127.0.0.1:{port}/v1"),
            Commands {
                start: Some(vec!["false".into()]),
                stop: None,
            },
            Duration::from_secs(1),
        );
        assert!(matches!(
            failing.load("m").await.unwrap_err(),
            EngineError::Refused(_)
        ));
    }

    #[test]
    fn can_unload_only_with_both_commands() {
        let engine = |start: bool, stop: bool| {
            GenericEngine::new(
                "g",
                "http://127.0.0.1:1/v1",
                Commands {
                    start: start.then(|| vec!["start".into()]),
                    stop: stop.then(|| vec!["stop".into()]),
                },
                Duration::from_secs(1),
            )
        };
        assert!(engine(true, true).can_unload());
        assert!(!engine(true, false).can_unload());
        assert!(!engine(false, true).can_unload());
        assert!(!engine(false, false).can_unload());
    }
}
