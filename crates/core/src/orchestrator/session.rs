//! A session with its recovery: the wrapper that reruns it after engine
//! failures, capacity caps and context stops without charging the task an
//! attempt, or pauses it. One session, start to journal record, is in
//! `record`; the harness run itself in `harness`; its requests in
//! `requests`; what the run says when it reruns or stops one in `reasons`.

use std::time::Duration;

use crate::backlog::Backlog;
use crate::briefing::without_map;
use crate::lease::{LeaseError, LeaseManager};
use crate::ledger::{JournalRecord, SessionRecord};
use crate::ownership::builder_reverted_note;
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::task::TaskId;

use super::recovery::{Continuation, FailureCause, GiveUp, Recovery, Step};
use super::{RunError, Runner};

mod harness;
mod reasons;
mod record;
mod requests;

pub(crate) use harness::HarnessRun;
#[cfg(any(test, feature = "testing"))]
pub(crate) use harness::{INVALID_CARDS, PASSING_CONFLICT, REFUSED_REOPENS};
#[cfg(test)]
pub(crate) use harness::{INVALID_CARDS_NOTE, PASSING_CONFLICT_NOTE, REFUSED_REOPENS_NOTE};
use reasons::{PromptLimit, give_up_reason, retry_notice, retry_reason};

/// macOS memory pressure level `critical`: no time to raise the budget.
const CRITICAL_PRESSURE: u32 = 4;

/// What a session is about.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionCtx {
    pub task: Option<TaskId>,
    pub attempt: Option<u32>,
    /// A card's turn budget; the run's default otherwise.
    pub turns: Option<u32>,
    /// A card's token budget; the run's default otherwise.
    pub context_tokens: Option<u64>,
    /// A split's or replan's backlog from before it. Cards Tenaz would
    /// throw away, and a blocked task a replan sets back to `todo` that
    /// Tenaz would block again, are pointed out before the session ends,
    /// once each per harness session: a continuation or a retry of the
    /// session is asked again.
    pub planner_before: Option<Backlog>,
}

/// A finished session: its journal record plus the raw metrics.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionResult {
    pub record: SessionRecord,
    pub metrics: SessionMetrics,
}

impl Runner {
    /// A session with the task loop's recovery around it: continuations
    /// after context stops and refused images, capacity caps, and engine
    /// retries with backoff. None of these charge an attempt. A continuation
    /// a pause left is owed until the session is done.
    pub(crate) async fn session(
        &mut self,
        role: Role,
        prompt: &str,
        ctx: &SessionCtx,
    ) -> Result<SessionResult, RunError> {
        let mut recovery = Recovery::resuming(self.ledger.pending_continuation);
        // The last try of this session, which a stop during the next
        // swap pauses at, as a stop after it would.
        let mut last: Option<String> = None;
        loop {
            let text = self.with_reverted_note(role, session_prompt(prompt, &recovery));
            self.check_prompt_fits(role, &text, ctx)?;
            self.swap_for(role, last.as_deref(), ctx).await?;
            let result = self.session_once(role, &text, ctx).await?;
            let label = result.record.label.clone();
            last = Some(label.clone());
            if self.stopping() {
                return Err(self.pause(&label, role, ctx)?);
            }
            if !result.metrics.engine_failed() {
                self.note_engine_held(&result.record)?;
            }
            match recovery.next(&result.metrics, role, &self.params) {
                Step::Done => {
                    self.clear_continuation()?;
                    return Ok(result);
                }
                Step::Continue { kind, count } => self.continue_session(&result, kind, count),
                Step::LowerCap { limit, count } => {
                    self.lower_context_cap(&label, limit, count)?;
                    self.capacity_wait(&label, role, ctx).await?;
                }
                Step::Retry { retry, cause } => {
                    self.engine_backoff(&label, retry, cause).await;
                    if self.stopping() {
                        return Err(self.pause(&label, role, ctx)?);
                    }
                    // The next session meets the engine as it is: a failure
                    // there counts toward the same retries.
                    let _ = self.wait_for_engine("after an engine error").await;
                }
                Step::PromptRefused => return Err(self.prompt_refused(&result.record, role)),
                Step::GiveUp(why) => return Err(self.give_up(&label, why)),
            }
        }
    }

