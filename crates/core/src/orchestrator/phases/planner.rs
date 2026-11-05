//! Split and replan sessions, whose cards are checked and either kept or
//! thrown away; and what becomes of a task out of attempts: split, or
//! blocked.

use crate::backlog::Backlog;
use crate::conflict::Conflict;
use crate::ledger::{JournalEntry, JournalRecord, TaskOutcome};
use crate::orchestrator::session::SessionCtx;
use crate::orchestrator::{RunError, Runner};
use crate::project::{conflict_path, note_path};
use crate::role::Role;
use crate::spec::coverage;
use crate::task::{TaskCard, TaskId, TaskStatus};

use super::{CONFLICTS_TITLE, PromptParts};

/// A planner session about one task (split).
pub(super) struct PlannerTask {
    id: TaskId,
    card_text: String,
    history: Vec<String>,
}

/// The section of a split's briefing with the failed attempts' notes.
const SPLIT_NOTES_TITLE: &str = "Builder notes from the failed attempts";

/// The journal entries of the recent orchestrator journal a planner is shown.
const RECENT_JOURNAL: usize = 12;

impl Runner {
    /// A split or replan session; the cards it leaves are validated and
    /// either committed or thrown away.
    pub(super) async fn planner_session(
        &mut self,
        role: Role,
        task: Option<&PlannerTask>,
        reason: &str,
    ) -> Result<bool, RunError> {
        let before = self
            .deps
            .vcs
            .commit_all(&format!("tenaz: bookkeeping before {role}"))?;
        let tasks = self.project.load_backlog().backlog;
        let extra = vec![
            ("Task list".to_owned(), tasks.table()),
            ("Why this session".to_owned(), reason.to_owned()),
        ];
        let prompt = self.planner_prompt(role, task, extra)?;
        self.session(
            role,
            &prompt,
            &SessionCtx {
                task: task.map(|t| t.id.clone()),
                replan_before: (role == Role::Replan).then(|| tasks.clone()),
                ..Default::default()
            },
        )
        .await?;
        // Statuses are put right first, so the backlog is judged with the
        // statuses it will keep: a blocked task the planner marked done is
        // blocked again, and its conflict stands (ADR 0018).
        self.undo_status_changes(role, &tasks)?;
        let (after, errors) = self.planner_errors(&tasks);
        let task_id = task.map(|t| t.id.clone());
        if !errors.is_empty() {
            self.deps.vcs.restore_paths(&before, &[".tenaz/tasks"])?;
            self.journal(rejected(role, task_id, errors.clone()));
            self.log(&format!("{role} rejected: {}", errors.join("; ")));
            return Ok(false);
        }
        let suffix = task.map(|t| format!(" {}", t.id)).unwrap_or_default();
        self.deps
            .vcs
            .commit_all(&format!("tenaz: {role}{suffix}"))?;
        let added: Vec<TaskId> = after
            .ids()
            .filter(|id| !tasks.contains(id))
            .cloned()
            .collect();
        self.journal(accepted(role, task_id, added));
        Ok(true)
    }

    /// The planner's prompt: `extra` (the task list and why), the failure
    /// history and the builders' notes of a task to split, the builders'
    /// context budget, the recent journal and the planner's notes.
    fn planner_prompt(
        &self,
        role: Role,
        task: Option<&PlannerTask>,
        mut extra: Vec<(String, String)>,
    ) -> Result<String, RunError> {
        if let (Role::Split, Some(task)) = (role, task) {
            extra.push(("Failure history".to_owned(), task.history.join("\n\n")));
            if let Some(notes) = self.clipped_note(task.id.as_str()) {
                extra.push((SPLIT_NOTES_TITLE.to_owned(), notes));
            }
        }
        extra.push(self.card_sizing());
        extra.push((
            "Recent orchestrator journal".to_owned(),
            self.recent_journal(),
        ));
        if role == Role::Replan {
            extra.extend(self.conflicts_section());
        }
        if let Some(planner) = self.project.note("PLANNER") {
            extra.push(("Previous planner notes".to_owned(), planner));
        }
        let fields = task
            .map(|t| vec![("Task".to_owned(), t.id.to_string())])
            .unwrap_or_default();
        let card = task.map(|t| (t.id.card_path(), t.card_text.clone()));
        self.briefing(
            role,
            PromptParts {
                fields,
                card: card.as_ref().map(|(p, t)| (p.as_str(), t.as_str())),
                extra,
                ..PromptParts::default()
            },
        )
    }

