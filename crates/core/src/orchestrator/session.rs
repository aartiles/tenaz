//! A session with its recovery: the wrapper that reruns it after engine
//! failures, capacity caps and context stops without charging the task an
//! attempt, or pauses it. One session, start to journal record, is in
//! `record`; the harness run itself in `harness`; its requests in
//! `requests`.

use std::time::Duration;

use crate::briefing::without_map;
use crate::ledger::{JournalRecord, SessionRecord};
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::task::TaskId;

use super::recovery::{Continuation, FailureCause, GiveUp, Recovery, Step};
use super::{RunError, Runner};

mod harness;
mod record;
mod requests;

pub(crate) use harness::HarnessRun;

/// macOS memory pressure level `critical`: no time to raise the budget.
const CRITICAL_PRESSURE: u32 = 4;
/// A cap this close to the largest configured budget is lifted rather than
/// raised.
const RAISE_STEP_MIN: u64 = 4096;

/// What a session is about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionCtx {
    pub task: Option<TaskId>,
    pub attempt: Option<u32>,
    /// A card's turn budget; the run's default otherwise.
    pub turns: Option<u32>,
    /// A card's token budget; the run's default otherwise.
    pub context_tokens: Option<u64>,
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
    /// retries with backoff. None of these charge an attempt.
    pub(crate) async fn session(
        &mut self,
        role: Role,
        prompt: &str,
        ctx: &SessionCtx,
    ) -> Result<SessionResult, RunError> {
        let mut recovery = Recovery::default();
        loop {
            let resumed = std::mem::take(&mut self.ledger.pending_continuation);
            if resumed {
                self.save()?;
            }
            let text = if recovery.has_continued() || resumed {
                format!("{}\n\n{}", without_map(prompt), recovery.note())
            } else {
                prompt.to_owned()
            };
            let result = self.session_once(role, &text, ctx).await?;
            let label = result.record.label.clone();
            if self.stopping() {
                return Err(self.pause(&label, role, ctx)?);
            }
            if !result.metrics.engine_failed() {
                self.note_engine_held(&result.record)?;
            }
            match recovery.next(&result.metrics, role, &self.params) {
                Step::Done => return Ok(result),
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
                    self.wait_for_engine().await;
                }
                Step::GiveUp(why) => return Err(self.give_up(&label, why)),
            }
        }
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

    /// The error that ends the run when a session keeps failing in the engine.
    fn give_up(&self, label: &str, why: GiveUp) -> RunError {
        RunError::EngineUnavailable(match why {
            GiveUp::Capacity => format!(
                "sessions still overflow the engine's memory after the context budget was lowered to {} (last: {label})",
                self.ledger.context_cap.unwrap_or(0)
            ),
            GiveUp::Engine { sessions } => {
                format!("{sessions} sessions in a row ended with engine errors (last: {label})")
            }
        })
    }

    /// Records a pause during `label` and returns the error that unwinds the run.
    fn pause(&mut self, label: &str, role: Role, ctx: &SessionCtx) -> Result<RunError, RunError> {
        self.ledger.pending_continuation = true;
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
        if self.ledger.context_cap.is_none_or(|current| cap < current) {
            self.ledger.context_cap = Some(cap);
        }
        self.ledger.cap_clean_sessions = 0;
        if std::mem::take(&mut self.ledger.cap_raised) {
            // What the engine holds at a raised budget is what it holds
            // without passing pressure: later raises stop there.
            self.ledger.cap_failed_raises += 1;
            self.ledger.cap_ceiling = Some(self.ledger.cap_ceiling.map_or(cap, |c| c.max(cap)));
        }
        self.save()?;
        let current = self.ledger.context_cap.unwrap_or(cap);
        self.journal(JournalRecord::ContextCap {
            label: label.to_owned(),
            engine_max_context: limit,
            context_cap: current,
            retry,
        });
        self.log(&format!("  {label}: the engine can hold about {limit} tokens now; context budget lowered to {current}; rerunning the same session ({retry}/{}, not counted as an attempt)", self.params.capacity_retries));
        Ok(())
    }

    /// Counts a session the engine held at a lowered budget and, after
    /// `capacityRecoverSessions` of them in a row, raises the budget halfway
    /// back to the largest configured one (the run's models may have
    /// different windows), or to the ceiling a failed raise set. A session
    /// whose budget was under the cap does not count: it did not test it. The
    /// engine only says how much it can hold when a prompt overflows it, so
    /// memory freed later is found by trying. Each raise that overflows again
    /// doubles the sessions the next one waits for, up to four times as many;
    /// a raise the engine held starts the count again.
    fn note_engine_held(&mut self, session: &SessionRecord) -> Result<(), RunError> {
        let Some(cap) = self.ledger.context_cap else {
            return Ok(());
        };
        if self.params.capacity_recover_sessions == 0 {
            return Ok(());
        }
        let label = session.label.as_str();
        if session
            .memory
            .as_ref()
            .is_some_and(|m| m.pressure_max >= CRITICAL_PRESSURE)
        {
            self.ledger.cap_clean_sessions = 0;
            return self.save();
        }
        if session.context_budget < cap {
            return Ok(());
        }
        self.ledger.cap_clean_sessions += 1;
        let needed = raise_wait(
            self.params.capacity_recover_sessions,
            self.ledger.cap_failed_raises,
        );
        let clean = self.ledger.cap_clean_sessions;
        if clean < needed {
            return self.save();
        }
        let full = self.ledger.largest_budget_tokens();
        let Some(raised) = raised_cap(cap, full, self.ledger.cap_ceiling) else {
            // Already at the ceiling: nothing to try.
            self.ledger.cap_clean_sessions = 0;
            return self.save();
        };
        let new_cap = (raised < full).then_some(raised);
        if std::mem::take(&mut self.ledger.cap_raised) {
            self.ledger.cap_failed_raises = 0;
        }
        self.ledger.context_cap = new_cap;
        self.ledger.cap_clean_sessions = 0;
        self.ledger.cap_raised = true;
        self.save()?;
        self.journal(JournalRecord::ContextCapRaised {
            label: label.to_owned(),
            context_cap: new_cap,
            clean_sessions: clean,
        });
        let budget = new_cap.unwrap_or(full);
        self.log(&format!("  {label}: {clean} sessions in a row without the engine running out of memory; context budget raised to {budget}"));
        Ok(())
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
        let what = match cause {
            FailureCause::Harness => "the harness ended before the turn settled",
            FailureCause::Engine => "engine error",
            FailureCause::OutOfMemory => {
                "the engine ran out of memory (another program may be using the GPU)"
            }
        };
        self.log(&format!("  {label}: {what}; retrying the same session in {wait} s ({}/{}, not counted as an attempt)", retry + 1, self.params.engine_retries));
        if retry == 0 && cause != FailureCause::Harness {
            self.notify("The engine is failing (often memory). Free memory; the run retries the same session.");
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

/// Sessions the engine must hold before a lowered budget is raised: the
/// configured count, doubled per raise that failed, at most four times it.
fn raise_wait(recover_sessions: u32, failed_raises: u32) -> u32 {
    recover_sessions.saturating_mul(1 << failed_raises.min(2))
}

/// The budget after a raise from `cap`: halfway to the largest configured
/// budget `full`, or to `ceiling` when that is lower, and all the way when less
/// than [`RAISE_STEP_MIN`] is left. `None` when `cap` is already there.
fn raised_cap(cap: u64, full: u64, ceiling: Option<u64>) -> Option<u64> {
    let target = ceiling.map_or(full, |c| c.min(full));
    if cap >= target {
        return None;
    }
    let raised = cap + (target - cap) / 2;
    let raised = if target - raised < RAISE_STEP_MIN {
        target
    } else {
        raised
    };
    Some(raised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raises_go_halfway_and_stop_at_the_ceiling_a_failed_raise_set() {
        // No ceiling: halfway to the configured budget, then all the way.
        assert_eq!(raised_cap(49_190, 85_196, None), Some(67_193));
        assert_eq!(raised_cap(83_000, 85_196, None), Some(85_196));
        // The Wild West run: the engine held 86016 when a raise overflowed,
        // so 68812 is the ceiling, and a passing low reading climbs back to it.
        let ceiling = Some(68_812);
        assert_eq!(raised_cap(38_215, 85_196, ceiling), Some(53_513));
        assert_eq!(raised_cap(66_000, 85_196, ceiling), Some(68_812));
        assert_eq!(raised_cap(68_812, 85_196, ceiling), None);
        assert_eq!(raised_cap(70_000, 85_196, ceiling), None);
        // A ceiling above the configured budget does not lift it further.
        assert_eq!(raised_cap(60_000, 85_196, Some(90_000)), Some(72_598));
    }

    #[test]
    fn the_wait_before_a_raise_doubles_at_most_twice() {
        assert_eq!(raise_wait(4, 0), 4);
        assert_eq!(raise_wait(4, 1), 8);
        assert_eq!(raise_wait(4, 2), 16);
        assert_eq!(raise_wait(4, 7), 16);
        assert_eq!(raise_wait(0, 3), 0);
    }
}
