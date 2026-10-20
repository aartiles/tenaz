//! End-to-end: the whole loop against the scripted harness and a real
//! temporary git repository. It exercises bootstrap with engine failures,
//! the capacity cap, a context stop and its continuation, a pause and
//! resume, every watchdog, a review rejection, a split, a replan,
//! out-of-role edits, and the final review.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tenaz_adapters::git::GitRepo;
use tenaz_adapters::notify::SilentNotifier;
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::ledger::{JournalRecord, ModelConfig, Phase, TaskOutcome};
use tenaz_core::orchestrator::{Deps, InitOptions, RunOptions, Runner, Templates, init};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_core::testing::{FastClock, ScriptedHarness};
use tokio_util::sync::CancellationToken;

fn templates() -> Templates {
    Templates::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates")).unwrap()
}

fn deps(project: &Project, harness: ScriptedHarness) -> Deps {
    Deps {
        harness: Arc::new(harness),
        vcs: Arc::new(GitRepo::new(project.root())),
        verify: Arc::new(VerifyProcess::new(project.root())),
        clock: Arc::new(FastClock::new(100)),
        notifier: Arc::new(SilentNotifier),
        engine: None,
        lease: None,
        engine_metrics: None,
        memory: None,
    }
}

fn card_line(project: &Project, id: &str, prefix: &str) -> Option<String> {
    project
        .read(&format!(".tenaz/tasks/{id}.md"))
        .ok()?
        .lines()
        .find(|l| l.starts_with(prefix))
        .map(str::to_owned)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_fake_run_completes_with_every_recovery_path_exercised() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild four trivial files.\n").unwrap();
    let project = Project::new(work.path().join("project"));
    let mut params = RunParams::default();
    for pair in [
        "maxAttempts=2",
        "replanEvery=2",
        "engineRetryBaseSec=0",
        "notify=false",
    ] {
        params.apply_override(pair).unwrap();
    }
    let config = ModelConfig {
        harness: "fake".into(),
        harness_version: "fake-pi 0.0.1".into(),
        model: "fake/fake".into(),
        provider: Some("fake".into()),
        id: "fake".into(),
        base_url: None,
        context_window: 131072,
        input: vec!["text".into()],
        thinking: Some("medium".into()),
    };
    let vcs = GitRepo::new(project.root());
    let clock = FastClock::new(100);
    let options = InitOptions {
        launch: tenaz_core::ledger::Launch::default(),
        brief: Some(brief),
        config,
        params,
        skills: vec![],
        runner_version: "test".into(),
    };
    let ledger = init(&project, &vcs, &clock, &templates(), options).unwrap();
    assert_eq!(ledger.phase, Phase::Bootstrap);

    // First run: pause during the T002 builder session (Ctrl-C), then run again to the end.
    let slow = ScriptedHarness {
        slow_task: Some("T002".into()),
        slow_for: Duration::from_secs(3),
        ..Default::default()
    };
    let stop = CancellationToken::new();
    let watcher = {
        let (stop, log) = (
            stop.clone(),
            project.path(".tenaz/private/orchestrator.log"),
        );
        tokio::spawn(async move {
            for _ in 0..3000 {
                if std::fs::read_to_string(&log).is_ok_and(|t| t.contains("builder-T002 started")) {
                    stop.cancel();
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        })
    };
    let run_options = RunOptions {
        runner_version: "test".into(),
        ..Default::default()
    };
    let mut runner = Runner::open(project.clone(), deps(&project, slow), stop.clone()).unwrap();
    runner.run(run_options.clone()).await.unwrap();
    assert!(
        watcher.await.unwrap(),
        "the run never reached the T002 builder"
    );
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert!(log.contains("PAUSED during"), "run did not pause:\n{log}");
    assert_eq!(
        card_line(&project, "T002", "attempts:").as_deref(),
        Some("attempts: 0"),
        "paused attempt was counted"
    );
    assert!(project.load_ledger().unwrap().paused_at.is_some());

    let mut runner = Runner::open(
        project.clone(),
        deps(&project, ScriptedHarness::default()),
        CancellationToken::new(),
    )
    .unwrap();
    runner.run(run_options).await.unwrap();
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    let ledger = project.load_ledger().unwrap();
    assert_eq!(
        ledger.phase,
        Phase::Complete,
        "run did not complete:\n{log}"
    );

    let journal = project.journal();
    let records: Vec<&JournalRecord> = journal.iter().map(|e| &e.record).collect();
    let sessions: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            JournalRecord::Session(s) => Some(s),
            _ => None,
        })
        .collect();
    let has = |f: &dyn Fn(&JournalRecord) -> bool| records.iter().any(|r| f(r));
    let watchdog = |kind: &str| {
        sessions
            .iter()
            .any(|s| s.watchdog.iter().any(|e| e.kind == kind))
    };
    let aborted_with = |text: &str| {
        sessions
            .iter()
            .any(|s| s.abort_reason.as_deref().is_some_and(|r| r.contains(text)))
    };

    assert!(
        watchdog("steer:loop-")
            || sessions
                .iter()
                .any(|s| s.watchdog.iter().any(|e| e.kind.starts_with("steer:loop-"))),
        "loop was not steered"
    );
    assert!(
        has(&|r| matches!(r, JournalRecord::EngineRetry { .. })),
        "engine error was not retried"
    );
    assert!(
        has(&|r| matches!(r, JournalRecord::BootstrapValidation { attempt: 1, .. })),
        "engine error used up a bootstrap attempt"
    );
    assert!(
        has(&|r| matches!(
            r,
            JournalRecord::ContextCap {
                engine_max_context: 61488,
                context_cap: 49190,
                ..
            }
        )),
        "engine capacity error did not lower the context budget"
    );
    assert!(
        sessions.iter().any(|s| s.context_budget == 49190),
        "lowered context budget not applied to the next session"
    );
    assert!(
        aborted_with("context budget exceeded"),
        "session past 1.3x its context budget was not stopped"
    );
    assert!(
        has(&|r| matches!(
            r,
            JournalRecord::ContextContinuation {
                continuation: 1,
                ..
            }
        )),
        "context-stopped session was not continued"
    );
    let continued = std::fs::read_dir(project.private_dir())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with("-builder-T001.prompt.md")
                && std::fs::read_to_string(e.path()).is_ok_and(|t| t.contains("## Continuation"))
        });
    assert!(continued, "continuation note missing from the next prompt");
    let t001_first_verifies = records.iter().filter(|r| matches!(r, JournalRecord::Verify { task, attempt: 1, .. } if task.as_str() == "T001")).count();
    assert_eq!(t001_first_verifies, 1, "context stop counted as an attempt");
    assert!(
        has(
            &|r| matches!(r, JournalRecord::Paused { label: Some(l), .. } if l.ends_with("-builder-T002"))
        ),
        "pause not journaled"
    );
    assert!(
        has(&|r| matches!(r, JournalRecord::Resumed { .. })),
        "resume after pause not journaled"
    );
    assert!(
        watchdog("steer:big-output-1"),
        "large tool output was not steered"
    );
    assert!(
        aborted_with("same bash call 5 times"),
        "loop was not aborted"
    );
    assert!(
        sessions.iter().any(|s| s
            .watchdog
            .iter()
            .any(|e| e.kind.starts_with("steer:repeat-"))),
        "alternating loop was not steered"
    );
    assert!(
        aborted_with("same result 6 times"),
        "alternating loop was not aborted"
    );
    assert!(
        has(
            &|r| matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Done, attempts: Some(1), stopped_sessions: Some(stopped), .. } if task.as_str() == "T004" && !stopped.is_empty())
        ),
        "stopped T004 session not recorded on the task"
    );
    assert!(
        has(
            &|r| matches!(r, JournalRecord::Review { task, attempt: 1, review: 2, verdict: Some(v) } if task.as_str() == "T004" && v == "ACCEPT")
        ),
        "T004 review without a verdict was not retried"
    );
    assert!(
        has(&|r| matches!(r, JournalRecord::Review { verdict: Some(v), .. } if v == "CHANGES")),
        "review rejection not recorded"
    );
    assert!(
        has(
            &|r| matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Split, .. } if task.as_str() == "T003")
        ),
        "T003 was not split"
    );
    assert!(
        has(&|r| matches!(r, JournalRecord::ReplanAccepted { .. })),
        "no replan"
    );
    assert!(
        sessions
            .iter()
            .any(|s| s.reverted.iter().any(|p| p == ".tenaz/brief.md")),
        "brief tampering not reverted"
    );
    assert_eq!(
        project.brief().unwrap(),
        "# Fake brief\n\nBuild four trivial files.\n"
    );
    for id in ["T001", "T002", "T004", "T003a", "T003b"] {
        assert_eq!(
            card_line(&project, id, "status:").as_deref(),
            Some("status: done"),
            "{id} not done"
        );
    }
    assert_eq!(
        card_line(&project, "T003", "status:").as_deref(),
        Some("status: split")
    );
    assert!(
        !project.exists("checks/T003.exists.sh"),
        "failed T003 work was not reverted"
    );
    assert!(has(
        &|r| matches!(r, JournalRecord::Run { outcome, .. } if outcome == "COMPLETE")
    ));
    let status = tenaz_core::orchestrator::status_text(&project);
    assert!(status.starts_with("Phase: COMPLETE\n"), "{status}");
    assert!(status.contains("Tasks: done 5, split 1"), "{status}");
}