    /// The last entries of the journal, oldest first, one line each.
    fn recent_journal(&self) -> String {
        let journal = self.project.journal();
        let skip = journal.len().saturating_sub(RECENT_JOURNAL);
        journal[skip..]
            .iter()
            .map(JournalEntry::summary)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The cards a planner left, after `before`, and what is wrong with them.
    fn planner_errors(&self, before: &Backlog) -> (Backlog, Vec<String>) {
        let loaded = self.project.load_backlog();
        let after = loaded.backlog;
        let mut errors = loaded.errors;
        let cov = coverage(&self.project.requirements(), after.cards(), false);
        if !cov.missing.is_empty() {
            errors.push(format!(
                "requirements left uncovered: {}",
                cov.missing.join(", ")
            ));
        }
        errors.extend(
            before
                .ids()
                .filter(|id| !after.contains(id))
                .map(|id| format!("card {id} was deleted")),
        );
        (after, errors)
    }

    /// Puts right the statuses a `role` session left that it may not set,
    /// against the backlog as it was `before` (`status_fixes`), and logs a
    /// reopen that is undone.
    fn undo_status_changes(&self, role: Role, before: &Backlog) -> Result<(), RunError> {
        let after = self.project.load_backlog().backlog;
        for fix in status_fixes(role, before, &after, |b| self.project.conflicts(b)) {
            if fix.refused_reopen() {
                self.log(&format!(
                    "  the {role} set {} back to todo, {UNDONE_REOPEN}",
                    fix.id
                ));
            }
            self.apply(&fix.id, fix.fix)?;
        }
        Ok(())
    }

    /// Writes `fix` on card `id`.
    fn apply(&self, id: &TaskId, fix: StatusFix) -> Result<(), RunError> {
        match fix {
            StatusFix::Reset => self.project.reset_card(id)?,
            StatusFix::Restore(old) => self.project.set_status(id, old)?,
        }
        Ok(())
    }

    /// A task out of attempts, or too big for its context budget: split it, or block it.
    pub(super) async fn escalate(
        &mut self,
        task: &TaskCard,
        history: Vec<String>,
        why: &str,
    ) -> Result<(), RunError> {
        self.revert_work(&task.id)?;
        if task.id.split_depth() < self.params.max_split_depth
            && self.split_task(task, &history, why).await?
        {
            return Ok(());
        }
        self.block_task(task, history)
    }

    /// `task`'s builder declared at `attempt` that it cannot pass without
    /// changing checks of done tasks: the task is blocked at once, and a
    /// replan routes around it and may amend those checks (ADR 0009).
    pub(super) async fn escalate_conflict(
        &mut self,
        task: &TaskCard,
        attempt: u32,
        conflict: Conflict,
    ) -> Result<(), RunError> {
        let checks = conflict.checks.join(", ");
        self.log(&format!(
            "  {} declares a conflict with done checks {checks}; a replan may amend them",
            task.id
        ));
        // The attempt's verify failure is already in the history; this line
        // says what came of it.
        let mut history = self.current_history();
        history.push(format!(
            "Attempt {attempt}: declared a conflict with {checks}"
        ));
        self.revert_work(&task.id)?;
        self.block_task(task, history)?;
        self.journal(JournalRecord::Conflict {
            task: task.id.clone(),
            attempt,
            checks: conflict.checks,
        });
        self.ledger.current = None;
        self.save()?;
        let why = format!(
            "{} declared a conflict with checks of done tasks ({checks}) and is blocked. See \"{CONFLICTS_TITLE}\".",
            task.id
        );
        self.planner_session(Role::Replan, None, &why).await?;
        Ok(())
    }

    /// A split session for `task`. True when it was split into two or more
    /// tasks; a split into fewer is thrown away.
    async fn split_task(
        &mut self,
        task: &TaskCard,
        history: &[String],
        why: &str,
    ) -> Result<bool, RunError> {
        let planner = PlannerTask {
            id: task.id.clone(),
            card_text: self.project.card_text(&task.id)?,
            history: history.to_vec(),
        };
        if !self
            .planner_session(Role::Split, Some(&planner), why)
            .await?
        {
            return Ok(false);
        }
        let children: Vec<TaskId> = self
            .project
            .load_backlog()
            .backlog
            .children(&task.id)
            .iter()
            .map(|c| c.id.clone())
            .collect();
        if children.len() >= 2 {
            let names = children
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            self.checkpoint_task(
                &task.id,
                TaskStatus::Split,
                &format!("tenaz: {} split into {names}", task.id),
            )?;
            self.journal(JournalRecord::Task {
                task: task.id.clone(),
                outcome: TaskOutcome::Split,
                children: Some(children),
                history: None,
                attempts: None,
                reviewed: None,
                stopped_sessions: None,
            });
            return Ok(true);
        }
        self.revert_work(&task.id)?;
        Ok(false)
    }

    /// Reverts the work since the current task started, but keeps the
    /// task's notes and conflict: they say what its attempts measured and
    /// ruled out, which the split, the replan and later readers need.
    fn revert_work(&self, id: &TaskId) -> Result<(), RunError> {
        let kept: Vec<(String, String)> = [note_path(id.as_str()), conflict_path(id.as_str())]
            .into_iter()
            .filter_map(|path| self.project.read_if(&path).map(|text| (path, text)))
            .collect();
        self.deps.vcs.reset_hard(&self.current_base())?;
        for (path, text) in kept {
            self.project.write(&path, &text)?;
        }
        Ok(())
    }

    /// Blocks `task` with the history of its attempts.
    fn block_task(&mut self, task: &TaskCard, history: Vec<String>) -> Result<(), RunError> {
        self.checkpoint_task(
            &task.id,
            TaskStatus::Blocked,
            &format!("tenaz: {} blocked", task.id),
        )?;
        self.journal(JournalRecord::Task {
            task: task.id.clone(),
            outcome: TaskOutcome::Blocked,
            children: None,
            history: Some(history),
            attempts: None,
            reviewed: None,
            stopped_sessions: None,
        });
        self.log(&format!("{} blocked", task.id));
        Ok(())
    }
}

/// The journal record of a planner session whose cards were thrown away.
fn rejected(role: Role, task: Option<TaskId>, errors: Vec<String>) -> JournalRecord {
    match role {
        Role::Split => JournalRecord::SplitRejected { task, errors },
        _ => JournalRecord::ReplanRejected { task, errors },
    }
}

/// The journal record of a planner session whose cards were kept.
fn accepted(role: Role, task: Option<TaskId>, added: Vec<TaskId>) -> JournalRecord {
    match role {
        Role::Split => JournalRecord::SplitAccepted { task, added },
        _ => JournalRecord::ReplanAccepted { task, added },
    }
}

/// What the log says of a reopen Tenaz undoes.
const UNDONE_REOPEN: &str =
    "but no card still to build amends a check of a conflict it declared; it stays blocked";

/// What Tenaz writes on one card a planner left.
struct CardFix {
    id: TaskId,
    fix: StatusFix,
    /// A replan set it from `blocked` back to `todo` (`reopens`).
    reopen: bool,
}

impl CardFix {
    /// A reopen Tenaz undoes.
    fn refused_reopen(&self) -> bool {
        self.reopen && self.fix == StatusFix::Restore(TaskStatus::Blocked)
    }
}

/// What Tenaz writes on the cards a `role` session left, `after`, against
/// the backlog as it was `before`: the statuses it may not set are put
/// right (`status_fix`), then a replan's reopens are judged against the
/// conflicts that stand with those statuses (`conflicts_of`, `reopen_fix`,
/// ADR 0018).
fn status_fixes(
    role: Role,
    before: &Backlog,
    after: &Backlog,
    conflicts_of: impl Fn(&Backlog) -> Vec<Conflict>,
) -> Vec<CardFix> {
    let reopen = |t: &TaskCard| reopens(role, before.get(&t.id), t);
    let mut fixes: Vec<CardFix> = after
        .cards()
        .filter(|t| !reopen(t))
        .filter_map(|t| {
            let fix = status_fix(before.get(&t.id), t)?;
            Some(CardFix {
                id: t.id.clone(),
                fix,
                reopen: false,
            })
        })
        .collect();
    let put_right = Backlog::from_cards(after.cards().map(|t| {
        let mut card = t.clone();
        if let Some(f) = fixes.iter().find(|f| f.id == t.id) {
            card.status = match f.fix {
                StatusFix::Reset => TaskStatus::Todo,
                StatusFix::Restore(old) => old,
            };
        }
        card
    }));
    let conflicts = conflicts_of(&put_right);
    fixes.extend(after.cards().filter(|t| reopen(t)).map(|t| CardFix {
        id: t.id.clone(),
        fix: reopen_fix(&t.id, &conflicts),
        reopen: true,
    }));
    fixes
}

/// The blocked tasks of `before` that a replan, leaving `after`, set back
/// to `todo` and that Tenaz blocks again when it ends (`status_fixes`).
pub(crate) fn refused_reopens_of(
    before: &Backlog,
    after: &Backlog,
    conflicts_of: impl Fn(&Backlog) -> Vec<Conflict>,
) -> Vec<TaskId> {
    status_fixes(Role::Replan, before, after, conflicts_of)
        .into_iter()
        .filter(CardFix::refused_reopen)
        .map(|f| f.id)
        .collect()
}

/// What Tenaz does to a card a planner left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusFix {
    /// `todo` with no attempts, as a new card starts.
    Reset,
    /// Back to the status it had before the planner.
    Restore(TaskStatus),
}

