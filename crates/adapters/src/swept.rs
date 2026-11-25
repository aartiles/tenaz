//! A harness whose sessions are followed by the search for what they left
//! running (ADR 0039): a server the agent started detached from its shell
//! tool outlives the harness, which Tenaz kills when the session ends.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tenaz_core::ports::{
    Harness, HarnessError, HarnessInfo, PermissionDecision, Session, SessionExit, SessionSpec,
};
use tenaz_core::session::SessionEvent;

use crate::engine::spans::EngineSpans;
use crate::process::{Left, sweep};

/// Wraps a harness: once one of its sessions is closed, what was born
/// since it opened, from its working directory, in a session of its own,
/// is stopped, except what was born while one of the engine's commands in
/// `spared` ran.
pub struct SweptHarness {
    inner: Arc<dyn Harness>,
    spared: EngineSpans,
}

impl SweptHarness {
    pub fn new(inner: Arc<dyn Harness>, spared: EngineSpans) -> Self {
        Self { inner, spared }
    }
}

/// Stops what the session `label` left from `root` since `at`.
async fn sweep_after(label: &str, root: PathBuf, at: SystemTime, spared: EngineSpans) {
    let left = Left {
        root,
        at,
        group: None,
        spared: Some(spared),
    };
    sweep(left, &format!("session {label}")).await;
}

impl std::fmt::Debug for SweptHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SweptHarness")
            .field("inner", &self.inner.info())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Harness for SweptHarness {
    fn info(&self) -> HarnessInfo {
        self.inner.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        // Before the harness starts: what it leaves is born after.
        let at = SystemTime::now();
        let label = spec.label.clone();
        let root = spec.cwd.clone();
        let inner = match self.inner.open(spec).await {
            Ok(inner) => inner,
            // It may have started and left something before it failed.
            Err(error) => {
                sweep_after(&label, root, at, self.spared.clone()).await;
                return Err(error);
            }
        };
        Ok(Box::new(SweptSession {
            inner,
            label,
            root,
            at,
            spared: self.spared.clone(),
        }))
    }
}

struct SweptSession {
    inner: Box<dyn Session>,
    label: String,
    root: PathBuf,
    at: SystemTime,
    spared: EngineSpans,
}

#[async_trait]
impl Session for SweptSession {
    async fn prompt(&mut self, text: &str) -> Result<(), HarnessError> {
        self.inner.prompt(text).await
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        self.inner.next_event().await
    }

    async fn steer(&mut self, text: &str) -> Result<(), HarnessError> {
        self.inner.steer(text).await
    }

    async fn abort(&mut self) -> Result<(), HarnessError> {
        self.inner.abort().await
    }

    async fn answer_permission(
        &mut self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        self.inner.answer_permission(request_id, decision).await
    }

    /// Closes the harness, then stops what the session left: the harness
    /// is gone by then, so what it started is adopted, as what a check
    /// leaves is once `verify` ends.
    async fn close(&mut self, grace: Duration) -> SessionExit {
        let exit = self.inner.close(grace).await;
        let spared = self.spared.clone();
        sweep_after(&self.label, self.root.clone(), self.at, spared).await;
        exit
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use tenaz_core::ports::SessionModel;

    use super::*;
    use crate::testing::{KillOnDrop, detached_sleep, gone_within};

    /// A harness whose session, when opened, runs `script` in the
    /// session's directory and waits for it, as an agent's shell tool runs
    /// a command; closing it does nothing more. One that `fails` then
    /// refuses the session, as a harness whose handshake failed.
    struct Shell {
        script: String,
        fails: bool,
    }

    impl Shell {
        fn running(script: String) -> Arc<Self> {
            Arc::new(Self {
                script,
                fails: false,
            })
        }
    }

    #[async_trait]
    impl Harness for Shell {
        fn info(&self) -> HarnessInfo {
            HarnessInfo {
                name: "shell".to_owned(),
                version: "0".to_owned(),
            }
        }

        async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
            let status = tokio::process::Command::new("sh")
                .args(["-c", &self.script])
                .current_dir(&spec.cwd)
                .status()
                .await
                .map_err(|e| HarnessError::Spawn(e.to_string()))?;
            assert!(status.success(), "the agent's command failed");
            if self.fails {
                return Err(HarnessError::Protocol("no handshake".to_owned()));
            }
            Ok(Box::new(Done))
        }
    }

    struct Done;

