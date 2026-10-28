//! The scripted run end to end: bootstrap with engine failures, the
//! capacity cap, a context stop and its continuation, every watchdog, a
//! review rejection, a split, a replan, out-of-role edits, a pause and
//! resume, and the final review. Each test plays the whole run and checks
//! one of them.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::ledger::{JournalRecord, SessionRecord, TaskOutcome, Verdict};
use crate::project::Project;
use crate::testing::{FastClock, ScriptedHarness};

/// A scripted run played to its end, and what it journaled.
struct Played {
    _work: tempfile::TempDir,
    project: Project,
    log: String,
    records: Vec<JournalRecord>,
}

impl Played {
    /// Plays the run with `harness`, two attempts per task and a replan
    /// every two tasks.
    async fn with(harness: ScriptedHarness) -> Self {
        Self::play(&["maxAttempts=2", "replanEvery=2"], harness).await
    }

    /// Plays the run with `harness` and the parameter `overrides`.
    async fn play(overrides: &[&str], harness: ScriptedHarness) -> Self {
        let work = tempfile::tempdir().unwrap();
        let (project, vcs) = fake_project(work.path(), overrides);
        let clock = Arc::new(FastClock::new(100));
        let deps = deps(&project, vcs, Arc::new(harness), clock);
        run(&project, deps, CancellationToken::new()).await;
        Self::read(work, project)
    }

    fn read(work: tempfile::TempDir, project: Project) -> Self {
        let log = project
            .read(".tenaz/private/orchestrator.log")
            .unwrap_or_default();
        let records = project.journal().into_iter().map(|e| e.record).collect();
        Self {
            _work: work,
            project,
            log,
            records,
        }
    }

    fn has(&self, test: impl Fn(&JournalRecord) -> bool) -> bool {
        self.records.iter().any(test)
    }

    fn sessions(&self) -> impl Iterator<Item = &SessionRecord> {
        self.records.iter().filter_map(|r| match r {
            JournalRecord::Session(s) => Some(s),
            _ => None,
        })
    }

    /// A session had a watchdog event whose kind starts with `kind`.
    fn watchdog(&self, kind: &str) -> bool {
        self.sessions()
            .any(|s| s.watchdog.iter().any(|e| e.kind.starts_with(kind)))
    }

    fn aborted_with(&self, text: &str) -> bool {
        self.sessions()
            .any(|s| s.abort_reason.as_deref().is_some_and(|r| r.contains(text)))
    }

    /// The prompts of the sessions labelled `…-<session>`, such as
    /// `builder-T001`.
    fn prompts(&self, session: &str) -> Vec<String> {
        let suffix = format!("-{session}.prompt.md");
        std::fs::read_dir(self.project.private_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(&suffix))
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect()
    }

    fn card_field(&self, id: &str, field: &str) -> Option<String> {
        card_field(&self.project, id, field)
    }

    /// The `attempt` of every verify of `task`, in order.
    fn verifies(&self, task: &str) -> Vec<u32> {
        self.records
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Verify {
                    task: t, attempt, ..
                } if t.as_str() == task => Some(*attempt),
                _ => None,
            })
            .collect()
    }
}

fn card_field(project: &Project, id: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}: ");
    project
        .read(&format!(".tenaz/tasks/{id}.md"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(&prefix).map(str::to_owned))
}

fn completed(record: &JournalRecord) -> bool {
    matches!(record, JournalRecord::Run { outcome, .. } if outcome == "COMPLETE")
}

/// The scripted run, with the recoveries it plays, reaches COMPLETE with
/// five tasks done and one split.
#[tokio::test]
async fn the_scripted_run_completes() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(run.has(completed), "no COMPLETE run record");
    let status = crate::orchestrator::status_text(&run.project);
    assert!(status.starts_with("Phase: COMPLETE\n"), "{status}");
    assert!(status.contains("Tasks: done 5, split 1"), "{status}");
}

#[tokio::test]
async fn bootstrap_engine_errors_are_retried_without_using_an_attempt() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(run.has(|r| matches!(r, JournalRecord::EngineRetry { .. })));
    assert!(
        run.has(|r| matches!(r, JournalRecord::BootstrapValidation { attempt: 1, .. })),
        "engine error used up a bootstrap attempt"
    );
}

