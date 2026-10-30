//! What the task loop does after a session ends: continue the same work in
//! a fresh session, lower the context budget, retry after an engine error,
//! accept the result, or give up. None of these charge an attempt. The
//! runner journals, logs and waits; this module only decides.

use crate::briefing::{CONTINUATION, OUT_OF_MEMORY, TURN_CONTINUATION, image_continuation};
use crate::params::RunParams;
use crate::role::Role;
use crate::session::{SessionMetrics, StopReason};

/// Why a fresh session continues the same work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Continuation {
    /// The session ran out of context: its budget, or a second compaction.
    Context,
    /// The engine refused an image the session read.
    Image(String),
    /// A session that edits code stopped at its turn budget.
    Turns,
}

/// Who ended a session the engine failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureCause {
    /// The harness ended before the turn settled.
    Harness,
    /// The engine returned an error.
    Engine,
    /// The engine ran out of memory on a request. Journaled as `engine`,
    /// so a script that counts engine reruns still counts it; the log line
    /// and the session's `provider-error` event name the memory.
    OutOfMemory,
}

impl FailureCause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Engine | Self::OutOfMemory => "engine",
        }
    }
}

/// Why the loop stops rerunning a session the engine failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GiveUp {
    /// Sessions still overflow after `capacityRetries` lowered budgets.
    Capacity,
    /// This many sessions in a row ended with engine errors; `cause` is
    /// why the last one failed.
    Engine { sessions: u32, cause: FailureCause },
}

/// The next thing the loop does, with the count it reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The session's result stands.
    Done,
    /// A fresh session continues the same work; `count` of `contextContinuations`.
    Continue { kind: Continuation, count: u32 },
    /// The engine can hold `limit` tokens: lower the budget and rerun;
    /// `count` of `capacityRetries`.
    LowerCap { limit: u64, count: u32 },
    /// Wait and rerun the same session; `retry` reruns came before it.
    Retry { retry: u32, cause: FailureCause },
    /// Stop rerunning: the run cannot go on with this engine.
    GiveUp(GiveUp),
}

/// The prompt note a continued session gets. A context stop keeps an image
/// note and drops a turn note; a session that ends without a continuation
/// drops an image note and keeps a turn note, which the rerun of the same
/// session still needs. The loop has always worked this way: keep it unless
/// a run shows a note is wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Note {
    #[default]
    Plain,
    Image(String),
    Turns,
}

/// The counts the loop keeps across the reruns of one session.
#[derive(Debug, Default)]
pub(crate) struct Recovery {
    continued: u32,
    capacity: u32,
    retry: u32,
    note: Note,
    /// The engine ran out of memory in a builder session, and jobs are on:
    /// every later session of this loop gets the memory note.
    out_of_memory: bool,
}

impl Recovery {
    /// Whether the session already ran once and the next prompt carries a note.
    pub fn has_continued(&self) -> bool {
        self.continued > 0
    }

    /// The note added to the prompt of a session that continues earlier work.
    pub fn note(&self) -> String {
        match &self.note {
            Note::Image(refusal) => image_continuation(refusal),
            Note::Turns => TURN_CONTINUATION.to_owned(),
            Note::Plain => CONTINUATION.to_owned(),
        }
    }

    /// The note the sessions after an engine out-of-memory get, when they
    /// can move heavy work into a job (ADR 0010).
    pub fn memory_note(&self) -> Option<&'static str> {
        self.out_of_memory.then_some(OUT_OF_MEMORY)
    }

    /// Decides what follows a session of `role` that ended with `metrics`,
    /// and counts it.
    pub fn next(&mut self, metrics: &SessionMetrics, role: Role, params: &RunParams) -> Step {
        if let Some(kind) = continuation(metrics, role)
            && self.continued < params.context_continuations
        {
            return self.continue_with(kind);
        }
        self.note = std::mem::take(&mut self.note).without_image();
        if !metrics.engine_failed() {
            return Step::Done;
        }
        // A prompt larger than the engine's current memory allows fails the same way
        // after any wait. If the reported maximum is still usable, lower the budget and
        // rerun after a pause; a tiny maximum means something else holds it, so back off.
        match metrics.capacity_limit() {
            Some(limit) if limit >= params.context_floor => self.lower_cap(limit, params),
            _ => self.retry(metrics, role, params),
        }
    }

    fn continue_with(&mut self, kind: Continuation) -> Step {
        self.continued += 1;
        self.note = std::mem::take(&mut self.note).after(&kind);
        Step::Continue {
            kind,
            count: self.continued,
        }
    }

    fn lower_cap(&mut self, limit: u64, params: &RunParams) -> Step {
        if self.capacity >= params.capacity_retries {
            return Step::GiveUp(GiveUp::Capacity);
        }
        self.capacity += 1;
        Step::LowerCap {
            limit,
            count: self.capacity,
        }
    }

    fn retry(&mut self, metrics: &SessionMetrics, role: Role, params: &RunParams) -> Step {
        let retry = self.retry;
        let cause = failure_cause(metrics);
        if retry >= params.engine_retries {
            return Step::GiveUp(GiveUp::Engine {
                sessions: retry + 1,
                cause,
            });
        }
        self.retry += 1;
        // Only a builder can declare a job, and only while jobs are on.
        self.out_of_memory |=
            cause == FailureCause::OutOfMemory && role == Role::Builder && params.jobs_on();
        Step::Retry { retry, cause }
    }
}

