//! The scripted run end to end: bootstrap with engine failures, the
//! capacity cap, a context stop and its continuation, every watchdog, a
//! review rejection, a split, a replan, out-of-role edits, a pause and
//! resume, and the final review. One test plays them all at once, as
//! `tenaz demo` does; each other plays the happy path with only what it
//! checks.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::cache_report::CacheReport;
use crate::ledger::{JournalRecord, SessionRecord, TaskOutcome, Verdict};
use crate::orchestrator::Deps;
use crate::project::Project;
use crate::testing::{ScriptedHarness, VirtualClock};

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
        let clock = Arc::new(VirtualClock::new());
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

/// The scripted run with every recovery at once, as `tenaz demo` plays it,
/// reaches COMPLETE with five tasks done and one split.
#[tokio::test(start_paused = true)]
async fn the_scripted_run_completes() {
    let run = Played::with(ScriptedHarness::every_recovery()).await;
    assert!(run.has(completed), "no COMPLETE run record");
    let status = crate::orchestrator::status_text(&run.project);
    assert!(status.starts_with("Phase: COMPLETE\n"), "{status}");
    assert!(status.contains("Tasks: done 5, split 1"), "{status}");
}

/// Without the engine proxy nobody saw whether the engine reports cached
/// tokens: `status --json` leaves it open and gives the share.
#[tokio::test(start_paused = true)]
async fn without_the_engine_proxy_the_cache_report_is_open() {
    let run = Played::with(ScriptedHarness::default()).await;
    let json = crate::orchestrator::status_json(&run.project);
    assert!(json["prompt_cache_reported"].is_null(), "{json}");
    assert!(json["prompt_cache_percent"].is_f64(), "{json}");
}

/// An engine that caches but leaves cached tokens out of its usage passes
/// the preflight with a warning, and status shows its cache as not
/// reported, not as 0 %.
#[tokio::test(start_paused = true)]
async fn a_run_on_an_engine_that_does_not_report_its_cache_completes() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=2", "replanEvery=2"]);
    let harness = ScriptedHarness {
        uncached: true,
        ..ScriptedHarness::default()
    };
    let report = CacheReport::new();
    report.note(false);
    let deps = Deps {
        cache_report: Some(report),
        ..deps(
            &project,
            vcs,
            Arc::new(harness),
            Arc::new(VirtualClock::new()),
        )
    };
    run(&project, deps, CancellationToken::new()).await;
    let run = Played::read(work, project);
    assert!(run.has(completed), "{}", run.log);
    assert!(
        run.log.contains("does not report cached prompt tokens"),
        "{}",
        run.log
    );
    let status = crate::orchestrator::status_text(&run.project);
    assert!(status.contains(" cache=not reported "), "{status}");
    assert!(status.contains("prompt cache not reported"), "{status}");
    let json = crate::orchestrator::status_json(&run.project);
    assert_eq!(json["prompt_cache_reported"], false, "{json}");
    assert!(json["prompt_cache_percent"].is_null(), "{json}");
}

/// The scripted harness on its own plays the happy path: four tasks
/// built and accepted on their first attempt, nothing to recover from.
#[tokio::test(start_paused = true)]
async fn the_happy_path_needs_no_recovery() {
    let run = Played::with(ScriptedHarness::default()).await;
    assert!(run.has(completed), "{}", run.log);
    let status = crate::orchestrator::status_text(&run.project);
    assert!(status.contains("Tasks: done 4\n"), "{status}");
    assert!(
        !run.has(|r| matches!(
            r,
            JournalRecord::EngineRetry { .. }
                | JournalRecord::ContextCap { .. }
                | JournalRecord::ContextContinuation { .. }
                | JournalRecord::Review {
                    verdict: Some(Verdict::Changes) | None,
                    ..
                }
        )),
        "{}",
        run.log
    );
    for task in ["T001", "T002", "T003", "T004"] {
        assert_eq!(run.verifies(task), vec![1], "{task}");
    }
    assert!(run.sessions().all(|s| s.ok && s.reverted.is_empty()));
}

#[tokio::test(start_paused = true)]
async fn bootstrap_engine_errors_are_retried_without_using_an_attempt() {
    let run = Played::with(ScriptedHarness {
        engine_failures: true,
        ..Default::default()
    })
    .await;
    assert!(run.has(|r| matches!(r, JournalRecord::EngineRetry { .. })));
    assert!(
        run.has(|r| matches!(r, JournalRecord::BootstrapValidation { attempt: 1, .. })),
        "engine error used up a bootstrap attempt"
    );
}

