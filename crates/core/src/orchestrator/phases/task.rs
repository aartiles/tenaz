//! One task: builder attempts judged by `verify`, then review, then done;
//! or, out of attempts or of context, escalated.

use crate::backlog::Backlog;
use crate::briefing;
use crate::ledger::{CurrentTask, JournalRecord, StoppedAttempt, TaskOutcome};
use crate::orchestrator::session::{SessionCtx, SessionResult};
use crate::orchestrator::{RunError, Runner};
use crate::role::Role;
use crate::task::{TaskCard, TaskId, TaskStatus};
use crate::util::clip;
use crate::verify::{VerifyOutcome, judge_attempt, with_stop_reason};

use super::PromptParts;
use super::review::{Review, ReviewInput};

/// A task card as an attempt loads it: parsed, and its text.
struct LoadedCard {
    card: TaskCard,
    text: String,
}

/// A builder attempt that passed `verify`.
struct Built {
    loaded: LoadedCard,
    all: VerifyOutcome,
    own: VerifyOutcome,
    /// Why the orchestrator stopped the builder, when its work still passed.
    stopped: Option<String>,
}

/// The work of an attempt, to be judged by `verify`.
struct Work {
    loaded: LoadedCard,
    /// Why the orchestrator stopped the builder, if it did.
    stopped: Option<String>,
    /// The builder ran out of context with no continuation left.
    outgrown: bool,
}

/// How a builder attempt ended.
enum Attempt {
    Passed(Box<Built>),
    /// `outgrown`: its session used up the context continuations.
    Failed {
        failure: String,
        outgrown: bool,
    },
}

/// What a passed attempt comes to after review.
enum Reviewed {
    /// The task is done; `reviewed` when a reviewer accepted it, not
    /// `verify` alone.
    Done { reviewed: bool },
    /// The reviewer asked for changes: another attempt follows.
    Changes,
}

impl Runner {
    /// Builds one task: attempts, verify, review, done; or escalate.
    pub(super) async fn run_task(
        &mut self,
        backlog: &Backlog,
        task: &TaskCard,
    ) -> Result<(), RunError> {
        self.start_task(task)?;
        loop {
            let Some((attempt, outcome)) = self.next_attempt(backlog, task).await? else {
                let why = format!("{} failed {} attempts.", task.id, self.params.max_attempts);
                return self.escalate_current(task, &why).await;
            };
            match outcome {
                Attempt::Failed { failure, outgrown } => {
                    self.record_failure(attempt, failure)?;
                    // Another attempt would meet the same budget: the task is too big for it.
                    if outgrown && self.splits_when_outgrown(&task.id) {
                        let why = format!(
                            "{} ran out of context: attempt {attempt} used all {} context continuations and still fails verify.",
                            task.id, self.params.context_continuations
                        );
                        self.log(&format!("  {why} Splitting it instead of retrying."));
                        return self.escalate_current(task, &why).await;
                    }
                }
                Attempt::Passed(built) => {
                    let reviewed = match self.review_attempt(task, attempt, &built).await? {
                        Reviewed::Done { reviewed } => reviewed,
                        Reviewed::Changes => continue,
                    };
                    // The floor only goes up.
                    self.ledger.passed_checks = Some(
                        built
                            .all
                            .passed()
                            .max(self.ledger.passed_checks.unwrap_or(0)),
                    );
                    return self.complete_task(task, attempt, reviewed);
                }
            }
        }
    }

    /// The next attempt and how it ended: the one whose review was cut
    /// off, judged again, or a new build. `None` when none is left.
    async fn next_attempt(
        &mut self,
        backlog: &Backlog,
        task: &TaskCard,
    ) -> Result<Option<(u32, Attempt)>, RunError> {
        let attempts = self
            .project
            .load_backlog()
            .backlog
            .get(&task.id)
            .map_or(0, |t| t.attempts);
        if self.take_in_review()? == Some(attempts) {
            return Ok(Some((attempts, self.judge_again(task, attempts).await?)));
        }
        if attempts >= self.params.max_attempts {
            return Ok(None);
        }
        let attempt = attempts + 1;
        Ok(Some((
            attempt,
            self.build_attempt(backlog, task, attempt).await?,
        )))
    }