#[tokio::test]
async fn a_capacity_error_lowers_the_budget_and_held_sessions_raise_it() {
    let run = Played::with(ScriptedHarness::default()).await;
    let lowered = |r: &JournalRecord| {
        matches!(
            r,
            JournalRecord::ContextCap {
                engine_max_context: 61488,
                context_cap: 49190,
                ..
            }
        )
    };
    // 85196 configured: halfway back from 49190 after four sessions held.
    let raised = |r: &JournalRecord| {
        matches!(
            r,
            JournalRecord::ContextCapRaised {
                context_cap: Some(67193),
                clean_sessions: 4,
                ..
            }
        )
    };
    assert!(
        run.has(lowered),
        "a capacity error did not lower the budget"
    );
    assert!(run.has(raised), "held sessions did not raise it back");
    let budgets: Vec<u64> = run.sessions().map(|s| s.context_budget).collect();
    assert!(
        budgets.contains(&49190),
        "lowered budget not applied: {budgets:?}"
    );
    assert!(
        budgets.contains(&67193),
        "raised budget not applied: {budgets:?}"
    );
}

#[tokio::test]
async fn a_session_past_its_budget_is_continued_without_the_map() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(run.aborted_with("context budget exceeded"));
    assert!(run.has(|r| matches!(
        r,
        JournalRecord::ContextContinuation {
            continuation: 1,
            ..
        }
    )));
    let prompts = run.prompts("builder-T001");
    let continued = prompts
        .iter()
        .find(|t| t.contains("## Continuation"))
        .expect("continuation note missing from the next prompt");
    assert!(!continued.contains("## Repository map"), "kept the map");
    assert!(prompts.iter().any(|t| t.contains("## Repository map")));
    let cut = |t: &String| t.contains("- R1: one") && !t.contains("- R2: two");
    assert!(prompts.iter().all(cut), "spec not cut to the card");
    let first_attempts = run
        .records
        .iter()
        .filter(|r| matches!(r, JournalRecord::Verify { task, attempt: 1, .. } if task.as_str() == "T001"))
        .count();
    assert_eq!(first_attempts, 1, "context stop counted as an attempt");
}

#[tokio::test]
async fn the_watchdogs_steer_and_then_abort() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(
        run.watchdog("steer:big-output-1"),
        "large output not steered"
    );
    assert!(run.watchdog("steer:loop-"), "loop was not steered");
    assert!(
        run.aborted_with("same bash call 5 times"),
        "loop not aborted"
    );
    assert!(
        run.watchdog("steer:repeat-"),
        "alternating loop not steered"
    );
    assert!(
        run.aborted_with("same result 6 times"),
        "alternating loop not aborted"
    );
}

#[tokio::test]
async fn reviews_without_a_verdict_are_retried_and_rejections_recorded() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(
        run.has(|r| matches!(r, JournalRecord::Review { task, attempt: 1, review: 2, verdict: Some(Verdict::Accept) } if task.as_str() == "T004")),
        "T004 review without a verdict was not retried"
    );
    assert!(
        run.has(|r| matches!(
            r,
            JournalRecord::Review {
                verdict: Some(Verdict::Changes),
                ..
            }
        )),
        "review rejection not recorded"
    );
    assert!(
        run.has(|r| matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Done, attempts: Some(1), stopped_sessions: Some(stopped), .. } if task.as_str() == "T004" && !stopped.is_empty())),
        "stopped T004 session not recorded on the task"
    );
}

#[tokio::test]
async fn a_task_that_keeps_failing_is_split_and_its_work_reverted() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(run.has(
        |r| matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Split, .. } if task.as_str() == "T003")
    ));
    assert_eq!(run.card_field("T003", "status").as_deref(), Some("split"));
    for id in ["T001", "T002", "T004", "T003a", "T003b"] {
        assert_eq!(
            run.card_field(id, "status").as_deref(),
            Some("done"),
            "{id}"
        );
    }
    assert!(
        !run.project.exists("checks/T003.exists.sh"),
        "failed T003 work was not reverted"
    );
}

#[tokio::test]
async fn the_backlog_is_replanned_and_the_brief_kept() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(run.has(|r| matches!(r, JournalRecord::ReplanAccepted { .. })));
    let reverted = run
        .sessions()
        .any(|s| s.reverted.iter().any(|p| p == ".tenaz/brief.md"));
    assert!(reverted, "brief tampering not reverted");
    assert_eq!(
        run.project.brief().unwrap(),
        "# Fake brief\n\nBuild four trivial files.\n"
    );
}

