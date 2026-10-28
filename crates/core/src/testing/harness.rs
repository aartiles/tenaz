//! The scripted harness: a stand-in agent that plays a small goal without
//! a model, with each recovery the orchestrator handles opt-in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::ports::{
    Harness, HarnessError, HarnessInfo, PermissionDecision, Session, SessionExit, SessionSpec,
};
use crate::session::SessionEvent;

use super::abort::Abort;
use super::paced::{self, PacedReceiver, PacedSender};
use super::player::{Player, field};

/// The scripted stand-in for an agent harness. By default it plays the
/// happy path of a four-task goal: a bootstrap that writes the spec, map,
/// `verify` and cards, builders that write their file and check, reviews
/// that accept, replans that change nothing, and a final review that
/// accepts. Each field below adds one thing to recover from;
/// [`ScriptedHarness::every_recovery`] is the script `tenaz demo` plays.
#[derive(Clone, Debug, Default)]
pub struct ScriptedHarness {
    /// The first two bootstrap sessions read a large file, then fail in
    /// the engine: out of memory, then a prompt over the most context it
    /// can hold now.
    pub engine_failures: bool,
    /// The bootstrap also writes the brief, which is not its to change:
    /// the edit is reverted.
    pub tampered_brief: bool,
    /// A replan and this task's builder also write this task's card, which
    /// is not theirs to change: the edits are reverted.
    pub tampered_card: Option<String>,
    /// The first review of this task asks for changes with a MAJOR finding.
    pub rejected_review: Option<String>,
    /// The first review of this task ends without a verdict.
    pub no_verdict_review: Option<String>,
    /// The first attempt of this task repeats one tool call until it is
    /// aborted.
    pub looping_task: Option<String>,
    /// Every attempt of this task writes a failing check, so it is split.
    pub failing_task: Option<String>,
    /// This task's builder finishes, then circles between two calls with
    /// the same results until it is aborted.
    pub alternating_task: Option<String>,
    /// The first builder session of this task that does the work then
    /// reads past its context stop, so a fresh session continues it.
    pub context_stop_task: Option<String>,
    /// The first builder session of this task writes its file, then the
    /// harness compacts its context twice, as Pi does near the window.
    pub compact_task: Option<String>,
    /// When a builder session for this task starts, it cancels this token
    /// (a test's Ctrl-C) and waits until its turn is aborted.
    pub stop_at: Option<(String, tokio_util::sync::CancellationToken)>,
    /// The first builder session for this task dies mid-turn between tool
    /// calls: it writes to stderr and exits with code 1.
    pub crash_task: Option<String>,
    /// The first builder session for this task dies while a shell call runs,
    /// as fx does on output that is not UTF-8.
    pub crash_in_tool_task: Option<String>,
    /// Where bootstrap writes `verify`; `None` is `verify` at the root.
    pub verify_path: Option<String>,
    /// Every builder session for this task grows its context past the stop
    /// without writing anything, so its attempt outgrows the budget.
    pub outgrow_task: Option<String>,
    /// The first builder session for this task that does the work leaves
    /// out its check, and adds it when the orchestrator asks.
    pub forget_checks_task: Option<String>,
    /// The first builder session for this task reasons until its reply is
    /// cut at the output limit, and the harness exits, as fx does.
    pub truncate_task: Option<String>,
    /// The first bootstrap session after the engine failures does the same.
    pub truncate_bootstrap: bool,
    /// The first bootstrap session writes the spec and a new file, then
    /// ends without the rest.
    pub partial_bootstrap: bool,
    /// Each bootstrap session writes only the files of the step its prompt
    /// names. Otherwise the first one writes them all.
    pub stepwise_bootstrap: bool,
    /// The first review of this task reads until its context is stopped.
    pub outgrow_review: Option<String>,
    /// The first review of this task asks for changes with a MINOR finding.
    pub minor_review: Option<String>,
    /// The builder of this task also writes an image, `assets/<task>.png`.
    pub image_task: Option<String>,
    /// The first attempt of this task deletes T001's check (any task but
    /// T001); later ones put it back.
    pub drop_check_task: Option<String>,
    /// The first builder session of this task keeps writing new files until
    /// it is stopped at its turn budget.
    pub long_task: Option<String>,
    /// The first `hang_times` builder turns of this task start a shell call
    /// that never ends until the turn is aborted.
    pub hang_task: Option<String>,
    /// How many of `hang_task`'s turns hang.
    pub hang_times: u32,
    /// The first builder session of this task writes its file in one reply
    /// that crosses the context stop, in Pi's order: the reply ends, then
    /// its write runs, unless the turn was aborted by then.
    pub big_write_task: Option<String>,
}

impl ScriptedHarness {
    /// Every recovery of a whole run at once, as `tenaz demo` plays it:
    /// engine failures and a capacity cap in the bootstrap, a context stop
    /// in T001, a loop in T002, a task that fails until it is split (T003),
    /// an alternating loop and a review without a verdict in T004, a review
    /// rejection of T001, and out-of-role edits.
    pub fn every_recovery() -> Self {
        Self {
            engine_failures: true,
            tampered_brief: true,
            tampered_card: Some("T001".into()),
            rejected_review: Some("T001".into()),
            no_verdict_review: Some("T004".into()),
            looping_task: Some("T002".into()),
            failing_task: Some("T003".into()),
            alternating_task: Some("T004".into()),
            context_stop_task: Some("T001".into()),
            ..Self::default()
        }
    }

