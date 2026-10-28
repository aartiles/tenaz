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
    Clock, Harness, HarnessError, HarnessInfo, PermissionDecision, Session, SessionExit,
    SessionSpec,
};
use crate::session::{SessionEvent, StopReason, Usage};
use crate::testing::{FastClock, ScriptedHarness};

/// How long the bootstrap thinks before the test stops the run.
const THINKING: Duration = Duration::from_secs(20);
/// How long the harness takes to answer the abort, inside the 5 s grace.
const ANSWER: Duration = Duration::from_secs(1);

/// The scripted harness, except that bootstrap sessions think silently,
/// stop the run after [`THINKING`], and answer the abort after [`ANSWER`].
/// Both are on the run's clock.
struct SilentBootstrap {
    scripted: ScriptedHarness,
    clock: Arc<FastClock>,
    stop: CancellationToken,
}

#[async_trait]
impl Harness for SilentBootstrap {
    fn info(&self) -> HarnessInfo {
        self.scripted.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        if !spec.label.ends_with("bootstrap") {
            return self.scripted.open(spec).await;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Box::new(Thinking {
            tx,
            rx,
            clock: self.clock.clone(),
            stop: self.stop.clone(),
        }))
    }
}

struct Thinking {
    tx: mpsc::UnboundedSender<SessionEvent>,
    rx: mpsc::UnboundedReceiver<SessionEvent>,
    clock: Arc<FastClock>,
    stop: CancellationToken,
}

#[async_trait]
impl Session for Thinking {
    async fn prompt(&mut self, _text: &str) -> Result<(), HarnessError> {
        let _ = self.tx.send(SessionEvent::TurnStart);
        let (clock, stop) = (self.clock.clone(), self.stop.clone());
        tokio::spawn(async move {
            clock.sleep(THINKING).await;
            stop.cancel();
        });
        Ok(())
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        self.rx.recv().await
    }

    async fn steer(&mut self, _text: &str) -> Result<(), HarnessError> {
        Ok(())
    }

    async fn abort(&mut self) -> Result<(), HarnessError> {
        let (tx, clock) = (self.tx.clone(), self.clock.clone());
        tokio::spawn(async move {
            clock.sleep(ANSWER).await;
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

/// Tokio's clock is paused, so the timers fire in the order of their
/// deadlines however busy the machine is: the answer to the abort always
/// comes before the grace runs out.
#[tokio::test(start_paused = true)]
async fn a_pause_after_a_long_wait_keeps_the_aborted_turn() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let stop = CancellationToken::new();
    let clock = Arc::new(FastClock::new(100));
    let harness = Arc::new(SilentBootstrap {
        scripted: ScriptedHarness::every_recovery(),
        clock: clock.clone(),
        stop: stop.clone(),
    });
    run(&project, deps(&project, vcs, harness, clock), stop).await;

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