/// Who ended a session the engine failed. What the engine said comes
/// first: fx exits after an engine error, and that exit is its
/// consequence, not its cause. The session's log line ranks an abort
/// reason higher (`outcome` in session/record.rs): it says why the session
/// stopped, this says why it is rerun.
fn failure_cause(metrics: &SessionMetrics) -> FailureCause {
    if metrics.engine_out_of_memory {
        FailureCause::OutOfMemory
    } else if metrics.harness_exit.is_none() || metrics.last_stop_reason == Some(StopReason::Error)
    {
        FailureCause::Engine
    } else {
        FailureCause::Harness
    }
}

impl Note {
    /// The note after a continuation of `kind`. A context stop keeps an
    /// image note: the refused image is still in the work.
    fn after(self, kind: &Continuation) -> Self {
        match kind {
            Continuation::Context => match self {
                Self::Turns => Self::Plain,
                kept => kept,
            },
            Continuation::Image(refusal) => Self::Image(refusal.clone()),
            Continuation::Turns => Self::Turns,
        }
    }

    /// The note once a session ends without a continuation: an image note
    /// is dropped, a turn note stays for the rerun of the same session.
    fn without_image(self) -> Self {
        match self {
            Self::Image(_) => Self::Plain,
            kept => kept,
        }
    }
}

