//! A pause (Ctrl-C) during a long wait for the model still gives the
//! harness its grace period: the events the abort produces (the cut-off
//! message, the settle) reach the session's journal record. Seen with the
//! real Pi, which answers `abort` with an `aborted` message after the
//! runner had already given up on it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tenaz_adapters::git::GitRepo;
use tenaz_adapters::notify::SilentNotifier;
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::ledger::ModelConfig;
use tenaz_core::orchestrator::{Deps, InitOptions, RunOptions, Runner, Templates, init};
use tenaz_core::params::RunParams;
use tenaz_core::ports::{
    Harness, HarnessError, HarnessInfo, PermissionDecision, Session, SessionExit, SessionSpec,
};
use tenaz_core::project::Project;
use tenaz_core::session::{SessionEvent, StopReason, Usage};
use tenaz_core::testing::{FastClock, ScriptedHarness};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

#[tokio::test]
async fn a_pause_after_a_long_wait_keeps_the_aborted_turn() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild one trivial file.\n").unwrap();
    let project = Project::new(work.path().join("project"));
    let mut params = RunParams::default();
    params.apply_override("notify=false").unwrap();
    let config = ModelConfig {
        harness: "fake".into(),
        harness_version: "fake-pi 0.0.1".into(),
        model: "fake/fake".into(),
        provider: Some("fake".into()),
        id: "fake".into(),
        base_url: None,
        context_window: 131_072,
        input: vec!["text".into()],
        thinking: Some("medium".into()),
    };
    let templates =
        Templates::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates")).unwrap();
    let options = InitOptions {
        brief: Some(brief),
        config,
        params,
        skills: vec![],
        runner_version: "test".into(),
    };
    let clock = Arc::new(FastClock::new(100));
    init(
        &project,
        &GitRepo::new(project.root()),
        &*clock,
        &templates,
        options,
    )
    .unwrap();

    // Pause 200 ms (20 s on the test clock) into the silent bootstrap.
    let stop = CancellationToken::new();
    let watcher = {
        let (stop, log) = (
            stop.clone(),
            project.path(".agent/private/orchestrator.log"),
        );
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
    };
    let deps = Deps {
        harness: Arc::new(SilentBootstrap(ScriptedHarness::default())),
        vcs: Arc::new(GitRepo::new(project.root())),
        verify: Arc::new(VerifyProcess::new(project.root())),
        clock,
        notifier: Arc::new(SilentNotifier),
        engine: None,
        lease: None,
        engine_metrics: None,
        memory: None,
    };
    let mut runner = Runner::open(project.clone(), deps, stop).unwrap();
    runner
        .run(RunOptions {
            runner_version: "test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        watcher.await.unwrap(),
        "the bootstrap session never started"
    );

    let journal = project.read(".agent/journal.jsonl").unwrap();
    let session = journal
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|r| r["type"] == "session" && r["role"] == "bootstrap")
        .unwrap_or_else(|| panic!("no bootstrap session record:\n{journal}"));
    assert_eq!(session["last_stop_reason"], "aborted", "{session}");
    assert_eq!(session["turns"], 1, "{session}");
    assert!(journal.contains(r#""type":"paused""#), "{journal}");
}