    /// A builder's `prompt`, with what earlier builder sessions of its task
    /// had reverted, so it does not write there again. Added to every
    /// session, so a continuation within an attempt is told too.
    fn with_reverted_note(&self, role: Role, prompt: String) -> String {
        let reverted = self
            .ledger
            .current
            .as_ref()
            .map_or(&[][..], |c| c.reverted.as_slice());
        if role != Role::Builder || reverted.is_empty() {
            return prompt;
        }
        format!("{prompt}\n\n{}", builder_reverted_note(reverted))
    }

    /// Journals and logs that a fresh session continues the work of `result`.
    fn continue_session(&mut self, result: &SessionResult, kind: Continuation, count: u32) {
        let label = result.record.label.clone();
        let limit = self.params.context_continuations;
        let why = match kind {
            Continuation::Context => {
                self.journal(JournalRecord::ContextContinuation {
                    label: label.clone(),
                    context_peak: result.record.context_peak,
                    context_budget: result.record.context_budget,
                    continuation: count,
                });
                if result.metrics.compaction_stopped() {
                    "stopped at its second context compaction"
                } else {
                    "stopped at its context budget"
                }
            }
            Continuation::Image(refusal) => {
                self.journal(JournalRecord::ImageContinuation {
                    label: label.clone(),
                    refusal: refusal.chars().take(300).collect(),
                    continuation: count,
                });
                "the engine refused an image the session read"
            }
            Continuation::Refused(refusal) => {
                self.journal(JournalRecord::RefusedContinuation {
                    label: label.clone(),
                    refusal: refusal.chars().take(300).collect(),
                    continuation: count,
                });
                "the engine refused a request of the session"
            }
            Continuation::Turns => {
                self.journal(JournalRecord::TurnContinuation {
                    label: label.clone(),
                    turns: result.metrics.turns,
                    continuation: count,
                });
                "stopped at its turn budget"
            }
        };
        self.log(&format!("  {label}: {why}; a fresh session continues the same work ({count}/{limit}, not counted as an attempt)"));
    }

    /// Stops the run before a session of `role` whose `prompt`, with the
    /// harness's reserve, is estimated over its model's budget: the engine
    /// or the harness would refuse it, or leave the session no room to
    /// work (ADR 0012).
    fn check_prompt_fits(
        &self,
        role: Role,
        prompt: &str,
        ctx: &SessionCtx,
    ) -> Result<(), RunError> {
        match self.prompt_limit(role).over(prompt, ctx.task.as_ref()) {
            Some(reason) => Err(RunError::PromptTooLarge(reason)),
            None => Ok(()),
        }
    }

    /// Before each try of a session of `role`, in a run that swaps its
    /// models: makes the role's model the only one loaded (ADR 0015). A
    /// swap that fails is waited for as after an engine error, and one the
    /// engine never takes blocks the run. A stop before, during or after
    /// the swap pauses the run before the session: at `last`, the earlier
    /// try of this session, as a stop after it would, else at the swap.
    async fn swap_for(
        &mut self,
        role: Role,
        last: Option<&str>,
        ctx: &SessionCtx,
    ) -> Result<(), RunError> {
        let Some(lease) = self.deps.lease.clone().filter(|l| l.swaps()) else {
            return Ok(());
        };
        let model = self.ledger.config.model_for(role).id;
        let served = if self.stopping() {
            Ok(())
        } else {
            self.serve(&lease, role, &model).await
        };
        if self.stopping() {
            return Err(match last {
                Some(label) => self.pause(label, role, ctx)?,
                None => self.record_pause(&format!("the swap to {model}"), role, ctx)?,
            });
        }
        served
    }

    /// Serves `model` alone through `lease`, waiting for the engine when
    /// that fails; engine unavailable when it never serves.
    async fn serve(&self, lease: &LeaseManager, role: Role, model: &str) -> Result<(), RunError> {
        let error = match lease.serve_only(model).await {
            Ok(()) => return Ok(()),
            // Cannot happen while the lease is built from the run's models;
            // the wait below would serve another model.
            Err(LeaseError::UnknownLlm(id)) => {
                return Err(RunError::Precondition(format!(
                    "the {role} session's model {id} is not one the lease swaps"
                )));
            }
            Err(error) => error,
        };
        self.log(&format!("  could not swap to {model}: {error}"));
        let why = format!("for the {role} session's model {model}");
        match self.wait_for_engine(&why).await {
            None => Ok(()),
            Some(error) => Err(RunError::EngineUnavailable(format!(
                "{model} did not load for the {role} session: {error}"
            ))),
        }
    }