    /// Marks `task` active and records where it starts from, unless the
    /// run is already on it (a resume).
    fn start_task(&mut self, task: &TaskCard) -> Result<(), RunError> {
        if self
            .ledger
            .current
            .as_ref()
            .is_some_and(|c| c.task == task.id)
        {
            return Ok(());
        }
        self.project.set_status(&task.id, TaskStatus::Active)?;
        let base = self
            .deps
            .vcs
            .commit_all(&format!("tenaz: start {}", task.id))?;
        self.ledger.current = Some(CurrentTask {
            task: task.id.clone(),
            base,
            history: Vec::new(),
            failure: None,
            stopped: Vec::new(),
            in_review: None,
        });
        self.save()
    }

    /// Whether a task whose attempt used up its context continuations is
    /// split instead of retried.
    fn splits_when_outgrown(&self, id: &TaskId) -> bool {
        self.params.context_split && id.split_depth() < self.params.max_split_depth
    }

    /// Splits or blocks `task` with the history of its attempts, and
    /// leaves it.
    async fn escalate_current(&mut self, task: &TaskCard, why: &str) -> Result<(), RunError> {
        let history = self
            .ledger
            .current
            .as_ref()
            .map(|c| c.history.clone())
            .unwrap_or_default();
        self.escalate(task, history, why).await?;
        self.ledger.current = None;
        self.save()
    }

    /// Records a failed attempt: the next one is told its failure.
    fn record_failure(&mut self, attempt: u32, failure: String) -> Result<(), RunError> {
        if let Some(current) = &mut self.ledger.current {
            current
                .history
                .push(format!("Attempt {attempt}: {}", clip(&failure, 1500)));
            current.failure = Some(failure);
        }
        self.save()
    }

    /// Commits a passed attempt and, when the run reviews, has it reviewed.
    async fn review_attempt(
        &mut self,
        task: &TaskCard,
        attempt: u32,
        built: &Built,
    ) -> Result<Reviewed, RunError> {
        self.deps.vcs.commit_all(&format!(
            "task {}: {} (attempt {attempt})",
            task.id, built.loaded.card.title
        ))?;
        if !self.params.review {
            return Ok(Reviewed::Done { reviewed: false });
        }
        let base = self.current_base();
        let review = ReviewInput {
            card: &built.loaded.card,
            card_text: &built.loaded.text,
            base: &base,
            attempt,
            all: &built.all,
            own: &built.own,
            stopped: built.stopped.as_deref(),
        };
        self.set_in_review(Some(attempt))?;
        let outcome = self.review_task(&review).await?;
        self.set_in_review(None)?;
        Ok(match outcome {
            Review::Changes(text) => {
                self.request_changes(task, attempt, &text)?;
                Reviewed::Changes
            }
            Review::NoVerdict => {
                self.log(&format!(
                    "  reviewer left no verdict for {}; accepting on verify alone",
                    task.id
                ));
                Reviewed::Done { reviewed: false }
            }
            Review::Accept => Reviewed::Done { reviewed: true },
        })
    }

    /// Records the attempt under review; a review cut off leaves it set.
    fn set_in_review(&mut self, attempt: Option<u32>) -> Result<(), RunError> {
        if let Some(current) = &mut self.ledger.current {
            current.in_review = attempt;
        }
        self.save()
    }

    /// Records the changes the reviewer asked for as the failure the next
    /// attempt is told.
    fn request_changes(
        &mut self,
        task: &TaskCard,
        attempt: u32,
        text: &str,
    ) -> Result<(), RunError> {
        if let Some(current) = &mut self.ledger.current {
            current.failure = Some(format!(
                "The reviewer requested changes:\n{}",
                clip(text, 3000)
            ));
            current.history.push(format!(
                "Attempt {attempt}: review requested changes:\n{}",
                clip(text, 1500)
            ));
        }
        self.deps
            .vcs
            .commit_all(&format!("tenaz: review {} attempt {attempt}", task.id))?;
        self.save()
    }

