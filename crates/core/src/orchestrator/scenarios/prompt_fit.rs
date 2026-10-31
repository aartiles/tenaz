//! Runs whose prompts do not fit their role's model: the run stops before
//! the session, or at the harness's first refusal, instead of sending the
//! same prompt again; and a review diff sized for the reviewer (ADR 0012).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{Blocked, deps, fake_project, run, sessions};
use crate::ledger::{JournalRecord, Ledger, Phase};
use crate::project::Project;
use crate::role::Role;
use crate::role_models::RoleModel;
use crate::testing::{ScriptedHarness, VirtualClock};

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

/// [`play`], unchanged, that must block.
async fn blocked(harness: ScriptedHarness, overrides: &[&str]) -> Blocked {
    let (work, project, _) = play(harness, overrides, |_| {}).await;
    Blocked::read(work, project)
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
/// given back, as after a pause. Nothing was paused, so no continuation is
/// owed.
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
    let ledger = run.project.load_ledger().unwrap();
    assert!(!ledger.pending_continuation, "{}", run.log);
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
/// hold. A reviewer prompt over a low cap still runs: under the 8192
/// reserve alone, the cap is below every prompt's estimate.
#[tokio::test(start_paused = true)]
async fn a_prompt_over_a_lowered_cap_still_runs() {
    let (_work, project, log) = play(
        ScriptedHarness::default(),
        &["harnessReserveTokens=8192"],
        |ledger| {
            ledger.context_cap = Some(8000);
        },
    )
    .await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    let review = sessions(&project, Role::Reviewer)
        .next()
        .expect("T001 is reviewed");
    assert_eq!(review.context_budget, 8000, "{log}");
}
