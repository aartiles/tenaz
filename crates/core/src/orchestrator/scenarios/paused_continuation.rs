//! A builder session paused with Ctrl-C is continued on the next run. Its
//! continuation is owed until a session that continues it ends: its
//! reruns are told they continue it too (the prompt's `## Continuation`),
//! and a run that blocks first continues it after `--resume`.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{Blocked, deps, fake_project, resume, run, sessions};
use crate::ledger::{JournalRecord, Ledger, Phase};
use crate::params::RunParams;
use crate::role::Role;
use crate::testing::{ScriptedHarness, SnapshotVcs, VirtualClock};

/// A run blocked after T001's first builder session was paused, and what
/// it takes to run it again.
struct BlockedAfterPause {
    run: Blocked,
    /// How many builder sessions T001 had when the run blocked.
    builders_blocked: usize,
    vcs: Arc<SnapshotVcs>,
    clock: Arc<VirtualClock>,
}

/// Pauses T001's first builder session, changes the ledger with `edit`,
/// then runs again with `harness`, which must block.
async fn blocked_after_pause(
    harness: ScriptedHarness,
    edit: impl FnOnce(&mut Ledger),
) -> BlockedAfterPause {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=2"]);
    let stop = CancellationToken::new();
    let stopping = ScriptedHarness {
        stop_at: Some(("T001".into(), stop.clone())),
        ..Default::default()
    };
    let clock = Arc::new(VirtualClock::new());
    run(
        &project,
        deps(&project, vcs.clone(), Arc::new(stopping), clock.clone()),
        stop,
    )
    .await;
    let mut ledger = project.load_ledger().unwrap();
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert!(ledger.pending_continuation, "T001 was not paused:\n{log}");
    edit(&mut ledger);
    project.save_ledger(&ledger).unwrap();

    run(
        &project,
        deps(&project, vcs.clone(), Arc::new(harness), clock.clone()),
        CancellationToken::new(),
    )
    .await;
    let run = Blocked::read(work, project);
    BlockedAfterPause {
        builders_blocked: run.sessions(Role::Builder),
        run,
        vcs,
        clock,
    }
}

impl BlockedAfterPause {
    /// The block still owes T001's paused builder its continuation: every
    /// builder session since the pause was told it continues it, and the
    /// attempt is not charged.
    fn assert_still_owed(&self) {
        let run = &self.run;
        let ledger = run.project.load_ledger().unwrap();
        assert!(
            ledger.pending_continuation,
            "the block dropped the continuation owed:\n{}",
            run.log
        );
        for label in self.builders().iter().skip(1) {
            self.assert_note(label, true, &run.log);
        }
        let card = run.project.read(".tenaz/tasks/T001.md").unwrap();
        assert!(card.contains("attempts: 0"), "{card}");
    }

    /// Changes the ledger with `fix`, then runs again with `--resume`.
    async fn fix_and_resume(&self, fix: impl FnOnce(&mut Ledger)) {
        let project = &self.run.project;
        let mut ledger = project.load_ledger().unwrap();
        fix(&mut ledger);
        project.save_ledger(&ledger).unwrap();
        let harness = Arc::new(ScriptedHarness::default());
        resume(
            project,
            deps(project, self.vcs.clone(), harness, self.clock.clone()),
        )
        .await;
    }

    /// After [`Self::fix_and_resume`], the run ended, one builder session continued
    /// the paused work, and the reviewer that followed was not told it
    /// continues anything.
    fn assert_continued(&self) {
        let project = &self.run.project;
        let log = project.read(".tenaz/private/orchestrator.log").unwrap();
        let ledger = project.load_ledger().unwrap();
        assert_eq!(ledger.phase, Phase::Complete, "{log}");
        assert!(
            !ledger.pending_continuation,
            "the continuation was not settled:\n{log}"
        );
        let builders = self.builders();
        assert_eq!(builders.len(), self.builders_blocked + 1, "{log}");
        self.assert_note(&builders[self.builders_blocked], true, &log);
        for review in sessions(project, Role::Reviewer) {
            self.assert_note(&review.label, false, &log);
        }
    }

    /// The labels of T001's builder sessions, in order.
    fn builders(&self) -> Vec<String> {
        sessions(&self.run.project, Role::Builder)
            .map(|s| s.label)
            .collect()
    }

