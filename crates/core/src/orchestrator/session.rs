//! A session with its recovery: the wrapper that reruns it after engine
//! failures, capacity caps and context stops without charging the task an
//! attempt, or pauses it. One session, start to journal record, is in
//! `record`; the harness run itself in `harness`; its requests in
//! `requests`.

use std::time::Duration;

use crate::briefing::without_map;
use crate::ledger::{JournalRecord, SessionRecord};
use crate::ports::SessionModel;
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::task::TaskId;
use crate::util::approx_code_tokens;

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
            let resumed = self.ledger.pending_continuation;
            let text = session_prompt(prompt, &recovery, resumed);
            self.check_prompt_fits(role, &text, ctx)?;
            if std::mem::take(&mut self.ledger.pending_continuation) {
                self.save()?;
            }
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
                    // The next session meets the engine as it is: a failure
                    // there counts toward the same retries.
                    let _ = self.wait_for_engine("after an engine error").await;
                }
                Step::PromptRefused => return Err(self.prompt_refused(&result.record, role)),
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
        let model = self.ledger.config.model_for(role);
        let limit = PromptLimit {
            role,
            model: &model,
            budget: self.ledger.model_budget_tokens(role),
            reserve: self.params.harness_reserve_tokens,
        };
        match limit.over(prompt, ctx.task.as_ref()) {
            Some(reason) => Err(RunError::PromptTooLarge(reason)),
            None => Ok(()),
        }
    }

    /// The error that ends the run when the harness refused `record`'s prompt.
    fn prompt_refused(&self, record: &SessionRecord, role: Role) -> RunError {
        let model = self.ledger.config.model_for(role);
        RunError::PromptTooLarge(prompt_refused_reason(
            &record.label,
            role,
            &model,
            record.prompt_chars,
        ))
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

/// What the operator can do when the engine runs out of memory: in the
/// Digger 3D run waits did not bring back the memory mlx-serve needed, a
/// reload did (ADR 0012).
const OUT_OF_MEMORY_ADVICE: &str =
    "free memory (another program may be using the GPU) or reload the model";

/// Why a session the engine failed is rerun, for the log.
fn retry_reason(cause: FailureCause) -> String {
    match cause {
        FailureCause::Harness => "the harness ended before the turn settled".to_owned(),
        FailureCause::Engine => "engine error".to_owned(),
        FailureCause::OutOfMemory => {
            format!("the engine ran out of memory; {OUT_OF_MEMORY_ADVICE}")
        }
    }
}

/// The notice at the first rerun of a session the engine failed; none for
/// a harness that ended on its own.
fn retry_notice(cause: FailureCause) -> Option<String> {
    match cause {
        FailureCause::Harness => None,
        FailureCause::Engine => Some(
            "The engine is failing (often memory). Free memory; the run retries the same session."
                .to_owned(),
        ),
        FailureCause::OutOfMemory => Some(format!(
            "The engine ran out of memory: {OUT_OF_MEMORY_ADVICE}. The run retries the same session."
        )),
    }
}

/// The first request of a session with `prompt`, estimated: the prompt at
/// three bytes per token, plus the harness's `reserve`. Over the Digger 3D
/// run, with a reserve of 8192, this was 2.2k to 4.5k tokens high and never
/// low (ADR 0012).
fn first_request_tokens(prompt: &str, reserve: u64) -> u64 {
    approx_code_tokens(prompt) as u64 + reserve
}

/// What a prompt of a `role` session must fit: `budget` tokens of
/// `model`, with the harness's `reserve` on top of the prompt.
struct PromptLimit<'a> {
    role: Role,
    model: &'a SessionModel,
    budget: u64,
    reserve: u64,
}

impl PromptLimit<'_> {
    /// Why `prompt`, for `task` when there is one, does not fit; `None`
    /// when it does.
    fn over(&self, prompt: &str, task: Option<&TaskId>) -> Option<String> {
        let estimate = first_request_tokens(prompt, self.reserve);
        if estimate <= self.budget {
            return None;
        }
        let role = self.role;
        let what = task.map_or_else(
            || format!("{role} prompt"),
            |task| format!("{role} prompt for {task}"),
        );
        Some(format!(
            "the {what}, {} bytes, is about {estimate} tokens with the harness's reserve, over the {role}'s budget of {}; {}. To go on, {}",
            prompt.len(),
            self.budget,
            window(self.model),
            change_advice(role)
        ))
    }
}