#[tokio::test]
async fn a_builder_without_its_check_is_asked_for_one() {
    let run = Played::with(ScriptedHarness {
        forget_checks_task: Some("T002".into()),
        ..Default::default()
    })
    .await;
    let asked = run
        .sessions()
        .find(|s| s.watchdog.iter().any(|e| e.kind == "own-checks"))
        .expect("a builder without its check was not asked for one");
    assert!(asked.label.ends_with("-builder-T002"), "{}", asked.label);
    let attempt = asked.attempt;
    assert!(
        run.has(|r| matches!(r, JournalRecord::Verify { task, attempt: a, task_ok: true, task_checks: 1.., .. } if task.as_str() == "T002" && Some(*a) == attempt)),
        "the check asked for was not there when the attempt was judged"
    );
    let log = &run.log;
    assert!(
        log.contains("no passing T002 check; asking for one"),
        "{log}"
    );
}

/// Ctrl-C during a builder session pauses the run without counting the
/// attempt, and the next run resumes it to the end.
#[tokio::test]
async fn a_pause_during_a_builder_is_resumed_without_counting_the_attempt() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=2", "replanEvery=2"]);
    let stop = CancellationToken::new();
    let stopping = ScriptedHarness {
        stop_at: Some(("T002".into(), stop.clone())),
        ..Default::default()
    };
    let clock = Arc::new(FastClock::new(100));
    let paused = deps(&project, vcs.clone(), Arc::new(stopping), clock.clone());
    run(&project, paused, stop).await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert!(
        log.contains("PAUSED during") && log.contains("builder-T002"),
        "{log}"
    );
    let attempts = card_field(&project, "T002", "attempts");
    assert_eq!(attempts.as_deref(), Some("0"), "paused attempt was counted");
    assert!(project.load_ledger().unwrap().paused_at.is_some());

    let harness = Arc::new(ScriptedHarness::default());
    run(
        &project,
        deps(&project, vcs, harness, clock),
        CancellationToken::new(),
    )
    .await;
    let run = Played::read(work, project);
    assert!(run.has(
        |r| matches!(r, JournalRecord::Paused { label: Some(l), .. } if l.ends_with("-builder-T002"))
    ));
    assert!(run.has(|r| matches!(r, JournalRecord::Resumed { .. })));
    // T002 loops on its first attempt in the script, so a second follows.
    let first = run.verifies("T002").first().copied();
    assert_eq!(first, Some(1), "the resumed attempt is not the first");
    assert!(run.has(completed), "{}", run.log);
}

/// Ctrl-C during the preflight pauses the run instead of failing it, and
/// the next run measures the engine again.
#[tokio::test]
async fn a_stop_during_the_preflight_pauses_the_run() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let clock = Arc::new(FastClock::new(100));
    let harness = || Arc::new(ScriptedHarness::default());
    let stop = CancellationToken::new();
    stop.cancel();
    run(
        &project,
        deps(&project, vcs.clone(), harness(), clock.clone()),
        stop,
    )
    .await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert!(log.contains("PAUSED during the preflight"), "{log}");
    let ledger = project.load_ledger().unwrap();
    assert!(ledger.paused_at.is_some());
    assert!(
        ledger.preflight.is_none(),
        "an interrupted preflight was saved"
    );
    assert_eq!(ledger.phase, crate::ledger::Phase::Bootstrap);

    run(
        &project,
        deps(&project, vcs, harness(), clock),
        CancellationToken::new(),
    )
    .await;
    let ledger = project.load_ledger().unwrap();
    assert!(ledger.preflight.is_some_and(|p| p.ok));
    assert_eq!(ledger.phase, crate::ledger::Phase::Complete);
}

/// Plays a run whose T003 always grows past its context stop.
async fn run_outgrowing(context_split: bool) -> Played {
    let split = format!("contextSplit={context_split}");
    let harness = ScriptedHarness {
        outgrow_task: Some("T003".into()),
        ..Default::default()
    };
    Played::play(&["contextContinuations=1", &split], harness).await
}

/// An attempt that uses up its context continuations and still fails is
/// split at once.
#[tokio::test]
async fn an_attempt_that_outgrows_its_context_is_split_at_once() {
    let run = run_outgrowing(true).await;
    let log = &run.log;
    assert!(run.has(completed), "{log}");
    assert_eq!(run.verifies("T003"), vec![1], "T003 was retried:\n{log}");
    assert!(log.contains("T003 outgrew its context budget"), "{log}");
    assert!(run.has(
        |r| matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Split, .. } if task.as_str() == "T003")
    ));
    let split_prompt = run.prompts("split-T003").concat();
    assert!(
        split_prompt.contains("outgrew its context budget"),
        "{split_prompt}"
    );
}

/// With `contextSplit=false` such an attempt is retried as before.
#[tokio::test]
async fn without_context_split_an_outgrown_attempt_is_retried() {
    let run = run_outgrowing(false).await;
    assert_eq!(run.verifies("T003"), vec![1, 2, 3]);
}