#[tokio::test(start_paused = true)]
async fn a_capacity_error_lowers_the_budget_and_held_sessions_raise_it() {
    let run = Played::with(ScriptedHarness {
        engine_failures: true,
        ..Default::default()
    })
    .await;
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

#[tokio::test(start_paused = true)]
async fn a_session_past_its_budget_is_continued_without_the_map() {
    let run = Played::with(ScriptedHarness {
        context_stop_task: Some("T001".into()),
        ..Default::default()
    })
    .await;
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
    assert!(
        run.log
            .contains("stopped at its context budget; a fresh session continues"),
        "{}",
        run.log
    );
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

/// A builder stopped at its second compaction ran out of context, like one
/// stopped at its budget: a fresh session continues the same attempt. In
/// the Digger 3D run of 2026-10-27 it was charged as a failed attempt.
#[tokio::test(start_paused = true)]
async fn a_session_stopped_at_its_second_compaction_is_continued() {
    let run = Played::with(ScriptedHarness {
        compact_task: Some("T002".into()),
        ..Default::default()
    })
    .await;
    assert!(run.aborted_with(crate::session::SECOND_COMPACTION));
    assert!(run.has(|r| matches!(
        r,
        JournalRecord::ContextContinuation { label, continuation: 1, .. }
            if label.ends_with("-builder-T002")
    )));
    assert_eq!(run.verifies("T002"), vec![1], "{}", run.log);
    let prompts = run.prompts("builder-T002");
    assert!(prompts.iter().any(|t| t.contains("## Continuation")));
    assert_eq!(run.card_field("T002", "attempts").as_deref(), Some("1"));
    assert!(
        run.log
            .contains("stopped at its second context compaction; a fresh session continues"),
        "{}",
        run.log
    );
    assert!(run.has(completed), "{}", run.log);
}

#[tokio::test(start_paused = true)]
async fn the_watchdogs_steer_and_then_abort() {
    let run = Played::with(ScriptedHarness {
        engine_failures: true,
        looping_task: Some("T002".into()),
        alternating_task: Some("T004".into()),
        ..Default::default()
    })
    .await;
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

#[tokio::test(start_paused = true)]
async fn reviews_without_a_verdict_are_retried_and_rejections_recorded() {
    let run = Played::with(ScriptedHarness {
        rejected_review: Some("T001".into()),
        no_verdict_review: Some("T004".into()),
        alternating_task: Some("T004".into()),
        // Continued after a context stop: not a stopped session.
        context_stop_task: Some("T001".into()),
        ..Default::default()
    })
    .await;
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
    // Its reviewer is told the builder was stopped; T001's is not.
    let note = "The orchestrator stopped the builder session";
    let reviews = run.prompts("reviewer-T004");
    assert_eq!(reviews.len(), 2, "T004 has a review and its retry");
    assert!(
        reviews.iter().all(|p| p.contains(note)),
        "T004's reviewer not told its builder was stopped"
    );
    assert!(!run.prompts("reviewer-T001").concat().contains(note));
}

#[tokio::test(start_paused = true)]
async fn a_task_that_keeps_failing_is_split_and_its_work_reverted() {
    let run = Played::with(ScriptedHarness {
        failing_task: Some("T003".into()),
        ..Default::default()
    })
    .await;
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

/// Every session that writes cards is told the builders' context budget:
/// the bootstrap's task-cards step, the split and the replan.
#[tokio::test(start_paused = true)]
async fn the_sessions_that_write_cards_are_told_the_builder_budget() {
    let run = Played::with(ScriptedHarness {
        stepwise_bootstrap: true,
        failing_task: Some("T003".into()),
        ..Default::default()
    })
    .await;
    // 65% of the fake model's 131,072-token window.
    let sizing = crate::briefing::card_sizing(85_196);
    let bootstraps = run.prompts("bootstrap");
    let (cards, others): (Vec<_>, Vec<_>) = bootstraps.iter().partition(|p| p.contains("Step: 3"));
    assert!(!cards.is_empty() && !others.is_empty(), "{}", run.log);
    let told = |p: &&String| p.contains(&sizing);
    assert!(cards.iter().all(told), "a task-cards step lacks the budget");
    assert!(!others.iter().any(told), "an earlier step has the budget");
    for planner in ["split-T003", "replan"] {
        let prompts = run.prompts(planner);
        assert!(!prompts.is_empty(), "no {planner} session:\n{}", run.log);
        assert!(
            prompts.iter().all(|p| told(&p)),
            "{planner} lacks the budget"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_backlog_is_replanned_and_the_brief_kept() {
    let run = Played::with(ScriptedHarness {
        tampered_brief: true,
        ..Default::default()
    })
    .await;
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

/// A replan and T001's builder both write T001's card, which is not theirs
/// to change: each edit is reverted and the card stays done.
#[tokio::test(start_paused = true)]
async fn out_of_role_card_edits_are_reverted() {
    let run = Played::with(ScriptedHarness {
        tampered_card: Some("T001".into()),
        ..Default::default()
    })
    .await;
    let reverted_by = |role: &str| {
        run.sessions().any(|s| {
            s.label.contains(role) && s.reverted.iter().any(|p| p == ".tenaz/tasks/T001.md")
        })
    };
    assert!(reverted_by("-builder-T001"), "{}", run.log);
    assert!(reverted_by("-replan"), "{}", run.log);
    assert_eq!(run.card_field("T001", "status").as_deref(), Some("done"));
    assert!(run.has(completed), "{}", run.log);
}

#[tokio::test(start_paused = true)]
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
#[tokio::test(start_paused = true)]
async fn a_pause_during_a_builder_is_resumed_without_counting_the_attempt() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=2", "replanEvery=2"]);
    let stop = CancellationToken::new();
    let stopping = ScriptedHarness {
        stop_at: Some(("T002".into(), stop.clone())),
        ..Default::default()
    };
    let clock = Arc::new(VirtualClock::new());
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
    let first = run.verifies("T002").first().copied();
    assert_eq!(first, Some(1), "the resumed attempt is not the first");
    assert!(run.has(completed), "{}", run.log);
}

/// Ctrl-C during the review of an attempt that passed `verify` pauses the
/// run; the next run reviews that same attempt again instead of charging
/// it and building another, as in the Digger 3D run of 2026-10-27. With
/// one attempt allowed, a charged pause would split the task.
#[tokio::test(start_paused = true)]
async fn a_pause_during_a_review_is_resumed_by_reviewing_the_same_attempt() {
    let run = resumed_after_a_review_pause(|_| {}).await;
    assert!(run.has(
        |r| matches!(r, JournalRecord::Paused { label: Some(l), .. } if l.ends_with("-reviewer-T002"))
    ));
    assert!(run.has(|r| matches!(r, JournalRecord::Resumed { .. })));
    assert!(run.has(completed), "{}", run.log);
    assert_eq!(run.card_field("T002", "attempts").as_deref(), Some("1"));
    let builders = run
        .sessions()
        .filter(|s| s.label.ends_with("-builder-T002"))
        .count();
    assert_eq!(builders, 1, "the paused review was charged:\n{}", run.log);
    assert_eq!(run.card_field("T002", "status").as_deref(), Some("done"));
    let reviews = run.prompts("reviewer-T002");
    assert_eq!(reviews.len(), 2, "{}", run.log);
    assert!(
        reviews.iter().all(|p| !p.contains("## Continuation")),
        "a review is redone, not continued:\n{}",
        run.log
    );
    assert!(run.has(|r| matches!(
        r,
        JournalRecord::Review { task, attempt: 1, verdict: Some(Verdict::Accept), .. }
            if task.as_str() == "T002"
    )));
}

/// After a pause in its review, the attempt's work no longer passes
/// `verify`: it fails once, and with no attempt left the task is split.
#[tokio::test(start_paused = true)]
async fn a_paused_review_whose_work_now_fails_is_not_judged_again_and_again() {
    let run = resumed_after_a_review_pause(|project| {
        std::fs::write(project.root().join("checks/T002.exists.sh"), "exit 1\n").unwrap();
    })
    .await;
    assert_eq!(run.verifies("T002"), vec![1, 1], "{}", run.log);
    assert_eq!(run.card_field("T002", "status").as_deref(), Some("split"));
}

/// Plays T002 with one attempt until Ctrl-C during its review, applies
/// `between` to the paused project, and plays the run again to its end.
async fn resumed_after_a_review_pause(between: impl FnOnce(&Project)) -> Played {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=1", "replanEvery=2"]);
    let stop = CancellationToken::new();
    let stopping = ScriptedHarness {
        stop_at_review: Some(("T002".into(), stop.clone())),
        ..Default::default()
    };
    let clock = Arc::new(VirtualClock::new());
    let paused = deps(&project, vcs.clone(), Arc::new(stopping), clock.clone());
    run(&project, paused, stop).await;
    assert!(project.load_ledger().unwrap().paused_at.is_some());
    assert_eq!(
        card_field(&project, "T002", "attempts").as_deref(),
        Some("1")
    );
    between(&project);
    let harness = Arc::new(ScriptedHarness::default());
    run(
        &project,
        deps(&project, vcs, harness, clock),
        CancellationToken::new(),
    )
    .await;
    Played::read(work, project)
}

/// Ctrl-C during the preflight pauses the run instead of failing it, and
/// the next run measures the engine again.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_preflight_pauses_the_run() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let clock = Arc::new(VirtualClock::new());
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
#[tokio::test(start_paused = true)]
async fn an_attempt_that_outgrows_its_context_is_split_at_once() {
    let run = run_outgrowing(true).await;
    let log = &run.log;
    assert!(run.has(completed), "{log}");
    assert_eq!(run.verifies("T003"), vec![1], "T003 was retried:\n{log}");
    assert!(log.contains("T003 ran out of context"), "{log}");
    assert!(run.has(
        |r| matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Split, .. } if task.as_str() == "T003")
    ));
    let split_prompt = run.prompts("split-T003").concat();
    assert!(
        split_prompt.contains("ran out of context"),
        "{split_prompt}"
    );
}

/// With `contextSplit=false` such an attempt is retried as before.
#[tokio::test(start_paused = true)]
async fn without_context_split_an_outgrown_attempt_is_retried() {
    let run = run_outgrowing(false).await;
    assert_eq!(run.verifies("T003"), vec![1, 2, 3]);
}