/// `model`'s name and window, for a reason.
fn window(model: &SessionModel) -> String {
    format!("{}'s window is {} tokens", model.name, model.context_window)
}

/// What to change when a `role` prompt does not fit: a review can be made
/// smaller; a model is changed in a new run only.
fn change_advice(role: Role) -> String {
    let larger = format!(
        "give the {role} a model with a larger window in a new run (tenaz init --model-for, --context-window-for)"
    );
    if role == Role::Reviewer {
        format!(
            "lower params.diffShare or params.diffLimitBytes in .tenaz/run.json and run with --resume, or {larger}"
        )
    } else {
        larger
    }
}

/// Why the run stops at `label`, whose prompt of `prompt_bytes` the harness
/// refused for `role`'s `model`: what was refused, and what to change.
fn prompt_refused_reason(
    label: &str,
    role: Role,
    model: &SessionModel,
    prompt_bytes: usize,
) -> String {
    format!(
        "the harness refused the {role} prompt of {label}, {prompt_bytes} bytes; {}. To go on, {}",
        window(model),
        change_advice(role)
    )
}

/// Why the run stops rerunning `label`, under the run's `context_cap`.
fn give_up_reason(label: &str, why: GiveUp, context_cap: Option<u64>) -> String {
    match why {
        GiveUp::Capacity => format!(
            "sessions still overflow the engine's memory after the context budget was lowered to {} (last: {label})",
            context_cap.unwrap_or(0)
        ),
        GiveUp::Engine {
            sessions,
            cause: FailureCause::OutOfMemory,
        } => format!(
            "{sessions} sessions in a row ended with engine errors, the last one out of memory (last: {label}); {OUT_OF_MEMORY_ADVICE}"
        ),
        GiveUp::Engine { sessions, .. } => {
            format!("{sessions} sessions in a row ended with engine errors (last: {label})")
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

/// The prompt of a session: `prompt`, with the note a session that
/// continues earlier work gets, and the one a builder rerun after the
/// engine ran out of memory gets.
fn session_prompt(prompt: &str, recovery: &Recovery, resumed: bool) -> String {
    let mut text = if recovery.has_continued() || resumed {
        format!("{}\n\n{}", without_map(prompt), recovery.note())
    } else {
        prompt.to_owned()
    };
    if let Some(note) = recovery.memory_note() {
        text = format!("{text}\n\n{note}");
    }
    text
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

    /// mlx-serve refuses a prompt for memory until the model is reloaded
    /// (the Digger 3D run of 2026-10-29): the reason says so.
    #[test]
    fn giving_up_after_the_engine_ran_out_of_memory_says_what_to_do() {
        let why = GiveUp::Engine {
            sessions: 7,
            cause: FailureCause::OutOfMemory,
        };
        assert_eq!(
            give_up_reason("031-reviewer-T003", why, None),
            "7 sessions in a row ended with engine errors, the last one out of memory (last: 031-reviewer-T003); free memory (another program may be using the GPU) or reload the model"
        );
    }

    #[test]
    fn giving_up_after_other_engine_errors_gives_no_memory_advice() {
        let why = GiveUp::Engine {
            sessions: 7,
            cause: FailureCause::Engine,
        };
        assert_eq!(
            give_up_reason("020-reviewer-T003", why, None),
            "7 sessions in a row ended with engine errors (last: 020-reviewer-T003)"
        );
    }

    fn qwen() -> SessionModel {
        SessionModel {
            name: "mlx-serve/Qwen3.8-27B".into(),
            id: "Qwen3.8-27B".into(),
            context_window: 65_536,
            supports_images: false,
        }
    }

    /// The reviewer of T001 in the Digger 3D run of 2026-10-29.
    #[test]
    fn a_refused_review_names_its_size_and_window_and_what_to_lower() {
        assert_eq!(
            prompt_refused_reason("007-reviewer-T001", Role::Reviewer, &qwen(), 281_465),
            "the harness refused the reviewer prompt of 007-reviewer-T001, 281465 bytes; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, lower params.diffShare or params.diffLimitBytes in .tenaz/run.json and run with --resume, or give the reviewer a model with a larger window in a new run (tenaz init --model-for, --context-window-for)"
        );
    }

    #[test]
    fn a_refused_prompt_of_another_role_asks_for_a_larger_window_only() {
        assert_eq!(
            prompt_refused_reason("005-builder-T001", Role::Builder, &qwen(), 300_000),
            "the harness refused the builder prompt of 005-builder-T001, 300000 bytes; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, give the builder a model with a larger window in a new run (tenaz init --model-for, --context-window-for)"
        );
    }

    /// Session 025 of the Digger 3D run: a reviewer prompt of 79670
    /// bytes whose first request was 30245 tokens.
    #[test]
    fn a_first_request_is_estimated_high_from_the_prompt_and_the_reserve() {
        let estimate = first_request_tokens(&"x".repeat(79_670), 8192);
        assert_eq!(estimate, 34_749);
        assert!(estimate >= 30_245);
        assert_eq!(first_request_tokens("", 8192), 8192);
    }

    fn limit(role: Role, model: &SessionModel) -> PromptLimit<'_> {
        PromptLimit {
            role,
            model,
            budget: 8202,
            reserve: 8192,
        }
    }

    /// 30 bytes are 10 tokens: with the reserve, exactly the budget.
    #[test]
    fn a_prompt_fits_up_to_its_budget_and_not_a_token_more() {
        let model = qwen();
        let reviewer = limit(Role::Reviewer, &model);
        assert_eq!(reviewer.over(&"x".repeat(30), None), None);
        assert!(reviewer.over(&"x".repeat(31), None).is_some());
    }

    #[test]
    fn a_prompt_over_its_budget_says_whose_it_is_and_what_to_change() {
        let model = qwen();
        let task = TaskId::parse("T003").unwrap();
        assert_eq!(
            limit(Role::Reviewer, &model).over(&"x".repeat(31), Some(&task)),
            Some("the reviewer prompt for T003, 31 bytes, is about 8203 tokens with the harness's reserve, over the reviewer's budget of 8202; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, lower params.diffShare or params.diffLimitBytes in .tenaz/run.json and run with --resume, or give the reviewer a model with a larger window in a new run (tenaz init --model-for, --context-window-for)".to_owned())
        );
        assert_eq!(
            limit(Role::Replan, &model).over(&"x".repeat(31), None),
            Some("the replan prompt, 31 bytes, is about 8203 tokens with the harness's reserve, over the replan's budget of 8202; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, give the replan a model with a larger window in a new run (tenaz init --model-for, --context-window-for)".to_owned())
        );
    }

    #[test]
    fn giving_up_on_capacity_names_the_lowered_budget() {
        assert_eq!(
            give_up_reason("009-builder-T002", GiveUp::Capacity, Some(30_000)),
            "sessions still overflow the engine's memory after the context budget was lowered to 30000 (last: 009-builder-T002)"
        );
    }

    #[test]
    fn the_log_line_of_a_rerun_names_its_cause() {
        for (cause, reason) in [
            (
                FailureCause::OutOfMemory,
                "the engine ran out of memory; free memory (another program may be using the GPU) or reload the model",
            ),
            (FailureCause::Engine, "engine error"),
            (
                FailureCause::Harness,
                "the harness ended before the turn settled",
            ),
        ] {
            assert_eq!(retry_reason(cause), reason, "{cause:?}");
        }
    }

    #[test]
    fn the_first_rerun_notice_says_what_to_do_and_none_follows_a_harness_exit() {
        for (cause, notice) in [
            (
                FailureCause::OutOfMemory,
                Some(
                    "The engine ran out of memory: free memory (another program may be using the GPU) or reload the model. The run retries the same session.",
                ),
            ),
            (
                FailureCause::Engine,
                Some(
                    "The engine is failing (often memory). Free memory; the run retries the same session.",
                ),
            ),
            (FailureCause::Harness, None),
        ] {
            assert_eq!(retry_notice(cause).as_deref(), notice, "{cause:?}");
        }
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