    /// Asserts whether the session `label` was told it continues earlier
    /// work; `log` is the run's log, shown if not.
    fn assert_note(&self, label: &str, continues: bool, log: &str) {
        let prompt = self
            .run
            .project
            .read(&format!(".tenaz/private/{label}.prompt.md"))
            .unwrap();
        assert_eq!(
            prompt.contains("## Continuation"),
            continues,
            "{label}:\n{log}"
        );
    }
}

/// A builder session paused mid-turn is continued on the next run. If its
/// prompt is then over the budget, the run blocks before the session,
/// still owes the continuation and does not charge the attempt, so
/// `--resume` after the fix continues the work.
#[tokio::test(start_paused = true)]
async fn a_blocked_continuation_is_still_owed() {
    let blocked = blocked_after_pause(ScriptedHarness::default(), |ledger| {
        ledger.params.harness_reserve_tokens = 1_000_000;
    })
    .await;
    let run = &blocked.run;
    assert!(
        run.reason
            .starts_with("prompt too large: the builder prompt for T001, "),
        "{}",
        run.reason
    );
    assert_eq!(run.sessions(Role::Builder), 1, "{}", run.log);
    blocked.assert_still_owed();
    blocked
        .fix_and_resume(|ledger| {
            ledger.params.harness_reserve_tokens = RunParams::default().harness_reserve_tokens;
        })
        .await;
    blocked.assert_continued();
}

/// A paused builder session whose continuation the harness then refuses:
/// the run blocks, still owes the continuation, and `--resume` continues
/// the work.
#[tokio::test(start_paused = true)]
async fn a_continuation_the_harness_refuses_is_still_owed() {
    let harness = ScriptedHarness {
        oversize_task: Some("T001".into()),
        ..Default::default()
    };
    let blocked = blocked_after_pause(harness, |_| {}).await;
    let run = &blocked.run;
    run.assert_reason_has(&["the harness refused the builder prompt of "]);
    assert_eq!(run.sessions(Role::Builder), 2, "{}", run.log);
    blocked.assert_still_owed();
    blocked.fix_and_resume(|_| {}).await;
    blocked.assert_continued();
}

/// A paused builder session whose continuation the engine keeps failing
/// until its retries run out: here the harness crashes, then the engine
/// runs out of memory in the one retry allowed (two causes, as each fails
/// only the first session it names). The retry is told it continues the
/// paused work too, the run blocks still owing the continuation, and
/// `--resume` continues the work.
#[tokio::test(start_paused = true)]
async fn a_continuation_the_engine_gives_up_on_is_still_owed() {
    let harness = ScriptedHarness {
        crash_task: Some("T001".into()),
        out_of_memory_task: Some("T001".into()),
        ..Default::default()
    };
    let blocked = blocked_after_pause(harness, |ledger| ledger.params.engine_retries = 1).await;
    let run = &blocked.run;
    assert!(
        run.reason.starts_with("engine unavailable: "),
        "{}",
        run.reason
    );
    assert_eq!(run.sessions(Role::Builder), 3, "{}", run.log);
    assert!(run.retried(), "{}", run.log);
    blocked.assert_still_owed();
    blocked.fix_and_resume(|_| {}).await;
    blocked.assert_continued();
}

/// A paused builder session whose continuation the engine keeps refusing
/// as over the context it can hold, until the capacity retries run out:
/// the rerun at the lowered budget is told it continues the paused work
/// too, the run blocks still owing the continuation, and `--resume`
/// continues the work.
#[tokio::test(start_paused = true)]
async fn a_continuation_the_engine_has_no_capacity_for_is_still_owed() {
    let harness = ScriptedHarness {
        over_capacity_task: Some("T001".into()),
        ..Default::default()
    };
    let blocked = blocked_after_pause(harness, |ledger| ledger.params.capacity_retries = 1).await;
    let run = &blocked.run;
    run.assert_reason_has(&["engine unavailable: sessions still overflow"]);
    assert_eq!(run.sessions(Role::Builder), 3, "{}", run.log);
    let lowered = run
        .project
        .journal()
        .iter()
        .any(|e| matches!(e.record, JournalRecord::ContextCap { .. }));
    assert!(lowered, "{}", run.log);
    blocked.assert_still_owed();
    blocked.fix_and_resume(|_| {}).await;
    blocked.assert_continued();
}
