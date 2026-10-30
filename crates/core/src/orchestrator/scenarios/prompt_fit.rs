//! Runs whose prompts do not fit their role's model: the run stops at the
//! first refusal instead of sending the same prompt again (ADR 0012).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::ledger::{JournalRecord, Phase};
use crate::project::Project;
use crate::role::Role;
use crate::role_models::RoleModel;
use crate::testing::{ScriptedHarness, VirtualClock};

/// A run blocked by a refused prompt: the project, its log and why.
struct Blocked {
    _work: tempfile::TempDir,
    project: Project,
    log: String,
    reason: String,
}

/// Plays the fake goal with `harness`, the reviewer on a model of its own
/// with a smaller window than the run's, as in the Digger 3D run. It must
/// block.
async fn blocked(harness: ScriptedHarness) -> Blocked {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=2"]);
    let mut ledger = project.load_ledger().unwrap();
    ledger.config.role_models.insert(
        Role::Reviewer,
        RoleModel {
            model: "fake/reviewer".into(),
            id: "reviewer".into(),
            context_window: 65_536,
            input: vec!["text".into()],
        },
    );
    project.save_ledger(&ledger).unwrap();
    let deps = deps(
        &project,
        vcs,
        Arc::new(harness),
        Arc::new(VirtualClock::new()),
    );
    run(&project, deps, CancellationToken::new()).await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    let ledger = project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Blocked, "{log}");
    let reason = ledger.blocked_reason.unwrap_or_default();
    Blocked {
        _work: work,
        project,
        log,
        reason,
    }
}

impl Blocked {
    /// The journaled sessions whose label ends with `suffix`.
    fn sessions(&self, suffix: &str) -> usize {
        self.project
            .journal()
            .iter()
            .filter(|e| matches!(&e.record, JournalRecord::Session(s) if s.label.ends_with(suffix)))
            .count()
    }

    fn retried(&self) -> bool {
        self.project
            .journal()
            .iter()
            .any(|e| matches!(e.record, JournalRecord::EngineRetry { .. }))
    }

    fn assert_reason_has(&self, parts: &[&str]) {
        for part in parts {
            assert!(
                self.reason.contains(part),
                "{part:?} not in {}",
                self.reason
            );
        }
    }
}

/// In the Digger 3D run of 2026-10-29 fx refused T001's review seven
/// times, about 45 minutes, before the run blocked as "engine
/// unavailable". One refusal now blocks it, and names the reviewer's model.
#[tokio::test(start_paused = true)]
async fn a_review_the_harness_refuses_blocks_the_run_at_once() {
    let run = blocked(ScriptedHarness {
        oversize_review: Some("T001".into()),
        ..Default::default()
    })
    .await;
    run.assert_reason_has(&[
        "prompt too large: the harness refused the reviewer prompt of ",
        "-reviewer-T001, ",
        " characters; fake/reviewer's window is 65536 tokens",
        "lower params.diffLimitBytes in .tenaz/run.json and run with --resume",
    ]);
    assert_eq!(run.sessions("-reviewer-T001"), 1, "{}", run.log);
    assert!(!run.retried(), "{}", run.log);
}

/// A builder's prompt refused is not the task's failure: the attempt is
/// given back, as after a pause.
#[tokio::test(start_paused = true)]
async fn a_builder_the_harness_refuses_is_not_charged_the_attempt() {
    let run = blocked(ScriptedHarness {
        oversize_task: Some("T001".into()),
        ..Default::default()
    })
    .await;
    run.assert_reason_has(&[
        "the harness refused the builder prompt of ",
        "fake/fake's window is 131072 tokens",
    ]);
    assert!(!run.reason.contains("--resume"), "{}", run.reason);
    assert_eq!(run.sessions("-builder-T001"), 1, "{}", run.log);
    assert!(!run.retried(), "{}", run.log);
    let card = run.project.read(".tenaz/tasks/T001.md").unwrap();
    assert!(card.contains("attempts: 0"), "{card}");
}
