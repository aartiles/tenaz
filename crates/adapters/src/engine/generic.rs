//! A generic engine: an OpenAI-compatible URL plus start and stop commands.
//! Loading runs the start command and waits for the model to be listed;
//! unloading runs the stop command and waits for it to disappear. The
//! commands start and stop the whole engine, so several models are loaded
//! and freed together, with one command each.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tenaz_core::ports::{EngineAdapter, EngineError, ModelState, serves};

use super::openai::{list_models, wait_for_model};

/// How to start and stop the engine process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commands {
    pub start: Option<Vec<String>>,
    pub stop: Option<Vec<String>>,
}

/// How long a stop may take to free the models.
const STOP_WAIT: Duration = Duration::from_secs(120);

/// `a, b`.
fn names(ids: &[&String]) -> String {
    ids.iter()
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The engine's start or stop command.
#[derive(Clone, Copy)]
enum Step {
    Start,
    Stop,
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

    /// Runs the start or stop command, then waits for each of `models` to
    /// serve, or to be gone, all within the one wait.
    async fn step(&self, step: Step, models: &[String]) -> Result<(), EngineError> {
        let (command, what, wait) = match step {
            Step::Start => (self.commands.start.as_ref(), "start", self.load_wait),
            Step::Stop => (self.commands.stop.as_ref(), "stop", STOP_WAIT),
        };
        let command = command.ok_or_else(|| {
            EngineError::Refused(format!("no {what} command configured for this engine"))
        })?;
        Self::run(command).await?;
        let deadline = Instant::now() + wait;
        for id in models {
            let left = deadline.saturating_duration_since(Instant::now());
            wait_for_model(&self.client, &self.base_url, id, left, |m| match step {
                Step::Start => m.is_some_and(ModelState::serving),
                Step::Stop => m.is_none_or(|m| !m.loaded),
            })
            .await
            .map_err(|e| match e {
                EngineError::Timeout(_) => EngineError::Timeout(format!(
                    "{id} did not reach the wanted state within {wait:?} of the {what} command"
                )),
                other => other,
            })?;
        }
        Ok(())
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
        self.step(Step::Start, &[model.to_owned()]).await
    }

    async fn unload(&self, model: &str) -> Result<(), EngineError> {
        self.unload_all(&[model.to_owned()]).await
    }

    /// Nothing when the engine serves every one of `models` already; one
    /// start, which brings them all back, when it does not answer or serves
    /// none of them (what a stop leaves). An engine that serves some of
    /// them is running: a start would launch a second one, so it is
    /// refused, naming the others.
    async fn load_all(&self, models: &[String]) -> Result<(), EngineError> {
        let Ok(listed) = self.models().await else {
            return self.step(Step::Start, models).await;
        };
        let (served, missing): (Vec<&String>, Vec<&String>) =
            models.iter().partition(|id| serves(&listed, id));
        match (served.is_empty(), missing.is_empty()) {
            (_, true) => Ok(()),
            (true, false) => self.step(Step::Start, models).await,
            (false, false) => Err(EngineError::Refused(format!(
                "the engine serves {} but not {}; restart it with every model",
                names(&served),
                names(&missing)
            ))),
        }
    }

    /// One stop, then a wait until none of `models` is listed loaded.
    async fn unload_all(&self, models: &[String]) -> Result<(), EngineError> {
        self.step(Step::Stop, models).await
    }

    /// Only with both commands: a stop without a start could not bring the
    /// LLM back.
    fn can_unload(&self) -> bool {
        self.commands.start.is_some() && self.commands.stop.is_some()
    }
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse as _;

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

    /// A server listing `m` and `r` until the file `stopped` exists. Then
    /// it fails every request, as an engine whose process was stopped, or,
    /// when it `stays_up`, lists nothing, as an engine whose models were
    /// unloaded (LM Studio's `lms unload`).
    async fn stoppable(dir: &std::path::Path, stays_up: bool) -> String {
        let stopped = dir.join("stopped");
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(move || {
                let data = if stopped.exists() {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{"id": "m"}, {"id": "r"}])
                };
                let down = stopped.exists() && !stays_up;
                async move {
                    if down {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    axum::Json(serde_json::json!({ "data": data })).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}/v1")
    }

    /// A command that appends `word` to `log`, then runs `then`.
    fn logged(log: &std::path::Path, word: &str, then: &str) -> Vec<String> {
        let script = format!("echo {word} >> '{}' && {then}", log.display());
        vec!["sh".into(), "-c".into(), script]
    }

    /// The commands around an image, on a server that stops answering or
    /// stays up when stopped, and what they appended to the log.
    async fn cycle(stays_up: bool) -> String {
        let dir = tempfile::tempdir().unwrap();
        let base_url = stoppable(dir.path(), stays_up).await;
        let (log, stopped) = (dir.path().join("log"), dir.path().join("stopped"));
        let commands = Commands {
            start: Some(logged(
                &log,
                "start",
                &format!("rm '{}'", stopped.display()),
            )),
            stop: Some(logged(
                &log,
                "stop",
                &format!("touch '{}'", stopped.display()),
            )),
        };
        let engine = GenericEngine::new("g", base_url, commands, Duration::from_secs(5));
        let models = ["m".to_owned(), "r".to_owned()];
        engine.unload_all(&models).await.unwrap();
        engine.load_all(&models).await.unwrap();
        // The engine serves both: nothing to start.
        engine.load_all(&models).await.unwrap();
        std::fs::read_to_string(&log).unwrap()
    }

    #[tokio::test]
    async fn several_models_are_freed_and_loaded_with_one_command_each() {
        assert_eq!(cycle(false).await, "stop\nstart\n");
    }

    /// A stop that unloads the models and leaves the server up is undone by
    /// the start command all the same.
    #[tokio::test]
    async fn an_engine_that_stays_up_without_its_models_is_started() {
        assert_eq!(cycle(true).await, "stop\nstart\n");
    }

    /// An engine that serves one model of the run and not another is
    /// running: it is not started a second time.
    #[tokio::test]
    async fn an_engine_serving_some_of_the_models_is_not_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let base_url = stoppable(dir.path(), false).await;
        let log = dir.path().join("log");
        let commands = Commands {
            start: Some(logged(&log, "start", "true")),
            stop: None,
        };
        let engine = GenericEngine::new("g", base_url, commands, Duration::from_secs(5));
        let error = engine
            .load_all(&["m".to_owned(), "critic".to_owned()])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "engine refused: the engine serves m but not critic; restart it with every model"
        );
        assert!(!log.exists(), "the start command ran");
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
