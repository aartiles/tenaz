//! A session whose harness fails mid-run is closed, not only dropped: a
//! harness that stops what its sessions left running (ADR 0039) does so
//! after a failed session too.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, try_run};
use crate::orchestrator::RunOptions;
use crate::ports::{
    Harness, HarnessError, HarnessInfo, PermissionDecision, Session, SessionExit, SessionSpec,
};
use crate::session::SessionEvent;
use crate::testing::{ScriptedHarness, VirtualClock};

/// The scripted harness, except that the first session's harness is gone
/// at its first prompt. The labels of the sessions closed are noted.
struct Failing {
    scripted: ScriptedHarness,
    opened: AtomicBool,
    closed: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Harness for Failing {
    fn info(&self) -> HarnessInfo {
        self.scripted.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        if self.opened.swap(true, Ordering::SeqCst) {
            return self.scripted.open(spec).await;
        }
        Ok(Box::new(Gone {
            label: spec.label,
            closed: self.closed.clone(),
        }))
    }
}

/// A session whose harness has exited: every command fails.
struct Gone {
    label: String,
    closed: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Session for Gone {
    async fn prompt(&mut self, _: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Closed)
    }
    async fn next_event(&mut self) -> Option<SessionEvent> {
        None
    }
    async fn steer(&mut self, _: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Closed)
    }
    async fn abort(&mut self) -> Result<(), HarnessError> {
        Err(HarnessError::Closed)
    }
    async fn answer_permission(
        &mut self,
        _: &str,
        _: PermissionDecision,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::Closed)
    }
    async fn close(&mut self, _: Duration) -> SessionExit {
        self.closed.lock().unwrap().push(self.label.clone());
        SessionExit { code: None }
    }
}

#[tokio::test(start_paused = true)]
async fn a_session_that_fails_is_closed() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let closed = Arc::new(Mutex::new(Vec::new()));
    let harness = Arc::new(Failing {
        scripted: ScriptedHarness::default(),
        opened: AtomicBool::new(false),
        closed: closed.clone(),
    });
    let deps = deps(&project, vcs, harness, Arc::new(VirtualClock::new()));
    let ended = try_run(
        &project,
        deps,
        CancellationToken::new(),
        RunOptions::default(),
    )
    .await;
    assert!(ended.is_err(), "the run went on: {ended:?}");
    let closed = closed.lock().unwrap().clone();
    assert_eq!(
        closed.len(),
        1,
        "the failed session was not closed: {closed:?}"
    );
}