/// Ctrl-C during the preflight pauses the run instead of failing it, and
/// the next run measures the engine again.
#[tokio::test]
async fn a_stop_during_the_preflight_pauses_the_run() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild four trivial files.\n").unwrap();
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
        context_window: 131072,
        input: vec!["text".into()],
        thinking: Some("medium".into()),
    };
    let options = InitOptions {
        launch: tenaz_core::ledger::Launch::default(),
        brief: Some(brief),
        config,
        params,
        skills: vec![],
        runner_version: "test".into(),
    };
    init(
        &project,
        &GitRepo::new(project.root()),
        &FastClock::new(100),
        &templates(),
        options,
    )
    .unwrap();
    let run_options = RunOptions {
        runner_version: "test".into(),
        ..Default::default()
    };

    let stop = CancellationToken::new();
    stop.cancel();
    let mut runner = Runner::open(
        project.clone(),
        deps(&project, ScriptedHarness::default()),
        stop,
    )
    .unwrap();
    runner.run(run_options.clone()).await.unwrap();
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert!(log.contains("PAUSED during the preflight"), "{log}");
    let ledger = project.load_ledger().unwrap();
    assert!(ledger.paused_at.is_some());
    assert!(
        ledger.preflight.is_none(),
        "an interrupted preflight was saved"
    );
    assert_eq!(ledger.phase, Phase::Bootstrap);

    let mut runner = Runner::open(
        project.clone(),
        deps(&project, ScriptedHarness::default()),
        CancellationToken::new(),
    )
    .unwrap();
    runner.run(run_options).await.unwrap();
    let ledger = project.load_ledger().unwrap();
    assert!(ledger.preflight.is_some_and(|p| p.ok));
    assert_eq!(ledger.phase, Phase::Complete);
}
