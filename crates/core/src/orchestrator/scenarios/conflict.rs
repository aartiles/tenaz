//! A builder that declares its task cannot pass without changing a check
//! of a done task (ADR 0009): the task is blocked at once, a replan adds a
//! card that amends the check, and its builder and reviewer are told which
//! checks it may change. A declaration that does not stand is an ordinary
//! failed attempt whose next builder is told why.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, resume, run};
use crate::ledger::{JournalRecord, Phase, TaskOutcome};
use crate::orchestrator::phases::UNDONE_REOPEN;
use crate::orchestrator::{INVALID_CARDS_NOTE, PASSING_CONFLICT_NOTE, REFUSED_REOPENS_NOTE};
use crate::project::Project;
use crate::testing::{
    Routing, ScriptedConflict, ScriptedHarness, ScriptedPassingConflict, UNDECLARED, VirtualClock,
};

/// Plays the fake goal with T003's first builder declaring a conflict with
/// `check`; the project, its log and the work directory.
async fn declaring(check: &str) -> (tempfile::TempDir, Project, String) {
    declaring_routed(check, Routing::Drop).await
}

/// As `declaring`, with the replan routing T003 as `routing` says.
async fn declaring_routed(check: &str, routing: Routing) -> (tempfile::TempDir, Project, String) {
    // The scripted replan's first amend may name `UNDECLARED`, which only
    // works while the conflict names another check.
    assert_ne!(check, UNDECLARED, "the conflict names the undeclared check");
    play(
        ScriptedHarness {
            conflict: Some(ScriptedConflict {
                task: "T003".into(),
                check: check.into(),
                routing,
            }),
            ..Default::default()
        },
        |_| {},
    )
    .await
}