    #[async_trait]
    impl Session for Done {
        async fn prompt(&mut self, _: &str) -> Result<(), HarnessError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            None
        }
        async fn steer(&mut self, _: &str) -> Result<(), HarnessError> {
            Ok(())
        }
        async fn abort(&mut self) -> Result<(), HarnessError> {
            Ok(())
        }
        async fn answer_permission(
            &mut self,
            _: &str,
            _: PermissionDecision,
        ) -> Result<(), HarnessError> {
            Ok(())
        }
        async fn close(&mut self, _: Duration) -> SessionExit {
            SessionExit { code: Some(0) }
        }
    }

    fn spec(cwd: &Path) -> SessionSpec {
        SessionSpec {
            label: "001-builder-T001".to_owned(),
            cwd: cwd.to_path_buf(),
            persist_dir: None,
            raw_log: None,
            thinking_log: None,
            stderr_log: None,
            model: SessionModel {
                name: "engine/m".to_owned(),
                id: "m".to_owned(),
                context_window: 32_768,
                supports_images: false,
                thinking: None,
            },
        }
    }

    /// The project's detached `sleep`, as the agent's server, and the
    /// harness that starts it.
    fn server(project: &Path, fails: bool) -> (Arc<Shell>, std::path::PathBuf) {
        let pidfile = project.join("server.pid");
        let shell = Arc::new(Shell {
            script: detached_sleep(&pidfile),
            fails,
        });
        (shell, pidfile)
    }

    /// Opens a session of `harness` in `project` and closes it, if it
    /// opened; whether it did.
    async fn run_session(harness: &SweptHarness, project: &Path) -> Result<(), HarnessError> {
        let mut session = harness.open(spec(project)).await?;
        session.close(Duration::ZERO).await;
        Ok(())
    }

    /// Whether the server whose pid is in `pidfile` is gone.
    fn gone(pidfile: &Path) -> bool {
        let server = KillOnDrop::read(pidfile);
        gone_within(server.pid(), Duration::from_secs(5))
    }

    /// The reviewer of the 2026-11-19 run started `npx vite preview`
    /// detached from its shell tool and never stopped it: here a detached
    /// `sleep`, started from the project during the session, is gone once
    /// the session is closed.
    #[tokio::test]
    async fn stops_what_a_closed_session_left() {
        let project = tempfile::tempdir().unwrap();
        let (shell, pidfile) = server(project.path(), false);
        let harness = SweptHarness::new(shell, EngineSpans::default());
        run_session(&harness, project.path()).await.unwrap();
        assert!(gone(&pidfile), "the server the session left still runs");
    }

    /// A harness that started the server and then failed to open the
    /// session gives its error, and the server is gone all the same.
    #[tokio::test]
    async fn stops_what_a_session_that_failed_to_open_left() {
        let project = tempfile::tempdir().unwrap();
        let (shell, pidfile) = server(project.path(), true);
        let harness = SweptHarness::new(shell, EngineSpans::default());
        let opened = run_session(&harness, project.path()).await;
        assert!(gone(&pidfile), "the server the session left still runs");
        assert!(
            matches!(opened, Err(HarnessError::Protocol(_))),
            "{opened:?}"
        );
    }

    /// An engine command that ended before the server was born, by more
    /// than the slack, spares the engine, not the server.
    #[tokio::test]
    async fn an_earlier_engine_command_does_not_spare_the_agents_server() {
        let project = tempfile::tempdir().unwrap();
        let (shell, pidfile) = server(project.path(), false);
        let spans = EngineSpans::default();
        let now = SystemTime::now();
        spans.noted(now - Duration::from_secs(20), now - Duration::from_secs(18));
        let harness = SweptHarness::new(shell, spans);
        run_session(&harness, project.path()).await.unwrap();
        assert!(gone(&pidfile), "the server was spared with the engine");
    }

    /// The engine Tenaz restarts during a session may detach from the
    /// project the same way; born while its command ran, it stays.
    #[tokio::test]
    async fn keeps_what_an_engine_command_started_during_the_session() {
        let project = tempfile::tempdir().unwrap();
        let pidfile = project.path().join("engine.pid");
        let spans = EngineSpans::default();
        let harness = SweptHarness::new(Shell::running("true".to_owned()), spans.clone());
        let mut session = harness.open(spec(project.path())).await.unwrap();
        {
            let _running = spans.begin();
            let status = tokio::process::Command::new("sh")
                .args(["-c", &detached_sleep(&pidfile)])
                .current_dir(project.path())
                .status()
                .await
                .unwrap();
            assert!(status.success());
        }
        let engine = KillOnDrop::read(&pidfile);
        session.close(Duration::ZERO).await;
        assert!(
            !gone_within(engine.pid(), Duration::from_millis(200)),
            "the engine was stopped"
        );
    }
}