/// What to do with card `after`, which was `before` the planner; `None`
/// keeps it. A new card starts as `todo`; an old one keeps its status
/// unless the planner may change it (`may_change_status`).
fn status_fix(before: Option<&TaskCard>, after: &TaskCard) -> Option<StatusFix> {
    match before {
        None => (after.status != TaskStatus::Todo).then_some(StatusFix::Reset),
        Some(old) => {
            (!may_change_status(old.status, after.status)).then_some(StatusFix::Restore(old.status))
        }
    }
}

/// Whether a `role` session set card `after`, which was `before` it, from
/// `blocked` back to `todo`, which only a replan may (ADR 0018).
fn reopens(role: Role, before: Option<&TaskCard>, after: &TaskCard) -> bool {
    role == Role::Replan
        && before.is_some_and(|old| old.status == TaskStatus::Blocked)
        && after.status == TaskStatus::Todo
}

/// What to do with blocked task `id` that a replan set back to `todo`,
/// given the `conflicts` that stand once the other statuses are put
/// right. While its own conflict stands (a card still to build amends one
/// of its checks), it starts again with no attempts and
/// `LoadedBacklog::ready` holds it until that card is built (ADR 0018).
/// Otherwise it is blocked again.
fn reopen_fix(id: &TaskId, conflicts: &[Conflict]) -> StatusFix {
    if conflicts.iter().any(|c| &c.task == id) {
        StatusFix::Reset
    } else {
        StatusFix::Restore(TaskStatus::Blocked)
    }
}