/// The continuation a session's stop calls for, before the budget for them
/// is checked. A session that edits files and ran out of turns was working,
/// not looping (the loop, repeat and idle rules stop those first): its
/// changes are kept, like a context stop, and a fresh session finishes them
/// instead of the attempt counting as failed.
fn continuation(metrics: &SessionMetrics, role: Role) -> Option<Continuation> {
    if metrics.context_stopped() {
        return Some(Continuation::Context);
    }
    if let Some(refusal) = &metrics.image_refusal {
        return Some(Continuation::Image(refusal.clone()));
    }
    (metrics.turns_stopped() && role.edits_code()).then_some(Continuation::Turns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StopReason;

    /// Each budget differs, so a test fails if one is read for another.
    const REFUSAL: &str = "at most 1 images";

    fn params() -> RunParams {
        RunParams {
            context_continuations: 2,
            capacity_retries: 1,
            engine_retries: 3,
            context_floor: 1000,
            ..RunParams::default()
        }
    }

    fn settled() -> SessionMetrics {
        SessionMetrics {
            settled: true,
            exit_code: Some(0),
            ..SessionMetrics::default()
        }
    }

    fn aborted(reason: &str) -> SessionMetrics {
        SessionMetrics {
            abort_reason: Some(reason.to_owned()),
            ..SessionMetrics::default()
        }
    }

    fn context_stop() -> SessionMetrics {
        aborted("context budget exceeded: 9000 > 8000")
    }

    fn turns_stop() -> SessionMetrics {
        aborted("turn budget exceeded: 45 > 30")
    }

    fn engine_error() -> SessionMetrics {
        SessionMetrics {
            last_stop_reason: Some(StopReason::Error),
            ..SessionMetrics::default()
        }
    }

    /// The watchdog records a refusal only from an engine error.
    fn image_refused() -> SessionMetrics {
        SessionMetrics {
            image_refusal: Some(REFUSAL.to_owned()),
            ..engine_error()
        }
    }

    fn overflow(limits: &[u64]) -> SessionMetrics {
        SessionMetrics {
            capacity_limits: limits.to_vec(),
            ..engine_error()
        }
    }

    fn harness_ended() -> SessionMetrics {
        SessionMetrics {
            harness_exit: Some("the harness ended".to_owned()),
            ..SessionMetrics::default()
        }
    }

    fn continued(kind: Continuation, count: u32) -> Step {
        Step::Continue { kind, count }
    }

    fn image() -> Continuation {
        Continuation::Image(REFUSAL.to_owned())
    }

    fn retry(retry: u32, cause: FailureCause) -> Step {
        Step::Retry { retry, cause }
    }

    fn engine_retry(retry: u32) -> Step {
        Step::Retry {
            retry,
            cause: FailureCause::Engine,
        }
    }

    fn lower_cap(limit: u64) -> Step {
        Step::LowerCap { limit, count: 1 }
    }

    /// Runs `sessions` through one recovery and returns the steps.
    fn steps(role: Role, sessions: &[SessionMetrics]) -> (Recovery, Vec<Step>) {
        steps_with(&params(), role, sessions)
    }

    /// [`steps`] with `params`.
    fn steps_with(
        params: &RunParams,
        role: Role,
        sessions: &[SessionMetrics],
    ) -> (Recovery, Vec<Step>) {
        let mut recovery = Recovery::default();
        let steps = sessions
            .iter()
            .map(|m| recovery.next(m, role, params))
            .collect();
        (recovery, steps)
    }

    #[test]
    fn a_settled_session_is_done() {
        let (_, steps) = steps(Role::Builder, &[settled()]);
        assert_eq!(steps, [Step::Done]);
    }

    #[test]
    fn only_a_continuation_makes_the_next_prompt_carry_a_note() {
        let (retried, _) = steps(Role::Builder, &[engine_error()]);
        assert!(!retried.has_continued());
        let (continued, _) = steps(Role::Builder, &[context_stop()]);
        assert!(continued.has_continued());
    }

    #[test]
    fn a_fresh_recovery_gives_the_plain_note_to_a_resumed_session() {
        assert_eq!(Recovery::default().note(), CONTINUATION);
    }

    #[test]
    fn context_stops_continue_until_the_budget_for_them_is_spent() {
        let (_, steps) = steps(
            Role::Reviewer,
            &[context_stop(), context_stop(), context_stop()],
        );
        assert_eq!(
            steps,
            [
                continued(Continuation::Context, 1),
                continued(Continuation::Context, 2),
                Step::Done
            ]
        );
    }

    #[test]
    fn the_three_kinds_of_continuation_share_one_budget() {
        let (_, steps) = steps(
            Role::Builder,
            &[image_refused(), context_stop(), turns_stop()],
        );
        assert_eq!(
            steps,
            [
                continued(image(), 1),
                continued(Continuation::Context, 2),
                Step::Done
            ]
        );
    }

    #[test]
    fn an_image_refusal_continues_with_the_refusal_in_the_note() {
        let (recovery, steps) = steps(Role::Builder, &[image_refused()]);
        assert_eq!(steps, [continued(image(), 1)]);
        assert_eq!(recovery.note(), image_continuation(REFUSAL));
    }

    #[test]
    fn an_image_refusal_after_the_budget_is_retried_without_the_image_note() {
        let (recovery, steps) = steps(
            Role::Builder,
            &[context_stop(), image_refused(), image_refused()],
        );
        assert_eq!(
            steps,
            [
                continued(Continuation::Context, 1),
                continued(image(), 2),
                engine_retry(0)
            ]
        );
        assert_eq!(recovery.note(), CONTINUATION);
    }

    #[test]
    fn a_context_stop_wins_over_an_image_refusal() {
        let both = SessionMetrics {
            image_refusal: Some(REFUSAL.to_owned()),
            ..context_stop()
        };
        let (recovery, steps) = steps(Role::Builder, &[both]);
        assert_eq!(steps, [continued(Continuation::Context, 1)]);
        assert_eq!(recovery.note(), CONTINUATION);
    }

    #[test]
    fn a_context_stop_keeps_the_image_note() {
        let (recovery, steps) = steps(Role::Builder, &[image_refused(), context_stop()]);
        assert_eq!(
            steps,
            [continued(image(), 1), continued(Continuation::Context, 2)]
        );
        assert_eq!(recovery.note(), image_continuation(REFUSAL));
    }

    #[test]
    fn a_retried_session_drops_the_image_note() {
        let (recovery, steps) = steps(Role::Builder, &[image_refused(), engine_error()]);
        assert_eq!(steps, [continued(image(), 1), engine_retry(0)]);
        assert_eq!(recovery.note(), CONTINUATION);
    }

    #[test]
    fn a_turn_stop_continues_for_a_role_that_edits_code() {
        let (recovery, steps) = steps(Role::Builder, &[turns_stop()]);
        assert_eq!(steps, [continued(Continuation::Turns, 1)]);
        assert_eq!(recovery.note(), TURN_CONTINUATION);
    }

    #[test]
    fn a_turn_stop_is_the_result_for_a_role_that_does_not_edit_code() {
        let (_, steps) = steps(Role::Reviewer, &[turns_stop()]);
        assert_eq!(steps, [Step::Done]);
    }

    #[test]
    fn a_turn_stop_after_the_budget_is_the_result() {
        let (_, steps) = steps(Role::Builder, &[turns_stop(), turns_stop(), turns_stop()]);
        assert_eq!(
            steps,
            [
                continued(Continuation::Turns, 1),
                continued(Continuation::Turns, 2),
                Step::Done
            ]
        );
    }

    #[test]
    fn a_turn_stop_replaces_the_image_note() {
        let (recovery, steps) = steps(Role::Builder, &[image_refused(), turns_stop()]);
        assert_eq!(
            steps,
            [continued(image(), 1), continued(Continuation::Turns, 2)]
        );
        assert_eq!(recovery.note(), TURN_CONTINUATION);
    }

    #[test]
    fn an_image_refusal_replaces_the_turn_note() {
        let (recovery, steps) = steps(Role::Builder, &[turns_stop(), image_refused()]);
        assert_eq!(
            steps,
            [continued(Continuation::Turns, 1), continued(image(), 2)]
        );
        assert_eq!(recovery.note(), image_continuation(REFUSAL));
    }

    #[test]
    fn the_turn_note_survives_an_engine_retry() {
        let (recovery, steps) = steps(Role::Builder, &[turns_stop(), engine_error()]);
        assert_eq!(steps, [continued(Continuation::Turns, 1), engine_retry(0)]);
        assert_eq!(recovery.note(), TURN_CONTINUATION);
    }

    #[test]
    fn a_context_stop_replaces_the_turn_note() {
        let (recovery, steps) = steps(Role::Builder, &[turns_stop(), context_stop()]);
        assert_eq!(
            steps,
            [
                continued(Continuation::Turns, 1),
                continued(Continuation::Context, 2)
            ]
        );
        assert_eq!(recovery.note(), CONTINUATION);
    }

    #[test]
    fn engine_errors_retry_counting_up_then_give_up() {
        let (_, steps) = steps(Role::Builder, &vec![engine_error(); 4]);
        assert_eq!(
            steps,
            [
                engine_retry(0),
                engine_retry(1),
                engine_retry(2),
                Step::GiveUp(GiveUp::Engine {
                    sessions: 4,
                    cause: FailureCause::Engine
                })
            ]
        );
    }

    /// The session's `harness_exit` after `engine`: fx exits once the engine
    /// fails a request.
    fn after_harness_exit(engine: SessionMetrics) -> SessionMetrics {
        SessionMetrics {
            harness_exit: Some("the agent connection ended: ProviderError".to_owned()),
            ..engine
        }
    }

    /// The engine's error names the cause, not the exit that followed it.
    #[test]
    fn an_engine_error_before_the_harness_exit_is_the_cause() {
        let (_, steps) = steps(Role::Builder, &[after_harness_exit(engine_error())]);
        assert_eq!(steps, [engine_retry(0)]);
    }

    #[test]
    fn an_engine_out_of_memory_before_the_harness_exit_is_named() {
        let out_of_memory = SessionMetrics {
            engine_out_of_memory: true,
            ..engine_error()
        };
        let (_, steps) = steps(Role::Builder, &[after_harness_exit(out_of_memory)]);
        assert_eq!(steps, [retry(0, FailureCause::OutOfMemory)]);
    }

    /// The cause is the last session's, whatever came before it.
    #[test]
    fn giving_up_names_the_last_cause() {
        let last_cause =
            |sessions: &[SessionMetrics]| match steps(Role::Reviewer, sessions).1.last() {
                Some(Step::GiveUp(GiveUp::Engine { cause, .. })) => Some(*cause),
                _ => None,
            };
        let memory_then_engine = [
            out_of_memory(),
            out_of_memory(),
            out_of_memory(),
            engine_error(),
        ];
        assert_eq!(last_cause(&memory_then_engine), Some(FailureCause::Engine));
        let engine_then_memory = [
            engine_error(),
            engine_error(),
            engine_error(),
            out_of_memory(),
        ];
        assert_eq!(
            last_cause(&engine_then_memory),
            Some(FailureCause::OutOfMemory)
        );
    }

    fn out_of_memory() -> SessionMetrics {
        SessionMetrics {
            engine_out_of_memory: true,
            ..engine_error()
        }
    }

    /// In the Lotion Pump run the engine ran out of memory beside a render
    /// the builder left in the background. Its rerun is told to declare a
    /// job instead.
    #[test]
    fn a_builder_rerun_after_an_out_of_memory_is_told_to_declare_a_job() {
        let (builder, _) = steps(Role::Builder, &[out_of_memory()]);
        assert_eq!(builder.memory_note(), Some(OUT_OF_MEMORY));
    }

    /// The note stays for the rest of the loop: a continuation of the rerun
    /// gets it beside its own note, as the heavy work is still to do.
    #[test]
    fn the_memory_note_stays_beside_a_later_continuation() {
        let (builder, _) = steps(Role::Builder, &[out_of_memory(), context_stop()]);
        assert_eq!(builder.memory_note(), Some(OUT_OF_MEMORY));
        assert_eq!(builder.note(), CONTINUATION);
    }

    /// Only a builder can declare a job.
    #[test]
    fn a_reviewer_rerun_after_an_out_of_memory_is_told_nothing() {
        let (reviewer, _) = steps(Role::Reviewer, &[out_of_memory()]);
        assert_eq!(reviewer.memory_note(), None);
    }

    #[test]
    fn a_builder_rerun_after_another_engine_error_is_told_nothing() {
        let (builder, _) = steps(Role::Builder, &[engine_error()]);
        assert_eq!(builder.memory_note(), None);
    }

    /// With `jobMinutes=0` a job would not run, so none is asked for.
    #[test]
    fn with_jobs_off_a_builder_rerun_is_told_nothing() {
        let off = RunParams {
            job_minutes: 0,
            ..params()
        };
        let (builder, steps) = steps_with(&off, Role::Builder, &[out_of_memory()]);
        assert_eq!(steps, [retry(0, FailureCause::OutOfMemory)]);
        assert_eq!(builder.memory_note(), None);
    }

    /// `cause` in the journal is read by scripts: an out-of-memory rerun
    /// keeps the value they count engine reruns by.
    #[test]
    fn an_out_of_memory_rerun_is_journaled_as_engine() {
        assert_eq!(FailureCause::OutOfMemory.as_str(), "engine");
    }

    #[test]
    fn harness_and_engine_failures_share_the_retry_budget() {
        let (_, steps) = steps(Role::Builder, &[harness_ended(), engine_error()]);
        assert_eq!(steps, [retry(0, FailureCause::Harness), engine_retry(1)]);
    }

    #[test]
    fn a_usable_capacity_limit_lowers_the_cap_then_gives_up() {
        let (_, steps) = steps(Role::Builder, &[overflow(&[4000]), overflow(&[4000])]);
        assert_eq!(steps, [lower_cap(4000), Step::GiveUp(GiveUp::Capacity)]);
    }

    #[test]
    fn the_smallest_limit_the_engine_named_is_lowered_to() {
        let (_, steps) = steps(Role::Builder, &[overflow(&[6000, 4000])]);
        assert_eq!(steps, [lower_cap(4000)]);
    }

    #[test]
    fn a_limit_at_the_floor_is_still_usable() {
        let (_, steps) = steps(Role::Builder, &[overflow(&[1000])]);
        assert_eq!(steps, [lower_cap(1000)]);
    }

    #[test]
    fn a_limit_below_the_floor_backs_off_like_an_engine_error() {
        let (_, steps) = steps(Role::Builder, &[overflow(&[999])]);
        assert_eq!(steps, [engine_retry(0)]);
    }

    #[test]
    fn a_retry_leaves_the_capacity_budget_whole() {
        let (_, steps) = steps(Role::Builder, &[engine_error(), overflow(&[4000])]);
        assert_eq!(steps, [engine_retry(0), lower_cap(4000)]);
    }

    #[test]
    fn a_lowered_cap_leaves_the_retry_budget_whole() {
        let (_, steps) = steps(Role::Builder, &[overflow(&[4000]), engine_error()]);
        assert_eq!(steps, [lower_cap(4000), engine_retry(0)]);
    }

    #[test]
    fn engine_failures_leave_the_continuation_budget_whole() {
        let (_, steps) = steps(
            Role::Builder,
            &[
                overflow(&[4000]),
                engine_error(),
                context_stop(),
                context_stop(),
            ],
        );
        assert_eq!(
            steps,
            [
                lower_cap(4000),
                engine_retry(0),
                continued(Continuation::Context, 1),
                continued(Continuation::Context, 2)
            ]
        );
    }
}