    /// The version pinned in the ledger of a fake run.
    pub fn version(&self) -> String {
        "fake-pi 0.0.1".to_owned()
    }
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn info(&self) -> HarnessInfo {
        HarnessInfo {
            name: "fake".to_owned(),
            version: self.version(),
        }
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        let (events, rx) = paced::channel();
        Ok(Box::new(ScriptedSession {
            cwd: spec.cwd,
            raw_log: spec.raw_log,
            stderr_log: spec.stderr_log,
            exit_code: Some(0),
            events,
            rx,
            abort: Arc::default(),
            prompts: 0,
            first: String::new(),
            role: String::new(),
            play: None,
            script: self.clone(),
        }))
    }
}

struct ScriptedSession {
    cwd: PathBuf,
    raw_log: Option<PathBuf>,
    stderr_log: Option<PathBuf>,
    exit_code: Option<i32>,
    events: PacedSender,
    rx: PacedReceiver,
    abort: Arc<Abort>,
    prompts: u32,
    /// The first prompt, which a prompt after a stopped tool call continues.
    first: String,
    /// The `Role` field of the first prompt.
    role: String,
    play: Option<tokio::task::JoinHandle<()>>,
    script: ScriptedHarness,
}

#[async_trait]
impl Session for ScriptedSession {
    async fn prompt(&mut self, text: &str) -> Result<(), HarnessError> {
        self.prompts += 1;
        self.abort.clear();
        if self.first.is_empty() {
            text.clone_into(&mut self.first);
        }
        // After a stopped tool call the session goes on with its first task.
        let text = if text.contains("was stopped") {
            self.first.clone()
        } else {
            text.to_owned()
        };
        if let Some(role) = field(&text, "Role")
            && self.role.is_empty()
        {
            role.clone_into(&mut self.role);
        }
        let player = Player {
            cwd: self.cwd.clone(),
            stderr_log: self.stderr_log.clone(),
            events: self.events.clone(),
            abort: self.abort.clone(),
            prompts: self.prompts,
            call: 0,
            script: self.script.clone(),
        };
        let prompt = text;
        self.play = Some(tokio::spawn(async move { player.play(&prompt).await }));
        Ok(())
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        let event = self.rx.recv().await?;
        if let SessionEvent::Exited { code } = event {
            self.exit_code = code;
        }
        if let Some(log) = &self.raw_log
            && let Ok(line) = serde_json::to_string(&event)
        {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .and_then(|mut f| {
                    std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes())
                });
        }
        Some(event)
    }

    /// Steers are appended to `.tenaz/private/fake-steers.log`, after the role.
    async fn steer(&mut self, text: &str) -> Result<(), HarnessError> {
        let log = self.cwd.join(".tenaz/private/fake-steers.log");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .and_then(|mut f| {
                std::io::Write::write_all(&mut f, format!("{}: {text}\n", self.role).as_bytes())
            });
        Ok(())
    }

    async fn abort(&mut self) -> Result<(), HarnessError> {
        self.abort.set();
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
        // The runner reads no more: a turn that waits on it ends.
        self.rx.close();
        if let Some(play) = self.play.take() {
            let _ = play.await;
        }
        SessionExit {
            code: self.exit_code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StopReason;

    #[tokio::test]
    async fn a_session_without_a_role_answers_and_settles() {
        let harness = ScriptedHarness::default();
        let dir = tempfile::tempdir().unwrap();
        let mut session = harness
            .open(SessionSpec {
                label: "preflight".into(),
                cwd: dir.path().to_path_buf(),
                persist_dir: None,
                raw_log: None,
                thinking_log: None,
                stderr_log: None,
            })
            .await
            .unwrap();
        session.prompt("what is 17 * 23?").await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event, SessionEvent::Settled { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert!(
            matches!(&events[0], SessionEvent::MessageEnd { text, stop_reason: StopReason::Stop, .. } if text == "323")
        );
        assert_eq!(session.close(Duration::ZERO).await.code, Some(0));
    }

    /// A session of `harness` in `dir`, prompted as T001's first builder.
    async fn builder(harness: ScriptedHarness, dir: &std::path::Path) -> Box<dyn Session> {
        let mut session = harness
            .open(SessionSpec {
                label: "001-builder-T001".into(),
                cwd: dir.to_path_buf(),
                persist_dir: None,
                raw_log: None,
                thinking_log: None,
                stderr_log: None,
            })
            .await
            .unwrap();
        session
            .prompt("Role: BUILDER\nTask: T001\nAttempt: 1\n")
            .await
            .unwrap();
        session
    }

    /// Reads `session`'s events up to the first that `is`.
    async fn read_until(session: &mut Box<dyn Session>, is: impl Fn(&SessionEvent) -> bool) {
        while let Some(event) = session.next_event().await {
            if is(&event) {
                return;
            }
        }
    }

    /// A builder looping on one call ends when the runner closes the
    /// session, even without an abort.
    #[tokio::test(start_paused = true)]
    async fn closing_a_session_ends_a_turn_that_loops() {
        let dir = tempfile::tempdir().unwrap();
        let harness = ScriptedHarness {
            looping_task: Some("T001".into()),
            ..Default::default()
        };
        let mut session = builder(harness, dir.path()).await;
        read_until(&mut session, |e| matches!(e, SessionEvent::TurnEnd)).await;
        let closed = tokio::time::timeout(Duration::from_secs(60), session.close(Duration::ZERO));
        assert!(closed.await.is_ok(), "the looping turn did not end");
    }

    /// A session closed at the end of a reply does not run the reply's
    /// write, as an agent that was stopped would not.
    #[tokio::test(start_paused = true)]
    async fn a_write_after_the_session_is_closed_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let harness = ScriptedHarness {
            big_write_task: Some("T001".into()),
            ..Default::default()
        };
        let mut session = builder(harness, dir.path()).await;
        read_until(&mut session, |e| {
            matches!(e, SessionEvent::MessageEnd { .. })
        })
        .await;
        session.close(Duration::ZERO).await;
        assert!(!dir.path().join("src/T001.txt").exists());
    }
}
