//! Runs whose prompts do not fit their role's model: the run stops before
//! the session, or at the harness's first refusal, instead of sending the
//! same prompt again; and a review diff sized for the reviewer (ADR 0012).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::ledger::{JournalRecord, Ledger, Phase, SessionRecord};
use crate::params::RunParams;
use crate::project::Project;
use crate::role::Role;
use crate::role_models::RoleModel;
use crate::testing::{ScriptedHarness, VirtualClock};
use crate::util::CODE_BYTES_PER_TOKEN;

/// A run blocked by a refused prompt: the project, its log and why.
struct Blocked {
    _work: tempfile::TempDir,
    project: Project,
    log: String,
    reason: String,
}

/// The reviewer's own model: a smaller window than the run's 131072, as in
/// the Digger 3D run.
const REVIEWER_WINDOW: u64 = 65_536;

/// Plays the fake goal with `harness` and `overrides`, the reviewer on a
/// model of its own and the ledger changed by `edit` before the run; the
/// project and its log.
async fn play(
    harness: ScriptedHarness,
    overrides: &[&str],
    edit: impl FnOnce(&mut Ledger),
) -> (tempfile::TempDir, Project, String) {
    let work = tempfile::tempdir().unwrap();
    let mut set = vec!["maxAttempts=2"];
    set.extend_from_slice(overrides);
    let (project, vcs) = fake_project(work.path(), &set);
    let mut ledger = project.load_ledger().unwrap();
    edit(&mut ledger);
    ledger.config.role_models.insert(
        Role::Reviewer,
        RoleModel {
            model: "fake/reviewer".into(),
            id: "reviewer".into(),
            context_window: REVIEWER_WINDOW,
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
    (work, project, log)
}

/// The journaled sessions of `role` for T001.
fn sessions(project: &Project, role: Role) -> impl Iterator<Item = SessionRecord> {
    project
        .journal()
        .into_iter()
        .filter_map(move |e| match e.record {
            JournalRecord::Session(s)
                if s.role == role && s.task.as_ref().is_some_and(|t| t.as_str() == "T001") =>
            {
                Some(s)
            }
            _ => None,
        })
}

/// [`play`], unchanged, that must block.
async fn blocked(harness: ScriptedHarness, overrides: &[&str]) -> Blocked {
    let (work, project, log) = play(harness, overrides, |_| {}).await;
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
    /// How many sessions of `role` T001 had.
    fn sessions(&self, role: Role) -> usize {
        sessions(&self.project, role).count()
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
    let run = blocked(
        ScriptedHarness {
            oversize_review: Some("T001".into()),
            ..Default::default()
        },
        &[],
    )
    .await;
    run.assert_reason_has(&[
        "prompt too large: the harness refused the reviewer prompt of ",
        "-reviewer-T001, ",
        " bytes; fake/reviewer's window is 65536 tokens",
        "lower params.diffShare or params.diffLimitBytes in .tenaz/run.json and run with --resume",
    ]);
    assert_eq!(run.sessions(Role::Reviewer), 1, "{}", run.log);
    assert!(!run.retried(), "{}", run.log);
}

/// A builder's prompt refused is not the task's failure: the attempt is
/// given back, as after a pause.
#[tokio::test(start_paused = true)]
async fn a_builder_the_harness_refuses_is_not_charged_the_attempt() {
    let run = blocked(
        ScriptedHarness {
            oversize_task: Some("T001".into()),
            ..Default::default()
        },
        &[],
    )
    .await;
    run.assert_reason_has(&[
        "the harness refused the builder prompt of ",
        "fake/fake's window is 131072 tokens",
    ]);
    assert!(!run.reason.contains("--resume"), "{}", run.reason);
    assert_eq!(run.sessions(Role::Builder), 1, "{}", run.log);
    assert!(!run.retried(), "{}", run.log);
    let card = run.project.read(".tenaz/tasks/T001.md").unwrap();
    assert!(card.contains("attempts: 0"), "{card}");
}

/// In the Digger 3D run the reviewer's first request was 30245 of its 42598
/// tokens, the diff half of the prompt, cut at one `diffLimitBytes` for
/// every role. The diff now takes `diffShare` of the reviewer's own budget:
/// here 0.0005 × 42598 tokens × 3 bytes = 63, where the builder's budget
/// would give 127.
#[tokio::test(start_paused = true)]
async fn a_review_diff_takes_its_share_of_the_reviewer_budget() {
    let (_work, project, log) =
        play(ScriptedHarness::default(), &["diffShare=0.0005"], |_| {}).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    let label = project
        .journal()
        .into_iter()
        .find_map(|e| match e.record {
            JournalRecord::Session(s) if s.label.ends_with("-reviewer-T001") => Some(s.label),
            _ => None,
        })
        .expect("T001 is reviewed");
    let prompt = project
        .read(&format!(".tenaz/private/{label}.prompt.md"))
        .unwrap();
    let after_note = prompt
        .split_once("clipped. Read the changed files directly.]\n\n")
        .expect("the diff is clipped")
        .1;
    let shown = after_note
        .split_once("\n```")
        .expect("the diff block ends")
        .0;
    assert_eq!(shown.len(), 63, "{prompt}");
}

/// A prompt estimated over its role's budget, with the harness's reserve,
/// stops the run before the session opens: here a reserve that leaves the
/// reviewer's 42598 tokens no room, while the builder's 85196 still fit.
#[tokio::test(start_paused = true)]
async fn a_prompt_over_its_budget_blocks_the_run_before_the_session() {
    let run = blocked(ScriptedHarness::default(), &["harnessReserveTokens=42000"]).await;
    run.assert_reason_has(&[
        "prompt too large: the reviewer prompt for T001, ",
        " tokens with the harness's reserve, over the reviewer's budget of 42598; ",
        "fake/reviewer's window is 65536 tokens",
        "lower params.diffShare",
    ]);
    assert_eq!(run.sessions(Role::Builder), 1, "{}", run.log);
    assert_eq!(run.sessions(Role::Reviewer), 0, "{}", run.log);
}

/// The cap the engine set after it ran out of memory is not what a prompt
/// must fit: it is raised again, and the engine refuses what it cannot
/// hold. A reviewer prompt over a low cap still runs.
#[tokio::test(start_paused = true)]
async fn a_prompt_over_a_lowered_cap_still_runs() {
    let (_work, project, log) = play(ScriptedHarness::default(), &[], |ledger| {
        ledger.context_cap = Some(9000);
    })
    .await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    let review = sessions(&project, Role::Reviewer)
        .next()
        .expect("T001 is reviewed");
    assert_eq!(review.context_budget, 9000, "{log}");
    // The check's estimate, as `first_request_tokens` makes it.
    let estimate = review.prompt_chars.div_ceil(CODE_BYTES_PER_TOKEN) as u64
        + RunParams::default().harness_reserve_tokens;
    assert!(estimate > 9000, "{estimate}");
}

/// A session paused mid-turn is continued on the next run. If its prompt
/// is then over the budget, the run blocks and still owes the
/// continuation, so `--resume` after the fix continues the work.
#[tokio::test(start_paused = true)]
async fn a_blocked_continuation_is_still_owed() {
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
    assert!(ledger.pending_continuation);
    ledger.params.harness_reserve_tokens = 1_000_000;
    project.save_ledger(&ledger).unwrap();

    let harness = Arc::new(ScriptedHarness::default());
    run(
        &project,
        deps(&project, vcs, harness, clock),
        CancellationToken::new(),
    )
    .await;
    let ledger = project.load_ledger().unwrap();
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert_eq!(ledger.phase, Phase::Blocked, "{log}");
    let reason = ledger.blocked_reason.unwrap_or_default();
    assert!(
        reason.starts_with("prompt too large: the builder prompt for T001, "),
        "{reason}"
    );
    assert!(ledger.pending_continuation, "{log}");
    let builders = sessions(&project, Role::Builder).count();
    assert_eq!(builders, 1, "{log}");
}
