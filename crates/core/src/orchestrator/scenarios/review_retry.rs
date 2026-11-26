//! A review that ends without a verdict is retried once, and the retry is
//! told how the first ended. When the retry gives none either, the task is
//! accepted on `verify` alone, and its done record and the final reviewer
//! say so. In the Last Delivery run of 2026-11-26 the
//! reviewer of T046 crashed fx on `cat -et`, and its retry, given the same
//! briefing, made the same call.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project};
use crate::ledger::{JournalRecord, Phase, TaskOutcome, Verdict};
use crate::project::Project;
use crate::testing::{ScriptedHarness, VirtualClock};

/// The command the scripted reviewers crash on.
const CAT_ET: &str = "sed -n '1,3p' checks/T001.check | cat -et";

/// Runs the fake goal to the end with `harness`; the project.
async fn run(harness: ScriptedHarness) -> (tempfile::TempDir, Project) {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let deps = deps(
        &project,
        vcs,
        Arc::new(harness),
        Arc::new(VirtualClock::new()),
    );
    super::run(&project, deps, CancellationToken::new()).await;
    let log = project.read_private("orchestrator.log").unwrap();
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    (work, project)
}

/// The briefings of `task`'s reviewers, in order.
fn review_prompts(project: &Project, task: &str) -> Vec<String> {
    prompts(project, &format!("-reviewer-{task}"))
}

/// The briefings of the sessions whose label ends in `suffix`, in order.
fn prompts(project: &Project, suffix: &str) -> Vec<String> {
    project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with(suffix) => Some(
                project
                    .read_private(&format!("{}.prompt.md", s.label))
                    .unwrap(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_review_retried_after_a_crash_is_told_why_the_first_ended() {
    let (_work, project) = run(ScriptedHarness {
        crashed_reviews: Some(("T001".into(), 1)),
        crash_in_tool_command: Some(CAT_ET.into()),
        ..Default::default()
    })
    .await;
    let reviews = review_prompts(&project, "T001");
    assert_eq!(reviews.len(), 2, "a review and its retry");
    assert!(!reviews[0].contains("## Earlier review"), "{}", reviews[0]);
    let retry = &reviews[1];
    assert!(retry.contains("## Earlier review"), "{retry}");
    assert!(retry.contains("cat -et"), "{retry}");
    assert!(retry.contains("`od -c` or `grep -n`"), "{retry}");
    let records: Vec<_> = project.journal().into_iter().map(|e| e.record).collect();
    assert!(
        records.iter().any(|r| matches!(r, JournalRecord::Review { task, review: 2, verdict: Some(Verdict::Accept), .. } if task.as_str() == "T001")),
        "the retry accepts T001"
    );
    assert!(
        records.iter().any(|r| matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Done, attempts: Some(1), .. } if task.as_str() == "T001")),
        "a crashed review costs the task no attempt"
    );
    let finals = prompts(&project, "-final-reviewer");
    let first = finals.first().expect("a final review");
    assert!(!first.contains("## Tasks accepted unreviewed"), "{first}");
    assert!(project.load_ledger().unwrap().unreviewed.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_task_whose_reviews_both_crash_is_accepted_unreviewed_and_the_final_reviewer_told() {
    let (_work, project) = run(ScriptedHarness {
        crashed_reviews: Some(("T001".into(), 2)),
        crash_in_tool_command: Some(CAT_ET.into()),
        ..Default::default()
    })
    .await;
    let why = project
        .journal()
        .into_iter()
        .find_map(|e| match e.record {
            JournalRecord::Task {
                task,
                outcome: TaskOutcome::Done,
                reviewed: Some(false),
                unreviewed,
                ..
            } if task.as_str() == "T001" => unreviewed,
            _ => None,
        })
        .expect("T001 is done, unreviewed, with why");
    assert!(
        why.starts_with("no reviewer gave a verdict (review 1: "),
        "{why}"
    );
    assert!(why.contains("; review 2: "), "{why}");
    assert!(why.contains("cat -et"), "{why}");
    let kept = project.load_ledger().unwrap().unreviewed;
    assert_eq!(
        kept.keys()
            .map(crate::task::TaskId::as_str)
            .collect::<Vec<_>>(),
        ["T001"]
    );
    let finals = prompts(&project, "-final-reviewer");
    let last = finals.last().expect("a final review");
    assert!(last.contains("## Tasks accepted unreviewed"), "{last}");
    assert!(
        last.contains("- T001: no reviewer gave a verdict"),
        "{last}"
    );
}