/// Plays the fake goal with `harness`, after `before` prepares the project.
async fn play(
    harness: ScriptedHarness,
    before: impl FnOnce(&Project),
) -> (tempfile::TempDir, Project, String) {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=3"]);
    before(&project);
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

/// The prompts of the sessions labelled `…-<session>`, in the order they
/// ran: labels start with a sequence number, and `read_dir` gives no
/// order (it differs between macOS and Linux).
fn prompts(project: &Project, session: &str) -> String {
    let suffix = format!("-{session}.prompt.md");
    let mut paths: Vec<_> = std::fs::read_dir(project.private_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(&suffix))
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
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
async fn a_declared_conflict_blocks_the_task_and_a_replan_routes_it_to_an_amending_card() {
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
}

/// A replan may route the blocked task's work through the amending card
/// by reopening the task behind it, as Qwen Code did on 2026-11-03,
/// instead of dropping it. The amend is accepted, and the reopened task is
/// built once the check is amended.
#[tokio::test(start_paused = true)]
async fn a_replan_may_reopen_the_blocked_task_behind_the_amending_card() {
    reopened_behind_its_amend(Routing::Reopen).await;
}

/// A reopened task waits for its amend even when its `depends` do not
/// say so (`LoadedBacklog::ready`).
#[tokio::test(start_paused = true)]
async fn a_reopened_task_waits_for_its_amend_without_depending_on_it() {
    reopened_behind_its_amend(Routing::ReopenUnlinked).await;
}

/// A reopen is judged after the other statuses are put right: the
/// amending card the replan wrote as `done` is `todo` by then.
#[tokio::test(start_paused = true)]
async fn a_reopen_is_judged_once_the_other_statuses_are_put_right() {
    reopened_behind_its_amend(Routing::ReopenAmenderMarkedDone).await;
}

/// Plays T003's conflict with the replan reopening T003 as `routing`
/// says, and checks the run built T005, then T003.
async fn reopened_behind_its_amend(routing: Routing) {
    let (_work, project, log) = declaring_routed("T001.exists", routing).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert!(accepted_t005(&project), "{log}");
    assert!(!rejected(&project), "{log}");
    // T003 sorts before T005: without the wait it would be built first.
    let done = done_in_order(&project);
    assert!(
        done.ends_with(&["T005".into(), "T003".into()]),
        "{done:?}\n{log}"
    );
    assert!(log.contains("T003 done (1 attempt)"), "{log}");
    // A reopen Tenaz keeps is not questioned, the amending card written
    // as `done` included: it is judged once that is put right.
    assert!(!log.contains("Tenaz would block each again"), "{log}");
    assert!(project.conflict_text("T003").is_none(), "{log}");
    let builders = prompts(&project, "builder-T005");
    assert!(
        builders.contains("## Checks this card may amend\n\n- `T001.exists`"),
        "{builders}"
    );
}

/// A status the planner may not set is put right before its cards are
/// judged: the blocked task it marked done is blocked again, its conflict
/// stands, and the amending card is accepted.
#[tokio::test(start_paused = true)]
async fn a_blocked_task_a_replan_marked_done_is_blocked_again() {
    let (_work, project, log) = declaring_routed("T001.exists", Routing::MarkDone).await;
    assert!(accepted_t005(&project), "{log}");
    assert!(!rejected(&project), "{log}");
    assert_eq!(status(&project, "T003"), "blocked", "{log}");
    assert_eq!(status(&project, "T005"), "done", "{log}");
}

/// A reopen with no card that amends the task's checks is undone: the
/// task is blocked again, and the replan is not rejected for it.
#[tokio::test(start_paused = true)]
async fn a_reopen_without_an_amend_is_blocked_again() {
    let (_work, project, log) = declaring_routed("T001.exists", Routing::ReopenWithoutAmend).await;
    assert!(!rejected(&project), "{log}");
    assert_eq!(status(&project, "T003"), "blocked", "{log}");
    let done = done_in_order(&project);
    assert!(!done.iter().any(|t| t == "T003"), "{done:?}\n{log}");
    assert!(
        log.contains("T003 set back to todo, but Tenaz would block each again"),
        "{log}"
    );
    assert!(log.contains("set T003 back to todo, but no card"), "{log}");
    // Once per session: the conflict's replan, then the one that finds no
    // task ready, are each asked once.
    let asked = |label: &str| log.matches(&format!("{label}: T003 set back")).count();
    assert_eq!((asked("007-replan"), asked("010-replan")), (1, 1), "{log}");
}

/// Told before it ends that its reopen would be undone, the replan may
/// still route the task another way: here it drops it.
#[tokio::test(start_paused = true)]
async fn a_replan_told_its_reopen_would_be_undone_may_drop_the_task() {
    let (_work, project, log) =
        declaring_routed("T001.exists", Routing::ReopenWithoutAmendThenDrop).await;
    assert!(!rejected(&project), "{log}");
    assert_eq!(status(&project, "T003"), "dropped", "{log}");
    assert!(
        done_in_order(&project).contains(&"T005".to_owned()),
        "{log}"
    );
    assert!(!log.contains(UNDONE_REOPEN), "{log}");
}

/// The tasks done, in the order the journal records them.
fn done_in_order(project: &Project) -> Vec<String> {
    project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Task {
                task,
                outcome: TaskOutcome::Done,
                ..
            } => Some(task.to_string()),
            _ => None,
        })
        .collect()
}

fn accepted_t005(project: &Project) -> bool {
    project.journal().iter().any(|e| {
        matches!(&e.record, JournalRecord::ReplanAccepted { added, .. }
            if added.iter().any(|id| id.as_str() == "T005"))
    })
}

/// The labels of the sessions whose watchdog noted `kind`, once per note.
fn noted(project: &Project, kind: &str) -> Vec<String> {
    project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) => Some(s),
            _ => None,
        })
        .flat_map(|s| {
            s.watchdog
                .iter()
                .filter(|e| e.kind == kind)
                .map(|_| s.label.clone())
        })
        .collect()
}

fn rejected(project: &Project) -> bool {
    project
        .journal()
        .iter()
        .any(|e| matches!(e.record, JournalRecord::ReplanRejected { .. }))
}

/// A builder that declared a conflict stopped as told: it is not asked
/// for a passing check, which would talk it out of the conflict.
#[tokio::test(start_paused = true)]
async fn a_builder_that_declared_a_conflict_is_not_asked_for_a_check() {
    let (_work, _project, log) = declaring("T001.exists").await;
    assert!(
        !log.contains("no passing T003 check; asking for one"),
        "{log}"
    );
}

