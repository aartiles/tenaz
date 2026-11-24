//! Runs that pin an output limit (`maxOutputTokens`) that does not fit
//! their model's window (ADR 0041): refused at `init`, refused at `run`
//! after an edit of run.json, and a prompt with no room for a reply
//! blocked before its session.

use std::num::NonZeroU64;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{Blocked, deps, fake_project, no_session, run, try_fake_project, try_run};
use crate::orchestrator::{RunError, RunOptions};
use crate::project::Project;
use crate::testing::{ScriptedHarness, VirtualClock};

/// The fakes for a run of `project` with the scripted harness.
fn fakes(project: &Project, vcs: Arc<crate::testing::SnapshotVcs>) -> super::Deps {
    deps(
        project,
        vcs,
        Arc::new(ScriptedHarness::default()),
        Arc::new(VirtualClock::new()),
    )
}

/// A reply of 125000 tokens and the harness's 8192 leave no room for a
/// prompt in the fake model's 131072: `init` says so, and writes nothing.
#[test]
fn init_refuses_a_limit_that_leaves_no_room_for_a_prompt() {
    let work = tempfile::tempdir().unwrap();
    let refused = try_fake_project(work.path(), &["maxOutputTokens=125000"]).unwrap_err();
    let RunError::Precondition(message) = refused else {
        panic!("not refused: {refused:?}");
    };
    assert_eq!(
        message,
        "params.maxOutputTokens 125000 leaves no room in fake/fake's window of 131072 tokens \
         (bootstrap, builder, reviewer, split, replan, final-reviewer) for a prompt and the \
         harness's reserve of 8192 (params.harnessReserveTokens); set it below 122880"
    );
    assert!(!work.path().join("project/.tenaz/run.json").exists());
}

/// The same limit written into run.json after `init` stops `run` before
/// any session.
#[tokio::test(start_paused = true)]
async fn run_refuses_a_limit_edited_into_run_json_that_leaves_no_room() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let mut ledger = project.load_ledger().unwrap();
    ledger.params.max_output_tokens = NonZeroU64::new(125_000);
    project.save_ledger(&ledger).unwrap();
    let result = try_run(
        &project,
        fakes(&project, vcs),
        CancellationToken::new(),
        RunOptions::default(),
    )
    .await;
    assert!(
        matches!(&result, Err(RunError::Precondition(m))
            if m.starts_with("params.maxOutputTokens 125000 leaves no room in fake/fake's window")),
        "{result:?}"
    );
    assert!(no_session(&project));
}

/// A reply of 122000 tokens fits next to the harness's reserve, but not
/// next to a session at its budget, and leaves 9072 tokens for a prompt:
/// the run warns, then blocks before the first session, whose prompt is
/// larger, and says to lower the limit.
#[tokio::test(start_paused = true)]
async fn a_prompt_with_no_room_for_a_reply_of_the_limit_blocks_before_its_session() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxOutputTokens=122000"]);
    run(&project, fakes(&project, vcs), CancellationToken::new()).await;
    let blocked = Blocked::read(work, project);
    assert!(
        blocked.log.contains(
            "warning: params.maxOutputTokens 122000 does not fit in fake/fake's window of \
             131072 tokens"
        ),
        "{}",
        blocked.log
    );
    blocked.assert_reason_has(&[
        "prompt too large: the bootstrap prompt, ",
        "over the 9072 tokens a reply of params.maxOutputTokens 122000 leaves of the ",
        "To go on, lower params.maxOutputTokens in .tenaz/run.json and run with --resume",
    ]);
    assert!(no_session(&blocked.project), "{}", blocked.log);
}