    /// One builder session and its judgement by `verify`.
    async fn build_attempt(
        &mut self,
        backlog: &Backlog,
        task: &TaskCard,
        attempt: u32,
    ) -> Result<Attempt, RunError> {
        self.project.set_attempts(&task.id, attempt)?;
        let card = self.load_card(&task.id)?;
        let prompt = self.builder_prompt(backlog, &card, attempt).await?;
        let s = self.builder_session(&card.card, attempt, &prompt).await?;
        let work = Work {
            loaded: card,
            stopped: s.metrics.stopped(),
            outgrown: s.metrics.context_stopped(),
        };
        self.judge(task, attempt, work).await
    }

    /// Takes the attempt whose review was cut off, if any. The session cut
    /// off was a reviewer, which nothing continues: a pause's continuation
    /// note goes with it.
    fn take_in_review(&mut self) -> Result<Option<u32>, RunError> {
        let taken = self
            .ledger
            .current
            .as_mut()
            .and_then(|c| c.in_review.take());
        if taken.is_some() {
            self.ledger.pending_continuation = false;
            self.save()?;
        }
        Ok(taken)
    }

    /// The attempt whose review was cut off: its work is judged again and,
    /// if it still passes, reviewed from the start, without another attempt.
    async fn judge_again(&mut self, task: &TaskCard, attempt: u32) -> Result<Attempt, RunError> {
        self.log(&format!(
            "  {} attempt {attempt} was cut off in review; reviewing it again",
            task.id
        ));
        let stopped = self.ledger.current.as_ref().and_then(|c| {
            c.stopped
                .iter()
                .find(|s| s.attempt == attempt)
                .map(|s| s.reason.clone())
        });
        let work = Work {
            loaded: self.load_card(&task.id)?,
            stopped,
            outgrown: false,
        };
        self.judge(task, attempt, work).await
    }

    /// `verify` on the work of `attempt`, journaled: passed, or failed and why.
    async fn judge(
        &mut self,
        task: &TaskCard,
        attempt: u32,
        work: Work,
    ) -> Result<Attempt, RunError> {
        let Work {
            loaded,
            stopped,
            outgrown,
        } = work;
        let all = self.run_verify(&["--all"]).await;
        let own = self.run_verify(&["--task", task.id.as_str()]).await;
        let failure = with_stop_reason(
            judge_attempt(task.id.as_str(), &all, &own, self.ledger.passed_checks),
            stopped.as_deref(),
        );
        if let (Some(stopped), None) = (&stopped, &failure) {
            self.record_stopped(attempt, stopped);
        }
        self.journal(JournalRecord::Verify {
            task: task.id.clone(),
            attempt,
            all_ok: all.ok,
            task_ok: own.ok,
            task_checks: own.passed(),
            all: all.json.clone(),
            task_json: own.json.clone(),
        });
        Ok(match failure {
            Some(failure) => Attempt::Failed { failure, outgrown },
            None => Attempt::Passed(Box::new(Built {
                loaded,
                all,
                own,
                stopped,
            })),
        })
    }

    /// The card of `id` as it is now on disk.
    fn load_card(&self, id: &TaskId) -> Result<LoadedCard, RunError> {
        let card = self
            .project
            .load_backlog()
            .backlog
            .get(id)
            .cloned()
            .ok_or_else(|| RunError::Precondition(format!("card {id} disappeared")))?;
        let text = self.project.card_text(id)?;
        Ok(LoadedCard { card, text })
    }

    /// The builder's session. A paused session, or one cut short by the
    /// engine, gives its attempt back: it is not a failed one.
    async fn builder_session(
        &mut self,
        card: &TaskCard,
        attempt: u32,
        prompt: &str,
    ) -> Result<SessionResult, RunError> {
        let ctx = SessionCtx {
            task: Some(card.id.clone()),
            attempt: Some(attempt),
            turns: card.budget.turns,
            context_tokens: card.budget.tokens,
        };
        let result = self.session(Role::Builder, prompt, &ctx).await;
        if let Err(RunError::Paused(_) | RunError::EngineUnavailable(_)) = &result {
            self.project.set_attempts(&card.id, attempt - 1)?;
        }
        result
    }

    /// A stopped session whose work still passes goes on to review, but is
    /// recorded as such.
    fn record_stopped(&mut self, attempt: u32, reason: &str) {
        if let Some(current) = &mut self.ledger.current
            && !current.stopped.iter().any(|s| s.attempt == attempt)
        {
            current.stopped.push(StoppedAttempt {
                attempt,
                reason: reason.to_owned(),
            });
        }
    }