/// Whether a split or replan session may change a card's status `from`
/// one `to` another: it may drop a task that is not started or is blocked,
/// and change nothing else. Blocked to todo is not allowed here: a replan's
/// reopen is judged apart (`reopen_fix`, ADR 0018).
fn may_change_status(from: TaskStatus, to: TaskStatus) -> bool {
    from == to
        || (to == TaskStatus::Dropped && matches!(from, TaskStatus::Todo | TaskStatus::Blocked))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status; a new one fails to compile here until it is listed.
    fn all() -> [TaskStatus; 6] {
        use TaskStatus::{Active, Blocked, Done, Dropped, Split, Todo};
        let all = [Todo, Active, Done, Blocked, Split, Dropped];
        for status in all {
            match status {
                Todo | Active | Done | Blocked | Split | Dropped => {}
            }
        }
        all
    }

    #[test]
    fn a_planner_may_drop_only_a_task_not_started_or_blocked() {
        let droppable: Vec<TaskStatus> = all()
            .into_iter()
            .filter(|&from| may_change_status(from, TaskStatus::Dropped))
            .collect();
        // A dropped task stays dropped: that is no change.
        assert_eq!(
            droppable,
            [TaskStatus::Todo, TaskStatus::Blocked, TaskStatus::Dropped]
        );
    }

    #[test]
    fn a_planner_may_change_no_other_status() {
        for from in all() {
            for to in all().into_iter().filter(|&to| to != TaskStatus::Dropped) {
                assert_eq!(may_change_status(from, to), from == to, "{from} -> {to}");
            }
        }
    }

    fn card(id: &str, status: &str) -> TaskCard {
        let text = format!("---\nid: {id}\ntitle: t\nstatus: {status}\n---\n");
        TaskCard::parse(&format!("{id}.md"), &text).unwrap()
    }

    fn conflict_of(task: &str) -> Conflict {
        let id = TaskId::parse(task).unwrap();
        Conflict::read(&id, "---\nchecks: [T001.runs]\n---\n").unwrap()
    }

    /// A blocked task set back to `todo` is reopened, with no attempts,
    /// only while its own conflict stands; another task's does not count.
    #[test]
    fn a_replan_may_reopen_a_blocked_task_only_while_its_conflict_stands() {
        let id = TaskId::parse("T002").unwrap();
        assert_eq!(reopen_fix(&id, &[conflict_of("T002")]), StatusFix::Reset);
        let blocked_again = StatusFix::Restore(TaskStatus::Blocked);
        assert_eq!(reopen_fix(&id, &[conflict_of("T003")]), blocked_again);
        assert_eq!(reopen_fix(&id, &[]), blocked_again);
    }

    /// A reopen is refused unless a card still to build amends a check of
    /// its conflict, judged once the statuses a planner may not set are put
    /// right: an amending card written as `done` still counts. A task with
    /// no conflict, or one not set back to `todo`, is not a refused reopen.
    #[test]
    fn a_reopen_is_refused_without_an_amend_still_to_build() {
        let amending = |status: &str, check: &str| {
            let text =
                format!("---\nid: T005\ntitle: t\nstatus: {status}\namends: [{check}]\n---\n");
            TaskCard::parse("T005.md", &text).unwrap()
        };
        let done = card("T001", "done");
        let before = Backlog::from_cards([done.clone(), card("T002", "blocked")]);
        let refused = |reopened: &str, amender: Option<TaskCard>, conflict: bool| {
            let after = Backlog::from_cards(
                [done.clone(), card("T002", reopened)]
                    .into_iter()
                    .chain(amender),
            );
            let conflicts = |b: &Backlog| {
                [conflict_of("T002")]
                    .into_iter()
                    .filter(|c| conflict && c.stands_in(b))
                    .collect()
            };
            refused_reopens_of(&before, &after, conflicts)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let other = Some(amending("todo", "T001.other"));
        assert_eq!(
            refused("todo", other, true),
            ["T002"],
            "amends another check"
        );
        assert_eq!(refused("todo", None, false), ["T002"], "no conflict at all");
        let runs = |status| Some(amending(status, "T001.runs"));
        assert!(
            refused("todo", runs("todo"), true).is_empty(),
            "behind its amend"
        );
        assert!(
            refused("todo", runs("done"), true).is_empty(),
            "amend marked done"
        );
        assert!(refused("blocked", None, true).is_empty(), "left blocked");
        assert!(refused("dropped", None, true).is_empty(), "dropped");
    }

    /// Only a replan reopens, and only from `blocked` to `todo`.
    #[test]
    fn only_a_replan_reopens_a_blocked_task() {
        let (blocked, todo) = (card("T002", "blocked"), card("T002", "todo"));
        assert!(reopens(Role::Replan, Some(&blocked), &todo));
        assert!(!reopens(Role::Split, Some(&blocked), &todo));
        assert!(!reopens(
            Role::Replan,
            Some(&card("T002", "dropped")),
            &todo
        ));
        assert!(!reopens(
            Role::Replan,
            Some(&blocked),
            &card("T002", "active")
        ));
        assert!(!reopens(Role::Replan, None, &todo), "a new card");
    }

    /// An old card's status is restored, to what it was, unless the planner
    /// may change it; a new card starts as `todo`.
    #[test]
    fn a_status_a_planner_may_not_set_is_restored() {
        let fix = |from: &str, to: &str| status_fix(Some(&card("T002", from)), &card("T002", to));
        assert_eq!(fix("todo", "dropped"), None);
        assert_eq!(
            fix("todo", "done"),
            Some(StatusFix::Restore(TaskStatus::Todo))
        );
        assert_eq!(
            fix("done", "todo"),
            Some(StatusFix::Restore(TaskStatus::Done))
        );
        assert_eq!(
            fix("blocked", "todo"),
            Some(StatusFix::Restore(TaskStatus::Blocked))
        );
        let new = |status| status_fix(None, &card("T009", status));
        assert_eq!(new("todo"), None);
        assert_eq!(new("done"), Some(StatusFix::Reset));
    }
}
