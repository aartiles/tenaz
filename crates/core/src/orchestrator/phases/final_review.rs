//! The final phase: a review of the whole product against the spec, which
//! completes the run, adds tasks for the loop, or runs again.

use crate::backlog::Backlog;
use crate::ledger::{JournalRecord, Phase, Verdict};
use crate::orchestrator::session::SessionCtx;
use crate::orchestrator::{RunError, Runner};
use crate::role::Role;
use crate::spec::{Coverage, coverage};
use crate::task::TaskId;
use crate::verify::VerifyOutcome;

use super::PromptParts;

/// Where the final reviewer writes its review.
const FINAL_REVIEW: &str = ".tenaz/reviews/FINAL.md";

impl Runner {
    pub(crate) async fn final_phase(&mut self) -> Result<(), RunError> {
        let tasks = self.project.load_backlog().backlog;
        let all = self.run_verify(&["--all"]).await;
        let cov = coverage(&self.project.requirements(), tasks.cards(), true);
        self.final_review_session(&tasks, &all, &cov).await?;
        let (verdict, _) = self.project.verdict(FINAL_REVIEW);
        let added = self.reset_added(&tasks)?;
        self.ledger.final_cycles += 1;
        self.deps
            .vcs
            .commit_all(&format!("tenaz: final review {}", self.ledger.final_cycles))?;
        let outcome = final_outcome(
            verdict == Some(Verdict::Accept),
            // The suite passes and every non-manual requirement is covered by a done task.
            all.ok && cov.missing.is_empty(),
            added.len(),
            self.ledger.final_cycles < self.params.max_final_cycles,
        );
        let added_count = added.len();
        self.journal(JournalRecord::FinalReview {
            cycle: self.ledger.final_cycles,
            verdict,
            verify_ok: all.ok,
            coverage: cov,
            added,
        });
        match outcome {
            FinalOutcome::Complete => self.finish(Phase::Complete),
            FinalOutcome::Gaps => self.finish(Phase::CompleteWithGaps),
            FinalOutcome::Loop => {
                self.ledger.phase = Phase::Loop;
                self.save()?;
                self.log(&format!("Final review added {added_count} task(s)"));
                Ok(())
            }
            FinalOutcome::Retry => {
                self.log("Final review did not produce a verdict or new tasks; retrying");
                Ok(())
            }
        }
    }

    /// The final reviewer's session, given the task list, the coverage
    /// and the last `./verify --all`.
    async fn final_review_session(
        &mut self,
        tasks: &Backlog,
        all: &VerifyOutcome,
        cov: &Coverage,
    ) -> Result<(), RunError> {
        self.project.remove(FINAL_REVIEW)?;
        let fields = vec![(
            "Cycle".to_owned(),
            (self.ledger.final_cycles + 1).to_string(),
        )];
        let extra = vec![
            ("Task list".to_owned(), tasks.table()),
            ("Coverage".to_owned(), coverage_report(cov)),
            ("./verify --all".to_owned(), all.summary.clone()),
        ];
        let prompt = self.briefing(
            Role::FinalReviewer,
            PromptParts {
                fields,
                extra,
                ..PromptParts::default()
            },
        )?;
        self.session(
            Role::FinalReviewer,
            &prompt,
            &SessionCtx {
                turns: Some(60),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    /// The tasks the final reviewer added to `before`, each put back to
    /// `todo` with no attempts.
    fn reset_added(&self, before: &Backlog) -> Result<Vec<TaskId>, RunError> {
        let after = self.project.load_backlog().backlog;
        let added: Vec<TaskId> = after
            .ids()
            .filter(|id| !before.contains(id))
            .cloned()
            .collect();
        for id in &added {
            self.project.reset_card(id)?;
        }
        Ok(added)
    }
}

/// What a final review leads to.
#[derive(Debug, PartialEq)]
enum FinalOutcome {
    /// The run is complete.
    Complete,
    /// The loop builds the tasks the review added.
    Loop,
    /// Out of final cycles: the run is complete with gaps.
    Gaps,
    /// Another final review.
    Retry,
}

/// The outcome of a final review that `accepted` (or not) a product that is
/// `product_ok` (or not), and `added` tasks, with `cycles_left` (or not).
fn final_outcome(
    accepted: bool,
    product_ok: bool,
    added: usize,
    cycles_left: bool,
) -> FinalOutcome {
    if accepted && product_ok {
        FinalOutcome::Complete
    } else if !cycles_left {
        FinalOutcome::Gaps
    } else if added > 0 {
        FinalOutcome::Loop
    } else {
        FinalOutcome::Retry
    }
}

/// The coverage line the final reviewer is given.
fn coverage_report(cov: &Coverage) -> String {
    let list = |ids: &[String]| {
        if ids.is_empty() {
            "none".to_owned()
        } else {
            ids.join(", ")
        }
    };
    format!(
        "Requirements: {}. Manual: {}. Not covered by a done task: {}.",
        cov.total,
        list(&cov.manual),
        list(&cov.missing)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_coverage_report_joins_ids_and_says_none_for_an_empty_list() {
        let cov = Coverage {
            total: 3,
            manual: Vec::new(),
            missing: vec!["R2".to_owned(), "R3".to_owned()],
        };
        assert_eq!(
            coverage_report(&cov),
            "Requirements: 3. Manual: none. Not covered by a done task: R2, R3."
        );
    }

    #[test]
    fn an_accepted_review_of_a_passing_product_completes_the_run() {
        assert_eq!(final_outcome(true, true, 2, false), FinalOutcome::Complete);
    }

    #[test]
    fn an_accepted_review_of_a_failing_product_is_not_complete() {
        assert_eq!(final_outcome(true, false, 0, true), FinalOutcome::Retry);
    }

    #[test]
    fn a_rejecting_review_that_adds_nothing_runs_again_while_cycles_are_left() {
        assert_eq!(final_outcome(false, true, 0, true), FinalOutcome::Retry);
    }

    #[test]
    fn added_tasks_go_back_to_the_loop_while_cycles_are_left() {
        assert_eq!(final_outcome(false, true, 2, true), FinalOutcome::Loop);
    }

    #[test]
    fn out_of_cycles_the_run_completes_with_gaps() {
        assert_eq!(final_outcome(false, true, 2, false), FinalOutcome::Gaps);
    }
}
