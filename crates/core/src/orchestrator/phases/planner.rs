//! Split and replan sessions, whose cards are checked and either kept or
//! thrown away; and what becomes of a task out of attempts: split, or
//! blocked.

mod judge;

use crate::conflict::Conflict;
use crate::ledger::{JournalEntry, JournalRecord, TaskOutcome};
use crate::orchestrator::session::SessionCtx;
use crate::orchestrator::{RunError, Runner};
use crate::project::{conflict_path, note_path};
use crate::role::Role;
use crate::task::{TaskCard, TaskId, TaskStatus};

use super::{CONFLICTS_TITLE, PromptParts, REJECTED_REPLAN_TITLE, error_list};
pub(crate) use judge::PlannerSession;
use judge::{Judgement, StatusFix, judge};

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
                planner_before: Some(tasks.clone()),
                ..Default::default()
            },
        )
        .await?;
        let judgement = self.judge_planner(PlannerSession {
            role,
            before: &tasks,
        });
        let task_id = task.map(|t| t.id.clone());
        if !judgement.errors.is_empty() {
            let errors = judgement.errors;
            let logged = format!("{role} rejected: {}", errors.join("; "));
            self.deps.vcs.restore_paths(&before, &[".tenaz/tasks"])?;
            self.journal(rejected(role, task_id, errors));
            self.log(&logged);
            return Ok(false);
        }
        self.put_statuses_right(role, &judgement)?;
        let suffix = task.map(|t| format!(" {}", t.id)).unwrap_or_default();
        self.deps
            .vcs
            .commit_all(&format!("tenaz: {role}{suffix}"))?;
        let added: Vec<TaskId> = judgement
            .after
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
        let journal = self.project.journal();
        extra.push((
            "Recent orchestrator journal".to_owned(),
            recent_journal(&journal),
        ));
        if role == Role::Replan {
            extra.extend(self.conflicts_section());
            extra.extend(rejection_section(&journal, self.params.note_clip_bytes));
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

    /// The cards a planner `session` left, judged (`judge`): the one place
    /// both the end of the session and the prompts before it (ADR 0018 and
    /// 0033) judge them.
    pub(crate) fn judge_planner(&self, session: PlannerSession<'_>) -> Judgement {
        judge(
            session,
            self.project.load_cards(),
            &self.project.requirements(),
            |b| self.project.conflicts(b),
        )
    }

    /// Writes the statuses `judgement` puts right on the cards a `role`
    /// session left, and logs a reopen that is undone.
    fn put_statuses_right(&self, role: Role, judgement: &Judgement) -> Result<(), RunError> {
        for fix in &judgement.fixes {
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

/// The last entries of `journal`, oldest first, one line each.
fn recent_journal(journal: &[JournalEntry]) -> String {
    let skip = journal.len().saturating_sub(RECENT_JOURNAL);
    journal[skip..]
        .iter()
        .map(JournalEntry::summary)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The section that tells a replan, until one is accepted, why the last
/// replan's changes to the cards were thrown away: its notes may describe
/// cards that are not in the task list, and the recent journal names the
/// rejection without its errors, or not at all once later entries push it
/// out. The errors are cut to `limit` bytes, as notes are. `None` when the
/// last replan was accepted, or there was none.
fn rejection_section(journal: &[JournalEntry], limit: usize) -> Option<(String, String)> {
    let last = journal.iter().rev().find(|entry| {
        matches!(
            entry.record,
            JournalRecord::ReplanRejected { .. } | JournalRecord::ReplanAccepted { .. }
        )
    })?;
    let JournalRecord::ReplanRejected { errors, .. } = &last.record else {
        return None;
    };
    Some((
        REJECTED_REPLAN_TITLE.to_owned(),
        format!(
            "Its changes to the cards were thrown away for:\n{}",
            error_list(errors, limit)
        ),
    ))
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

/// What the log says of a reopen Tenaz undoes; scenarios know it by this.
pub(crate) const UNDONE_REOPEN: &str =
    "but no card still to build amends a check of a conflict it declared; it stays blocked";

#[cfg(test)]
mod tests {
    use super::*;

    /// Large enough for every error list below but the clipped one.
    const LIMIT: usize = 1_000;

    fn entry(record: JournalRecord) -> JournalEntry {
        JournalEntry {
            at: "2026-11-12T10:00:00Z".to_owned(),
            record,
        }
    }

    fn replan_rejected(errors: &[&str]) -> JournalEntry {
        entry(JournalRecord::ReplanRejected {
            task: None,
            errors: errors.iter().map(|e| (*e).to_owned()).collect(),
        })
    }

    fn replan_accepted() -> JournalEntry {
        entry(JournalRecord::ReplanAccepted {
            task: None,
            added: vec![],
        })
    }

    fn split_rejected() -> JournalEntry {
        entry(JournalRecord::SplitRejected {
            task: None,
            errors: vec!["card T009 was deleted".to_owned()],
        })
    }

    /// Each error of the last replan, rejected after an accepted one, on a
    /// line of its own under the section's title.
    #[test]
    fn a_replan_after_a_rejected_one_is_told_each_error() {
        let journal = [
            replan_accepted(),
            replan_rejected(&["T024: amends `T002.state`", "card T007 was deleted"]),
        ];
        let (title, body) = rejection_section(&journal, LIMIT).expect("a section");
        assert_eq!(title, REJECTED_REPLAN_TITLE);
        assert!(
            body.contains("- T024: amends `T002.state`\n- card T007 was deleted"),
            "{body}"
        );
    }

    /// A replan accepted since the rejection has its cards in the task list:
    /// nothing is left to tell.
    #[test]
    fn a_replan_accepted_since_the_rejection_clears_it() {
        let journal = [
            replan_rejected(&["card T007 was deleted"]),
            replan_accepted(),
        ];
        assert_eq!(rejection_section(&journal, LIMIT), None);
    }

    /// With no replan yet there is nothing to tell.
    #[test]
    fn a_first_replan_is_told_nothing() {
        assert_eq!(rejection_section(&[], LIMIT), None);
    }

    /// Of two rejections in a row, the replan is told the latest.
    #[test]
    fn the_latest_of_two_rejections_is_told() {
        let journal = [replan_rejected(&["old"]), replan_rejected(&["new"])];
        let (_, body) = rejection_section(&journal, LIMIT).expect("a section");
        assert!(body.contains("- new") && !body.contains("- old"), "{body}");
    }

    /// A rejected split is not a rejected replan.
    #[test]
    fn a_rejected_split_is_not_told_to_a_replan() {
        assert_eq!(
            rejection_section(&[replan_accepted(), split_rejected()], LIMIT),
            None
        );
    }

    /// A split after a rejected replan leaves its errors to tell.
    #[test]
    fn a_split_does_not_clear_a_rejected_replan() {
        let journal = [
            replan_rejected(&["card T007 was deleted"]),
            split_rejected(),
        ];
        let (_, body) = rejection_section(&journal, LIMIT).expect("a section");
        assert!(
            body.contains("- card T007 was deleted") && !body.contains("T009"),
            "{body}"
        );
    }

    /// A long list of errors is cut to the limit, and says how much.
    #[test]
    fn a_long_list_of_errors_is_clipped() {
        let errors: Vec<String> = (0..100)
            .map(|n| format!("card T{n:03} was deleted"))
            .collect();
        let errors: Vec<&str> = errors.iter().map(String::as_str).collect();
        let (_, body) = rejection_section(&[replan_rejected(&errors)], 200).expect("a section");
        assert!(body.contains("- card T000 was deleted"), "{body}");
        assert!(body.contains("[... clipped "), "{body}");
        assert!(!body.contains("T099"), "{body}");
    }
}
