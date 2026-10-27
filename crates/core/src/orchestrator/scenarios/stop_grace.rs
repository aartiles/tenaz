//! A pause (Ctrl-C) during a long wait for the model still gives the
//! harness its grace period: the events the abort produces (the cut-off
//! message, the settle) reach the session's journal record. Seen with the
//! real Pi, which answers `abort` with an `aborted` message after the
//! runner had already given up on it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::ports::{
    Harness, HarnessError, HarnessInfo, PermissionDecision, Session, SessionExit, SessionSpec,
};
use crate::project::Project;
use crate::session::{SessionEvent, StopReason, Usage};
use crate::testing::{FastClock, ScriptedHarness};

/// The scripted harness, except that bootstrap sessions think silently
/// until aborted and then end the turn shortly after.
struct SilentBootstrap(ScriptedHarness);

#[async_trait]
impl Harness for SilentBootstrap {
    fn info(&self) -> HarnessInfo {
        self.0.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        if !spec.label.ends_with("bootstrap") {
            return self.0.open(spec).await;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Box::new(Thinking { tx, rx }))
    }
}

struct Thinking {
    tx: mpsc::UnboundedSender<SessionEvent>,
    rx: mpsc::UnboundedReceiver<SessionEvent>,
}

#[async_trait]
impl Session for Thinking {
    async fn prompt(&mut self, _text: &str) -> Result<(), HarnessError> {
        let _ = self.tx.send(SessionEvent::TurnStart);
        Ok(())
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        self.rx.recv().await
    }

    async fn steer(&mut self, _text: &str) -> Result<(), HarnessError> {
        Ok(())
    }

    /// 10 ms real is 1 s on the test clock, well inside the 5 s grace.
    async fn abort(&mut self) -> Result<(), HarnessError> {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            for event in [
                SessionEvent::MessageEnd {
                    server: None,
                    sent: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Aborted,
                    error_message: Some("Request was aborted".into()),
                    thinking_chars: 9,
                    text: String::new(),
                },
                SessionEvent::TurnEnd,
                SessionEvent::Settled { aborted: true },
            ] {
                let _ = tx.send(event);
            }
        });
        Ok(())
    }

    async fn answer_permission(
        &mut self,
        _request_id: &str,
        _decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        Ok(())
    }

    async fn close(&mut self, _grace: Duration) -> SessionExit {
        SessionExit { code: Some(0) }
    }
}

/// Stops the run 200 ms (20 s on the test clock) after its bootstrap
/// session starts; the handle says whether it started at all.
fn stop_into_the_bootstrap(
    project: &Project,
    stop: CancellationToken,
) -> tokio::task::JoinHandle<bool> {
    let log = project.path(".tenaz/private/orchestrator.log");
    tokio::spawn(async move {
        for _ in 0..3000 {
            if std::fs::read_to_string(&log).is_ok_and(|t| t.contains("bootstrap started")) {
                tokio::time::sleep(Duration::from_millis(200)).await;
                stop.cancel();
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    })
}

#[tokio::test]
async fn a_pause_after_a_long_wait_keeps_the_aborted_turn() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let stop = CancellationToken::new();
    let watcher = stop_into_the_bootstrap(&project, stop.clone());
    let harness = Arc::new(SilentBootstrap(ScriptedHarness::default()));
    let clock = Arc::new(FastClock::new(100));
    run(&project, deps(&project, vcs, harness, clock), stop).await;
    assert!(
        watcher.await.unwrap(),
        "the bootstrap session never started"
    );

    let journal = project.read(".tenaz/journal.jsonl").unwrap();
    let session = journal
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|r| r["type"] == "session" && r["role"] == "bootstrap")
        .unwrap_or_else(|| panic!("no bootstrap session record:\n{journal}"));
    assert_eq!(session["last_stop_reason"], "aborted", "{session}");
    assert_eq!(session["turns"], 1, "{session}");
    assert!(journal.contains(r#""type":"paused""#), "{journal}");
}