/// A conflict file left from before the attempt, here written before the
/// run as one left when a blocked task is reopened, is not the attempt's:
/// T003's attempts fail and it is split, with no conflict declared.
#[tokio::test(start_paused = true)]
async fn a_conflict_left_from_before_the_attempt_is_not_inherited() {
    let harness = ScriptedHarness {
        failing_task: Some("T003".into()),
        ..Default::default()
    };
    let (_work, project, log) = play(harness, |project| {
        project
            .write(
                ".tenaz/conflicts/T003.md",
                "---\nchecks: [T001.exists]\n---\nOld.\n",
            )
            .unwrap();
    })
    .await;
    assert!(
        !project
            .journal()
            .iter()
            .any(|e| matches!(e.record, JournalRecord::Conflict { .. })),
        "{log}"
    );
    assert_eq!(status(&project, "T003"), "split", "{log}");
}

/// The conflict outlives the revert of the blocked task's work: later
/// replans and the amending card read it.
#[tokio::test(start_paused = true)]
async fn a_declared_conflict_outlives_the_revert_of_its_task_s_work() {
    let (_work, project, log) = declaring("T001.exists").await;
    assert!(project.conflict_text("T003").is_some(), "{log}");
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
        project.conflict_text("T003").is_none(),
        "a conflict that did not stand was kept, and could license an amend"
    );
    let builders = prompts(&project, "builder-T003");
    assert!(
        builders.contains("Your .tenaz/conflicts/T003.md does not stand"),
        "{builders}"
    );
}

/// A run blocked with no task ready gets its replan again on `--resume`:
/// here that replan, told its reopen would be undone, routes around it.
#[tokio::test(start_paused = true)]
async fn a_resumed_run_with_no_task_ready_replans_again() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=3"]);
    let clock = Arc::new(VirtualClock::new());
    let harness = |routing| {
        Arc::new(ScriptedHarness {
            conflict: Some(ScriptedConflict {
                task: "T003".into(),
                check: "T001.exists".into(),
                routing,
            }),
            ..Default::default()
        })
    };
    let first = harness(Routing::ReopenWithoutAmend);
    run(
        &project,
        deps(&project, vcs.clone(), first, clock.clone()),
        CancellationToken::new(),
    )
    .await;
    let blocked = project.load_ledger().unwrap();
    assert_eq!(blocked.phase, Phase::Blocked);
    assert!(blocked.stuck_replanned, "{:?}", blocked.blocked_reason);
    let again = harness(Routing::ReopenWithoutAmendThenDrop);
    resume(&project, deps(&project, vcs, again, clock)).await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert_eq!(status(&project, "T003"), "dropped", "{log}");
}

/// A replan whose cards were thrown away is followed by one told why:
/// here the first amends a check no conflict names, and the next, shown
/// that error, amends the declared check and the run completes.
#[tokio::test(start_paused = true)]
async fn the_next_replan_is_told_why_the_last_was_rejected() {
    let (_work, project, log) =
        declaring_routed("T001.exists", Routing::AmendUndeclaredFirst).await;
    let first_replan = project
        .journal()
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-replan") => Some(s.label.clone()),
            _ => None,
        })
        .expect("a replan ran");
    assert_eq!(
        noted(&project, INVALID_CARDS_NOTE),
        [first_replan],
        "the first replan is told once, then left as it was: {log}"
    );
    assert!(rejected(&project), "{log}");
    assert!(accepted_t005(&project), "{log}");
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    let replans = prompts(&project, "replan");
    let told = format!("## {}", crate::orchestrator::REJECTED_REPLAN_TITLE);
    assert_eq!(replans.matches(&told).count(), 1, "{replans}");
    assert!(
        replans.contains(&format!("- T005: amends `{UNDECLARED}`")),
        "{replans}"
    );
}