    /// The error that ends the run when the harness refused `record`'s prompt.
    fn prompt_refused(&self, record: &SessionRecord, role: Role) -> RunError {
        RunError::PromptTooLarge(
            self.prompt_limit(role)
                .refused(&record.label, record.prompt_chars),
        )
    }

    /// What a `role` prompt must fit: its model's budget, with the
    /// harness's reserve.
    fn prompt_limit(&self, role: Role) -> PromptLimit {
        PromptLimit {
            role,
            model: self.ledger.config.model_for(role),
            budget: self.ledger.model_budget_tokens(role),
            reserve: self.params.harness_reserve_tokens,
        }
    }

    /// The error that ends the run when a session keeps failing in the engine.
    fn give_up(&self, label: &str, why: GiveUp) -> RunError {
        RunError::EngineUnavailable(give_up_reason(label, why, self.ledger.context_cap))
    }

    /// Records a pause during `label` and returns the error that unwinds the run.
    fn pause(&mut self, label: &str, role: Role, ctx: &SessionCtx) -> Result<RunError, RunError> {
        self.ledger.pending_continuation = true;
        self.record_pause(label, role, ctx)
    }

    /// Saves that no continuation is owed, once a session is done. Not
    /// before: the reruns of a session that continues a paused one get the
    /// note too, a block leaves it to `--resume`, and an error to the next
    /// run. Taking back a paused review clears it too (`take_in_review`).
    fn clear_continuation(&mut self) -> Result<(), RunError> {
        if std::mem::take(&mut self.ledger.pending_continuation) {
            self.save()?;
        }
        Ok(())
    }

    /// Saves and journals a pause at `label`, without telling the next
    /// session it continues a stopped one.
    pub(super) fn record_pause(
        &mut self,
        label: &str,
        role: Role,
        ctx: &SessionCtx,
    ) -> Result<RunError, RunError> {
        self.ledger.paused_at = Some(self.now());
        self.save()?;
        self.journal(JournalRecord::Paused {
            label: Some(label.to_owned()),
            role: Some(role),
            task: ctx.task.clone(),
            attempt: ctx.attempt,
        });
        Ok(RunError::Paused(label.to_owned()))
    }

    /// Waits `capacityRetryWaitSec` before a session is rerun at a lowered
    /// budget; a stop during the wait pauses the run instead.
    async fn capacity_wait(
        &mut self,
        label: &str,
        role: Role,
        ctx: &SessionCtx,
    ) -> Result<(), RunError> {
        self.sleep_unless_stopped(self.params.capacity_retry_wait())
            .await;
        if self.stopping() {
            return Err(self.pause(label, role, ctx)?);
        }
        Ok(())
    }

