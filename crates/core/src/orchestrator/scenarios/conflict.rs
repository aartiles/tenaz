//! A builder that declares its task cannot pass without changing a check
//! of a done task (ADR 0009): the task is blocked at once, a replan adds a
//! card that amends the check, and its builder and reviewer are told which
//! checks it may change. A declaration that does not stand is an ordinary
//! failed attempt whose next builder is told why.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::ledger::{JournalRecord, Phase};
use crate::project::Project;
use crate::task::TaskId;
use crate::testing::{ScriptedHarness, VirtualClock};

/// Plays the fake goal with T003's first builder declaring a conflict with
/// `check`; the project, its log and the work directory.
async fn declaring(check: &str) -> (tempfile::TempDir, Project, String) {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=3"]);
    let harness = ScriptedHarness {
        conflict: Some(("T003".into(), check.into())),
        ..Default::default()
    };
    let clock = Arc::new(VirtualClock::new());
    run(
        &project,
        deps(&project, vcs, Arc::new(harness), clock),
        CancellationToken::new(),
    )
    .await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    (work, project, log)
}

fn prompts(project: &Project, session: &str) -> String {
    let suffix = format!("-{session}.prompt.md");
    std::fs::read_dir(project.private_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(&suffix))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect()
}

fn status(project: &Project, id: &str) -> String {
    let card = project.read(&format!(".tenaz/tasks/{id}.md")).unwrap();
    card.lines()
        .find_map(|l| l.strip_prefix("status: "))
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(start_paused = true)]
async fn a_declared_conflict_blocks_the_task_and_a_replan_amends_the_check() {
    let (_work, project, log) = declaring("T001.exists").await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    let journal = project.journal();
    assert!(
        journal.iter().any(|e| matches!(
            &e.record,
            JournalRecord::Conflict { task, attempt: 1, checks }
                if task.as_str() == "T003" && checks == &["T001.exists"]
        )),
        "{log}"
    );
    let builders = journal
        .iter()
        .filter(|e| matches!(&e.record, JournalRecord::Session(s) if s.label.ends_with("-builder-T003")))
        .count();
    assert_eq!(builders, 1, "T003 was retried after its conflict:\n{log}");
    assert_eq!(status(&project, "T003"), "dropped");
    assert_eq!(status(&project, "T005"), "done");
    assert!(
        project
            .conflict_text(&TaskId::parse("T003").unwrap())
            .is_some(),
        "the conflict went with the revert of T003's work"
    );
}

/// The replan is shown the conflict, and the amending card's builder and
/// reviewer the checks it may change and the conflict that named them.
#[tokio::test(start_paused = true)]
async fn the_replan_and_the_amending_card_are_shown_the_conflict() {
    let (_work, project, _log) = declaring("T001.exists").await;
    let said = "T003 and T001.exists cannot both pass.";
    let replan = prompts(&project, "replan");
    assert!(replan.contains("## Declared conflicts"), "{replan}");
    assert!(replan.contains(said), "{replan}");
    for session in ["builder-T005", "reviewer-T005"] {
        let prompt = prompts(&project, session);
        assert!(
            prompt.contains("## Checks this card may amend\n\n- `T001.exists`"),
            "{session}:\n{prompt}"
        );
        assert!(prompt.contains(said), "{session}:\n{prompt}");
    }
}

/// A task's own check is its card to fix, not a done check to amend: the
/// attempt fails as any other and the next builder is told why.
#[tokio::test(start_paused = true)]
async fn a_conflict_with_the_task_s_own_check_does_not_stand() {
    let (_work, project, log) = declaring("T003.exists").await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert!(
        !project
            .journal()
            .iter()
            .any(|e| matches!(e.record, JournalRecord::Conflict { .. })),
        "{log}"
    );
    assert!(
        log.contains("does not stand: `T003.exists` is T003's own check"),
        "{log}"
    );
    assert_eq!(status(&project, "T003"), "done");
    assert!(
        project
            .conflict_text(&TaskId::parse("T003").unwrap())
            .is_none(),
        "a conflict that did not stand was kept, and could license an amend"
    );
    let builders = prompts(&project, "builder-T003");
    assert!(
        builders.contains("Your .tenaz/conflicts/T003.md does not stand"),
        "{builders}"
    );
}