/// A rejection older than the recent journal still reaches the replan,
/// with every error: here it is journaled before the run, whose own
/// entries push it out of the recent journal by the first replan, as the
/// test checks. The scripted replan amends a check no conflict names
/// unless it is told why the last replan was rejected, so it is accepted
/// at once only when told.
#[tokio::test(start_paused = true)]
async fn a_rejection_out_of_the_recent_journal_still_reaches_the_replan() {
    let harness = ScriptedHarness {
        conflict: Some(ScriptedConflict {
            task: "T003".into(),
            check: "T001.exists".into(),
            routing: Routing::AmendUndeclaredFirst,
        }),
        ..Default::default()
    };
    let (_work, project, log) = play(harness, |project| {
        let at = || "2026-11-12T10:00:00Z".to_owned();
        let errors = vec![
            "T009: amends `T002.state`".to_owned(),
            "card T007 was deleted".to_owned(),
        ];
        project
            .append_journal(at(), JournalRecord::ReplanRejected { task: None, errors })
            .unwrap();
    })
    .await;
    let replans = prompts(&project, "replan");
    let recent = replans
        .split("## Recent orchestrator journal")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_default();
    assert!(
        recent.contains("conflict T003"),
        "no recent journal: {replans}"
    );
    assert!(
        !recent.contains("replan-rejected"),
        "the rejection is still in the recent journal: {recent}"
    );
    assert!(
        replans.contains("- T009: amends `T002.state`\n- card T007 was deleted"),
        "{replans}"
    );
    let rejections = project
        .journal()
        .iter()
        .filter(|e| matches!(e.record, JournalRecord::ReplanRejected { .. }))
        .count();
    assert_eq!(
        rejections, 1,
        "only the one journaled before the run: {log}"
    );
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
}

/// A replan that leaves cards Tenaz would throw away is told why before
/// it ends (ADR 0033): here it amends a check no conflict names, fixes it
/// when told, and its cards are kept the first time.
#[tokio::test(start_paused = true)]
async fn a_replan_fixes_its_cards_when_told_before_it_ends() {
    let (_work, project, log) =
        declaring_routed("T001.exists", Routing::AmendUndeclaredFixedWhenTold).await;
    let asked = noted(&project, INVALID_CARDS_NOTE);
    assert!(
        matches!(asked.as_slice(), [label] if label.ends_with("-replan")),
        "{asked:?}"
    );
    assert!(!rejected(&project), "{log}");
    assert!(accepted_t005(&project), "{log}");
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
}

/// A replan whose cards are fine is not told they are invalid.
#[tokio::test(start_paused = true)]
async fn a_replan_with_valid_cards_is_not_told_they_are_invalid() {
    let (_work, project, log) = declaring_routed("T001.exists", Routing::Reopen).await;
    assert!(accepted_t005(&project), "no replan was kept: {log}");
    assert_eq!(
        noted(&project, INVALID_CARDS_NOTE),
        Vec::<String>::new(),
        "{log}"
    );
}

/// The watchdog's planner notes of the first replan session, in order.
fn first_replan_notes(project: &Project) -> Vec<String> {
    project
        .journal()
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-replan") => Some(s),
            _ => None,
        })
        .expect("a replan ran")
        .watchdog
        .iter()
        .map(|e| e.kind.clone())
        .filter(|k| k == INVALID_CARDS_NOTE || k == REFUSED_REOPENS_NOTE)
        .collect()
}

/// A replan whose cards are invalid and that reopens a task Tenaz would
/// block again gets both prompts in one session, the cards first: here it
/// fixes the cards, keeps the reopen, and is then told about it; its cards
/// are kept and the reopen undone.
#[tokio::test(start_paused = true)]
async fn a_replan_is_told_about_its_cards_then_its_reopen() {
    let (_work, project, log) =
        declaring_routed("T001.exists", Routing::ReopenAndAmendUndeclared).await;
    assert_eq!(
        first_replan_notes(&project),
        [INVALID_CARDS_NOTE, REFUSED_REOPENS_NOTE],
        "{log}"
    );
    assert!(accepted_t005(&project), "{log}");
    assert!(log.contains(UNDONE_REOPEN), "{log}");
    assert_eq!(status(&project, "T003"), "blocked", "{log}");
}

/// The statuses a planner may not set are written only when its cards are
/// kept: a replan whose cards are thrown away logs no undone reopen, and
/// the task it reopened stays blocked.
#[tokio::test(start_paused = true)]
async fn a_rejected_replan_writes_no_statuses() {
    let (_work, project, log) =
        declaring_routed("T001.exists", Routing::ReopenAndAmendUndeclaredIgnored).await;
    assert!(rejected(&project), "{log}");
    assert!(!log.contains(UNDONE_REOPEN), "{log}");
    assert_eq!(status(&project, "T003"), "blocked", "{log}");
}