    /// Lowers the run's context budget to a share of what the engine said it can hold.
    fn lower_context_cap(&mut self, label: &str, limit: u64, retry: u32) -> Result<(), RunError> {
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a context limit times a fraction is far below 2^53 and floors to a positive u64"
        )]
        let cap = (limit as f64 * self.params.capacity_headroom).floor() as u64;
        let current = self.ledger.lower_cap(cap);
        self.save()?;
        self.journal(JournalRecord::ContextCap {
            label: label.to_owned(),
            engine_max_context: limit,
            context_cap: current,
            retry,
        });
        self.log(&format!("  {label}: the engine can hold about {limit} tokens now; context budget lowered to {current}; rerunning the same session ({retry}/{}, not counted as an attempt)", self.params.capacity_retries));
        Ok(())
    }

    /// Counts a session the engine held at a lowered budget toward a raise
    /// (`Ledger::note_cap_held`, ADR 0014), and journals the raise when one
    /// is due. A session whose budget was under the cap does not count: it
    /// did not test it. Critical memory pressure starts the count again. The
    /// engine only says how much it can hold when a prompt overflows it, so
    /// memory freed later is found by trying.
    fn note_engine_held(&mut self, session: &SessionRecord) -> Result<(), RunError> {
        let Some(cap) = self.ledger.context_cap else {
            return Ok(());
        };
        let label = session.label.as_str();
        if session
            .memory
            .as_ref()
            .is_some_and(|m| m.pressure_max >= CRITICAL_PRESSURE)
        {
            self.ledger.note_cap_pressed();
            return self.save();
        }
        if self.note_cap_overrun(session)? || self.params.capacity_recover_sessions == 0 {
            return Ok(());
        }
        if session.context_budget < cap {
            return Ok(());
        }
        let Some(raise) = self
            .ledger
            .note_cap_held(self.params.capacity_recover_sessions)
        else {
            return self.save();
        };
        self.save()?;
        self.journal(JournalRecord::ContextCapRaised {
            label: label.to_owned(),
            context_cap: raise.cap,
            clean_sessions: raise.clean_sessions,
        });
        let budget = raise
            .cap
            .unwrap_or_else(|| self.ledger.largest_budget_tokens());
        let past = raise
            .past_ceiling
            .map_or_else(String::new, |c| format!(", past the ceiling of {c}"));
        self.log(&format!("  {label}: {} sessions in a row without the engine running out of memory; context budget raised to {budget}{past}", raise.clean_sessions));
        Ok(())
    }

    /// Lifts the cap to the headroom share of a peak the engine held past
    /// it (`Ledger::note_cap_overrun`, ADR 0026). True when it did: the
    /// session did not test the new cap, so it does not count toward a raise.
    fn note_cap_overrun(&mut self, session: &SessionRecord) -> Result<bool, RunError> {
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a context peak times a fraction is far below 2^53 and floors to a positive u64"
        )]
        let held = (session.context_peak as f64 * self.params.capacity_headroom).floor() as u64;
        let Some(cap) = self.ledger.note_cap_overrun(held) else {
            return Ok(false);
        };
        self.save()?;
        let label = session.label.as_str();
        self.journal(JournalRecord::ContextCapOverrun {
            label: label.to_owned(),
            context_peak: session.context_peak,
            context_cap: cap,
        });
        let budget = cap.unwrap_or_else(|| self.ledger.largest_budget_tokens());
        self.log(&format!(
            "  {label}: the engine held {} tokens with no error; context budget raised to {budget}",
            session.context_peak
        ));
        Ok(true)
    }

    /// Waits before rerunning a session the engine failed: the base backoff,
    /// doubling per retry up to 15 minutes, cut short by a pause.
    async fn engine_backoff(&mut self, label: &str, retry: u32, cause: FailureCause) {
        let wait = (self.params.engine_retry_base_sec * 2u64.pow(retry)).min(900);
        self.journal(JournalRecord::EngineRetry {
            label: label.to_owned(),
            retry: retry + 1,
            wait_s: wait,
            cause: Some(cause.as_str().to_owned()),
        });
        let what = retry_reason(cause);
        self.log(&format!("  {label}: {what}; retrying the same session in {wait} s ({}/{}, not counted as an attempt)", retry + 1, self.params.engine_retries));
        if retry == 0
            && let Some(notice) = retry_notice(cause)
        {
            self.notify(&notice);
        }
        self.sleep_unless_stopped(Duration::from_secs(wait)).await;
    }

    /// Sleeps `wait`, or less when the run is stopped.
    async fn sleep_unless_stopped(&self, wait: Duration) {
        tokio::select! {
            () = self.deps.clock.sleep(wait) => {}
            () = self.stop.cancelled() => {}
        }
    }
}

/// The prompt of a session: `prompt`, with the note a session that
/// continues earlier work gets, and the one a builder rerun after the
/// engine ran out of memory gets.
fn session_prompt(prompt: &str, recovery: &Recovery) -> String {
    let mut text = if recovery.carries_note() {
        format!("{}\n\n{}", without_map(prompt), recovery.note())
    } else {
        prompt.to_owned()
    };
    if let Some(note) = recovery.memory_note() {
        text = format!("{text}\n\n{note}");
    }
    text
}
