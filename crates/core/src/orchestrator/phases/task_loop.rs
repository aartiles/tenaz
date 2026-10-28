//! The loop phase: repair cards the orchestrator cannot read, replan when
//! it is due or nothing is ready, or build the next ready task.

use crate::ledger::Phase;
use crate::orchestrator::{RunError, Runner};
use crate::role::Role;

/// Replan sessions in a row that may fix invalid task cards before the run blocks.
const CARD_REPAIRS: u32 = 2;

impl Runner {
    pub(crate) async fn loop_phase(&mut self) -> Result<(), RunError> {
        let loaded = self.project.load_backlog();
        if !loaded.errors.is_empty() {
            return self.repair_cards(&loaded.errors).await;
        }
        if self.ledger.card_repairs > 0 {
            self.ledger.card_repairs = 0;
            self.save()?;
        }
        let backlog = loaded.backlog;
        if self.replan_due() {
            return self.periodic_replan().await;
        }
        if let Some(task) = backlog.ready().first().copied().cloned() {
            return self.run_task(&backlog, &task).await;
        }
        let open: Vec<String> = backlog
            .open()
            .iter()
            .map(|t| format!("{} [{}]", t.id, t.status))
            .collect();
        if open.is_empty() {
            self.ledger.phase = Phase::Final;
            return self.save();
        }
        self.unstick(&open).await
    }

    /// A planner wrote cards the orchestrator cannot read (`errors`): a
    /// replan session gets the problems to fix before the run blocks.
    async fn repair_cards(&mut self, errors: &[String]) -> Result<(), RunError> {
        if self.ledger.card_repairs >= CARD_REPAIRS {
            return self.block(&format!("task cards are invalid: {}", errors.join("; ")));
        }
        self.ledger.card_repairs += 1;
        self.save()?;
        self.log(&format!(
            "Task cards are invalid ({}); a replan session fixes them ({} of {CARD_REPAIRS})",
            errors.join("; "),
            self.ledger.card_repairs
        ));
        self.planner_session(
            Role::Replan,
            None,
            &format!(
                "The orchestrator cannot read these task cards. Fix them (cards that are not done only), change nothing else:\n{}",
                errors
                    .iter()
                    .map(|e| format!("- {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .await?;
        Ok(())
    }

    /// Whether `replanEvery` tasks were done since the last replan. A task
    /// in progress is finished first.
    fn replan_due(&self) -> bool {
        if self.ledger.current.is_some() {
            return false;
        }
        self.params.replan_every > 0 && self.ledger.done_since_replan >= self.params.replan_every
    }

    /// A replan session after `replanEvery` tasks were done.
    async fn periodic_replan(&mut self) -> Result<(), RunError> {
        self.ledger.done_since_replan = 0;
        self.save()?;
        self.planner_session(
            Role::Replan,
            None,
            &format!(
                "Periodic replan after {} completed tasks.",
                self.params.replan_every
            ),
        )
        .await?;
        Ok(())
    }

    /// No task is ready and some are `open`: one replan session routes the
    /// work around them, then the run blocks.
    async fn unstick(&mut self, open: &[String]) -> Result<(), RunError> {
        if self.ledger.stuck_replanned {
            return self.block(&format!(
                "no task is ready; open tasks: {}",
                open.join(", ")
            ));
        }
        self.ledger.stuck_replanned = true;
        self.save()?;
        self.planner_session(
            Role::Replan,
            None,
            &format!(
                "No task is ready. Blocked or unreachable: {}. Route the work around the blocked tasks.",
                open.join(", ")
            ),
        )
        .await?;
        Ok(())
    }
}