/// Plays the fake goal with T003's first builder doing its work and also
/// declaring a conflict with T001.exists; told it does not stand, it makes
/// T003's check fail when `fails_when_told`.
async fn passing_conflict(fails_when_told: bool) -> (tempfile::TempDir, Project, String) {
    play(
        ScriptedHarness {
            passing_conflict: Some(Box::new(ScriptedPassingConflict {
                task: "T003".into(),
                check: "T001.exists".into(),
                fails_when_told,
            })),
            ..Default::default()
        },
        |_| {},
    )
    .await
}

fn conflicts_journaled(project: &Project) -> usize {
    project
        .journal()
        .iter()
        .filter(|e| matches!(e.record, JournalRecord::Conflict { .. }))
        .count()
}

/// A conflict left on an attempt that passes does not stand (ADR 0034):
/// the builder is told once before it ends, the reviewer is told, and the
/// file's removal when the task is done is logged.
#[tokio::test(start_paused = true)]
async fn a_conflict_left_on_a_passing_attempt_is_pointed_out() {
    let (_work, project, log) = passing_conflict(false).await;
    let told = noted(&project, PASSING_CONFLICT_NOTE);
    assert!(
        matches!(told.as_slice(), [label] if label.ends_with("-builder-T003")),
        "{told:?}"
    );
    let reviews = prompts(&project, "reviewer-T003");
    assert!(reviews.contains("## Conflict file"), "{reviews}");
    let other = prompts(&project, "reviewer-T001");
    assert!(!other.is_empty(), "T001 was not reviewed: {log}");
    assert!(!other.contains("## Conflict file"), "{other}");
    assert!(
        log.contains(".tenaz/conflicts/T003.md removed: the attempt passed"),
        "{log}"
    );
    assert_eq!(status(&project, "T003"), "done", "{log}");
    assert_eq!(conflicts_journaled(&project), 0, "{log}");
}

/// Told that its conflict does not stand, a builder whose check now tests
/// what the card asks fails the attempt, and the conflict stands: the task
/// is blocked and a replan runs.
#[tokio::test(start_paused = true)]
async fn a_builder_told_its_conflict_does_not_stand_may_make_it_stand() {
    let (_work, project, log) = passing_conflict(true).await;
    let told = noted(&project, PASSING_CONFLICT_NOTE);
    assert!(
        matches!(told.as_slice(), [label] if label.ends_with("-builder-T003")),
        "{told:?}"
    );
    assert_eq!(conflicts_journaled(&project), 1, "{log}");
    assert!(log.contains("T003 declares a conflict"), "{log}");
    assert_eq!(status(&project, "T003"), "blocked", "{log}");
    assert!(
        !prompts(&project, "replan").is_empty(),
        "no replan ran: {log}"
    );
}

/// A conflict declared on an attempt that fails is read, not pointed out:
/// the builder stopped as it was told (ADR 0009).
#[tokio::test(start_paused = true)]
async fn a_conflict_on_a_failing_attempt_is_not_pointed_out() {
    let (_work, project, log) = declaring("T001.exists").await;
    assert_eq!(conflicts_journaled(&project), 1, "{log}");
    assert_eq!(
        noted(&project, PASSING_CONFLICT_NOTE),
        Vec::<String>::new(),
        "{log}"
    );
}

/// A conflict left while the checks pass does not stand when the attempt
/// fails only because `verify` changed the working tree (ADR 0034, 0035):
/// the builder is told, the conflict file is removed when the attempt
/// fails, and the next attempt, told about the tree, gets the task done.
#[tokio::test(start_paused = true)]
async fn a_conflict_does_not_stand_on_an_attempt_failed_by_the_tree() {
    let (_work, project, log) = play(
        ScriptedHarness {
            passing_conflict: Some(Box::new(ScriptedPassingConflict {
                task: "T003".into(),
                check: "T001.exists".into(),
                fails_when_told: false,
            })),
            stamping_task: Some("T003".into()),
            ..Default::default()
        },
        |_| {},
    )
    .await;
    assert_eq!(noted(&project, PASSING_CONFLICT_NOTE).len(), 1, "{log}");
    assert_eq!(conflicts_journaled(&project), 0, "{log}");
    // Removed when the attempt fails, so no escalation could carry it on.
    assert!(
        log.contains(".tenaz/conflicts/T003.md does not stand: the attempt's checks pass"),
        "{log}"
    );
    assert_eq!(status(&project, "T003"), "done", "{log}");
}
