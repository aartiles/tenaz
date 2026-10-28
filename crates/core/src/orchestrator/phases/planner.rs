//! Split and replan sessions, whose cards are checked and either kept or
//! thrown away; and what becomes of a task out of attempts: split, or
//! blocked.

use crate::backlog::Backlog;
use crate::ledger::{JournalEntry, JournalRecord, TaskOutcome};
use crate::orchestrator::session::SessionCtx;
use crate::orchestrator::{RunError, Runner};
use crate::role::Role;
use crate::spec::coverage;
use crate::task::{TaskCard, TaskId, TaskStatus};

use super::PromptParts;

/// A planner session about one task (split).
pub(super) struct PlannerTask {
    id: TaskId,
    card_text: String,
    history: Vec<String>,
}

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
                ..Default::default()
            },
        )
        .await?;
        let (after, errors) = self.planner_errors(&tasks);
        self.undo_status_changes(&tasks, &after)?;
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
    /// history of a task to split, the builders' context budget, the recent
    /// journal and the planner's notes.
    fn planner_prompt(
        &self,
        role: Role,
        task: Option<&PlannerTask>,
        mut extra: Vec<(String, String)>,
    ) -> Result<String, RunError> {
        if let (Role::Split, Some(task)) = (role, task) {
            extra.push(("Failure history".to_owned(), task.history.join("\n\n")));
        }
        extra.push(self.card_sizing());
        extra.push((
            "Recent orchestrator journal".to_owned(),
            self.recent_journal(),
        ));
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

    /// A new card starts as `todo`; an old one keeps its status unless the
    /// planner may change it (`may_change_status`).
    fn undo_status_changes(&self, before: &Backlog, after: &Backlog) -> Result<(), RunError> {
        for t in after.cards() {
            match before.get(&t.id) {
                None if t.status != TaskStatus::Todo => self.project.reset_card(&t.id)?,
                Some(old) if !may_change_status(old.status, t.status) => {
                    self.project.set_status(&t.id, old.status)?;
                }
                _ => {}
            }
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
        self.deps.vcs.reset_hard(&self.current_base())?;
        if task.id.split_depth() < self.params.max_split_depth
            && self.split_task(task, &history, why).await?
        {
            return Ok(());
        }
        self.block_task(task, history)
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
        self.deps.vcs.reset_hard(&self.current_base())?;
        Ok(false)
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

/// Whether a split or replan session may change a card's status `from`
/// one `to` another: it may drop a task that is not started or is blocked,
/// and change nothing else.
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
}