    /// Marks a task done, commits the checkpoint and journals it.
    fn complete_task(
        &mut self,
        task: &TaskCard,
        attempt: u32,
        reviewed: bool,
    ) -> Result<(), RunError> {
        self.checkpoint_task(
            &task.id,
            TaskStatus::Done,
            &format!("tenaz: {} done", task.id),
        )?;
        let stopped_sessions = self
            .ledger
            .current
            .take()
            .map(|c| c.stopped)
            .unwrap_or_default();
        self.journal(JournalRecord::Task {
            task: task.id.clone(),
            outcome: TaskOutcome::Done,
            children: None,
            history: None,
            attempts: Some(attempt),
            reviewed: Some(reviewed),
            stopped_sessions: Some(stopped_sessions.clone()),
        });
        self.ledger.done_since_replan += 1;
        self.ledger.stuck_replanned = false;
        self.save()?;
        let plural = |n: usize, word: &str| {
            if n == 1 {
                word.to_owned()
            } else {
                format!("{word}s")
            }
        };
        let stopped_note = if stopped_sessions.is_empty() {
            String::new()
        } else {
            format!(
                "; {} stopped builder {} passed verify",
                stopped_sessions.len(),
                plural(stopped_sessions.len(), "session")
            )
        };
        self.log(&format!(
            "{} done ({attempt} {}{stopped_note})",
            task.id,
            plural(attempt as usize, "attempt")
        ));
        Ok(())
    }

    /// The builder's briefing: notes from the dependencies and from the task
    /// itself, the previous failure and what it left, and the latest
    /// `verify` result.
    async fn builder_prompt(
        &self,
        backlog: &Backlog,
        loaded: &LoadedCard,
        attempt: u32,
    ) -> Result<String, RunError> {
        let card = &loaded.card;
        let mut extra = self.builder_notes(backlog, card);
        extra.extend(self.previous_failure()?);
        extra.push((
            "Latest ./verify --all".to_owned(),
            self.run_verify(&["--all"]).await.summary,
        ));
        let fields = vec![
            ("Task".to_owned(), card.id.to_string()),
            (
                "Attempt".to_owned(),
                format!("{attempt} of {}", self.params.max_attempts),
            ),
        ];
        let cites: &[String] = if self.params.cited_spec {
            &card.requires
        } else {
            &[]
        };
        self.briefing(
            Role::Builder,
            PromptParts {
                fields,
                card: Some((&card.file, &loaded.text)),
                extra,
                cites,
            },
        )
    }

    /// Notes from the task's dependencies (and the tasks they were split
    /// into), and the task's own notes from earlier attempts.
    fn builder_notes(&self, backlog: &Backlog, card: &TaskCard) -> Vec<(String, String)> {
        let mut extra = Vec::new();
        let mut note_ids: Vec<String> = vec!["BOOTSTRAP".to_owned()];
        for dep in &card.depends {
            note_ids.push(dep.to_string());
            note_ids.extend(
                backlog
                    .children(dep)
                    .iter()
                    .map(|child| child.id.to_string()),
            );
        }
        let notes = briefing::dependency_notes(
            &note_ids,
            |id| self.project.note(id),
            self.params.note_clip_bytes,
        );
        if !notes.is_empty() && self.params.briefing {
            extra.push(("Notes from earlier tasks".to_owned(), notes));
        }
        if let Some(own) = self.project.note(card.id.as_str()) {
            extra.push((
                "Your notes from earlier attempts".to_owned(),
                clip(own.trim(), self.params.note_clip_bytes),
            ));
        }
        extra
    }

    /// Why the previous attempt failed, and what the task's attempts left.
    fn previous_failure(&self) -> Result<Vec<(String, String)>, RunError> {
        let Some(failure) = self.ledger.current.as_ref().and_then(|c| c.failure.clone()) else {
            return Ok(Vec::new());
        };
        let mut extra = vec![("Previous attempt failed".to_owned(), failure)];
        let diff = self.deps.vcs.diff_stat(&self.current_base(), None, true)?;
        if !diff.trim().is_empty() {
            extra.push((
                "Changes already made since the task started (kept)".to_owned(),
                diff.trim().to_owned(),
            ));
        }
        Ok(extra)
    }
}
