//! One session: open the harness, feed the watchdog, answer permissions,
//! steer and abort, then record it. And the wrapper that reruns a session
//! after engine failures, capacity caps and context stops without charging
//! the task an attempt.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::briefing::{CONTINUATION, TURN_CONTINUATION, image_continuation, without_map};
use crate::ledger::{Inflight, JournalRecord, SessionRecord};
use crate::memory::MemoryWindow;
use crate::metrics::{EngineCounters, RequestRecord, delta, engine_summary};
use crate::ownership;
use crate::ports::{PermissionDecision, SessionSpec};
use crate::role::Role;
use crate::session::{SessionEvent, SessionMetrics};
use crate::task::TaskId;
use crate::verify::retarget;
use crate::watchdog::{Action, WatchConfig, Watchdog};

use super::{RunError, Runner};

/// macOS memory pressure level `critical`: no time to raise the budget.
const CRITICAL_PRESSURE: u32 = 4;
/// A cap this close to the configured budget is lifted rather than raised.
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

impl SessionResult {
    pub fn ok(&self) -> bool {
        self.record.ok
    }
}

/// The grace given to a harness between an abort and a kill.
const ABORT_GRACE: Duration = Duration::from_secs(60);

impl Runner {
    /// How to open a session; `persist` keeps its logs and state under
    /// `.tenaz/private/`.
    fn session_spec(&self, label: &str, cwd: PathBuf, persist: bool) -> SessionSpec {
        let private = self.project.private_dir();
        let log = |suffix: &str| persist.then(|| private.join(format!("{label}.{suffix}")));
        SessionSpec {
            label: label.to_owned(),
            cwd,
            persist_dir: persist.then(|| private.join("sessions")),
            raw_log: log("rpc.jsonl"),
            thinking_log: log("thinking.md"),
            stderr_log: log("stderr.log"),
        }
    }

    /// Runs the harness once with `prompts` sent one after another (the
    /// preflight sends several). `persist` keeps the raw stream and the
    /// harness state under `.tenaz/private/`. With `own_checks`, a session
    /// that answered every prompt while `./verify --task` runs no passing
    /// check of that task gets one more prompt asking for them.
    pub(crate) async fn run_harness(
        &self,
        label: &str,
        prompts: &[String],
        cwd: PathBuf,
        watch: Option<WatchConfig>,
        persist: bool,
        mut own_checks: Option<&TaskId>,
    ) -> Result<(SessionMetrics, Vec<RequestRecord>), RunError> {
        let spec = self.session_spec(label, cwd, persist);
        let stderr_log = spec.stderr_log.clone();
        let mut session = self.deps.harness.open(spec).await?;
        let mut watchdog = Watchdog::new(self.params.clone(), watch);
        if watch.is_some_and(|w| w.idle) {
            watchdog.set_fingerprint(self.deps.vcs.worktree_fingerprint().ok());
        }
        let clock = self.deps.clock.clone();
        let started = clock.monotonic_ms();
        let wall_ms = self.params.session_minutes * 60_000;
        let recorder = RequestRecorder::start(
            label,
            persist.then(|| self.project.clone()),
            self.deps.engine_metrics.clone(),
            clock.clone(),
        );
        let mut turn_started: Option<u64> = None;
        let mut first_token: Option<u64> = None;
        let mut kill_at: Option<u64> = None;
        let mut prompt_index = 0;
        if let Some(first) = prompts.first() {
            session.prompt(first).await?;
            prompt_index = 1;
        }
        loop {
            let now = clock.monotonic_ms();
            let elapsed = now.saturating_sub(started);
            // Deadlines are checked here, not only in the timers below: with a
            // backlog of ready events a fresh timer future never gets to fire.
            if kill_at.is_some_and(|k| now >= k) {
                tracing::debug!("{label}: kill deadline passed");
                break;
            }
            if kill_at.is_none() && elapsed >= wall_ms {
                let actions = watchdog.abort_external(
                    &format!("session exceeded {} minutes", self.params.session_minutes),
                    elapsed,
                );
                kill_at = self.apply(&mut *session, actions, now, kill_at).await;
                continue;
            }
            let wall_left = Duration::from_millis(wall_ms.saturating_sub(elapsed));
            let kill_left = Duration::from_millis(kill_at.map_or(0, |k| k.saturating_sub(now)));
            let event = tokio::select! {
                event = session.next_event() => event,
                () = clock.sleep(kill_left), if kill_at.is_some() => {
                    tracing::debug!("{label}: kill timer fired");
                    break;
                }
                // Both deadlines below start when the wait ends: `now` is from
                // before it, and the abort's grace would already be over.
                () = clock.sleep(wall_left), if kill_at.is_none() => {
                    let now = clock.monotonic_ms();
                    let actions = watchdog.abort_external(&format!("session exceeded {} minutes", self.params.session_minutes), now.saturating_sub(started));
                    kill_at = self.apply(&mut *session, actions, now, kill_at).await;
                    continue;
                }
                () = self.stop.cancelled(), if kill_at.is_none() => {
                    let _ = session.abort().await;
                    kill_at = Some(clock.monotonic_ms() + 5_000);
                    continue;
                }
            };
            let Some(event) = event else {
                tracing::debug!("{label}: event stream ended");
                break;
            };
            // The wait above may have been long; the event's time is now, not
            // the time the wait started.
            let now = clock.monotonic_ms();
            tracing::trace!(
                "{label}: event {} (kill_at {kill_at:?}, now {now})",
                event_name(&event)
            );
            match &event {
                SessionEvent::Exited { .. } => break,
                SessionEvent::PermissionRequest { id, .. } => {
                    let _ = session
                        .answer_permission(id, PermissionDecision::Allow)
                        .await;
                    continue;
                }
                SessionEvent::TurnStart => {
                    turn_started = Some(now);
                    first_token = None;
                }
                SessionEvent::FirstToken => {
                    first_token.get_or_insert(now);
                }
                SessionEvent::MessageEnd {
                    usage,
                    stop_reason,
                    server,
                    sent,
                    ..
                } => {
                    let timing = ClientTiming {
                        turn_started,
                        first_token,
                        now,
                    };
                    recorder.record(RequestInput {
                        usage: *usage,
                        stop_reason: stop_reason.clone(),
                        server: *server,
                        sent: sent.clone(),
                        timing,
                    });
                }
                _ => {}
            }
            let vcs = self.deps.vcs.clone();
            let actions = watchdog.handle(&event, elapsed, move || {
                vcs.worktree_fingerprint().unwrap_or_default()
            });
            for action in &actions {
                self.log(&format!("  {label}: {}", describe(action)));
            }
            kill_at = self.apply(&mut *session, actions, now, kill_at).await;
            if let SessionEvent::Settled { aborted } = event {
                if aborted || watchdog.aborting() || self.stopping() {
                    break;
                }
                if let Some(next) = prompts.get(prompt_index) {
                    session.prompt(next).await?;
                    prompt_index += 1;
                } else if let Some(message) = self.own_checks_prompt(label, own_checks.take()).await
                {
                    watchdog.note("own-checks", clock.monotonic_ms().saturating_sub(started));
                    session.prompt(&message).await?;
                } else {
                    break;
                }
            }
        }
        let grace = if kill_at.is_some_and(|k| clock.monotonic_ms() >= k) {
            Duration::ZERO
        } else {
            ABORT_GRACE
        };
        let exit = session.close(grace).await;
        watchdog.finish(exit.code, clock.monotonic_ms().saturating_sub(started));
        let requests = recorder.finish().await;
        let mut metrics = watchdog.into_metrics();
        // The harness went away before its turn settled and nobody stopped it.
        // Between tool calls that says nothing about the task, so it is
        // handled like an engine failure (rerun, uncounted). During a tool
        // call, the call ended it and a rerun would make the same call (fx
        // dies on output that is not UTF-8), so the session stops with that
        // call as the reason and the attempt counts. The stderr is kept for
        // diagnosis either way.
        if metrics.ended_early() && !self.stopping() {
            let note = harness_exit_note(exit.code, stderr_log.as_deref());
            if let Some(call) = &metrics.tool_in_flight {
                metrics.abort_reason = Some(crashed_running(call, &note));
            } else if metrics.output_truncated() {
                // fx exits when a reply is cut at the output limit; the model
                // was still reasoning, and would again in an unchanged rerun.
                metrics.abort_reason = Some(OUTPUT_LIMIT.to_owned());
            }
            metrics.harness_exit = Some(note);
        }
        Ok((metrics, requests))
    }

    /// The prompt for a builder session that answered every prompt while
    /// `./verify --task` runs no passing check of `task`; `None` when it does.
    /// The attempt would fail on that alone, and the session still holds what
    /// it learned, so it adds them now.
    async fn own_checks_prompt(&self, label: &str, task: Option<&TaskId>) -> Option<String> {
        let task = task?;
        let own = self.run_verify(&["--task", task.as_str()]).await;
        if own.passed() > 0 {
            return None;
        }
        self.log(&format!(
            "  {label}: no passing {task} check; asking for one"
        ));
        Some(retarget(
            &no_own_checks(task, &own.summary),
            self.params.verify_path(),
        ))
    }

    /// Sends steers and aborts to the session; an abort starts the kill timer.
    async fn apply(
        &self,
        session: &mut dyn crate::ports::Session,
        actions: Vec<Action>,
        now: u64,
        kill_at: Option<u64>,
    ) -> Option<u64> {
        let mut kill_at = kill_at;
        for action in actions {
            match action {
                Action::Steer { message, .. } => {
                    let message = retarget(&message, self.params.verify_path());
                    let _ = session.steer(&message).await;
                }
                Action::Abort { .. } => {
                    tracing::debug!("aborting the session; kill in {ABORT_GRACE:?}");
                    let _ = session.abort().await;
                    kill_at.get_or_insert(
                        now + u64::try_from(ABORT_GRACE.as_millis()).unwrap_or(60_000),
                    );
                }
            }
        }
        kill_at
    }

    /// Reverts the changes a session left where its role may not write.
    fn enforce_ownership(
        &self,
        role: Role,
        task: Option<&TaskId>,
    ) -> Result<Vec<String>, RunError> {
        let vcs = self.deps.vcs.as_ref();
        let changed = vcs.changed_paths()?;
        let reverted = ownership::partition(
            role,
            changed.iter().map(String::as_str),
            task,
            &RepoView(vcs),
        );
        for path in &reverted {
            vcs.revert_path(path)?;
        }
        Ok(reverted)
    }

    /// One session, start to journal record.
    pub(crate) async fn session_once(
        &mut self,
        role: Role,
        prompt: &str,
        ctx: &SessionCtx,
    ) -> Result<SessionResult, RunError> {
        if !self.deps.vcs.changed_paths()?.is_empty() {
            self.deps
                .vcs
                .commit_all(&format!("tenaz: bookkeeping before {role}"))?;
        }
        let label = self.ledger.next_label(role, ctx.task.as_ref());
        self.ledger.inflight = Some(Inflight {
            label: label.clone(),
            role,
            task: ctx.task.clone(),
            started_at: self.now(),
        });
        self.save()?;
        self.project
            .write(&format!(".tenaz/private/{label}.prompt.md"), prompt)?;
        self.log(&format!("Session {label} started"));
        let watch = WatchConfig {
            turns: ctx.turns.unwrap_or(self.params.turn_budget),
            context_tokens: self.ledger.context_budget_tokens(ctx.context_tokens),
            idle: role.edits_code(),
            wrap_up: role.wrap_up(),
        };
        if let Some(dial) = &self.deps.effort {
            dial.set_lowered(self.ledger.lower_effort);
            if self.ledger.lower_effort {
                self.log(&format!(
                    "  {label}: the last reply stopped at the output limit; this session reasons at a lower effort"
                ));
            }
        }
        if let Some(sampler) = &self.sampler {
            sampler.begin(&label).await;
        }
        let (metrics, requests) = self
            .run_harness(
                &label,
                &[prompt.to_owned()],
                self.project.root().to_path_buf(),
                Some(watch),
                true,
                ctx.task.as_ref().filter(|_| role == Role::Builder),
            )
            .await?;
        let memory = match &self.sampler {
            Some(sampler) => sampler.end().await,
            None => None,
        };
        let reverted = self.enforce_ownership(role, ctx.task.as_ref())?;
        self.check_skills()?;
        if !reverted.is_empty() {
            self.log(&format!(
                "  {label}: reverted out-of-role changes: {}",
                reverted.join(", ")
            ));
        }
        let ok = metrics.ok();
        let record = SessionRecord {
            label: label.clone(),
            role,
            task: ctx.task.clone(),
            attempt: ctx.attempt,
            ok,
            abort_reason: metrics.abort_reason.clone(),
            exit_code: metrics.exit_code,
            last_stop_reason: metrics.last_stop_reason.clone(),
            turns: metrics.turns,
            tool_calls: metrics.tool_calls,
            writes: metrics.writes,
            usage: metrics.usage,
            reasoning_chars: metrics.reasoning_chars,
            context_peak: metrics.context_peak,
            context_budget: watch.context_tokens,
            compactions: metrics.compactions,
            provider_errors: metrics.provider_errors,
            capacity_limits: metrics.capacity_limits.clone(),
            image_refusal: metrics.image_refusal.clone(),
            big_outputs: metrics.big_outputs,
            watchdog: metrics.events.clone(),
            reverted,
            prompt_chars: prompt.len(),
            duration_ms: metrics.duration_ms,
            engine: engine_summary(&requests),
            memory,
            harness_exit: metrics.harness_exit.clone(),
        };
        self.journal(JournalRecord::Session(record.clone()));
        self.ledger.inflight = None;
        self.ledger.lower_effort = metrics.output_truncated();
        self.save()?;
        let outcome = if ok {
            "settled".to_owned()
        } else {
            let why = metrics
                .abort_reason
                .clone()
                .or_else(|| {
                    metrics
                        .last_stop_reason
                        .as_ref()
                        .map(|r| r.as_str().to_owned())
                })
                .or_else(|| metrics.exit_code.map(|c| c.to_string()))
                .unwrap_or_default();
            format!("failed ({why})")
        };
        self.log(&format!(
            "Session {label} {outcome} after {} turns, peak context {}",
            metrics.turns, metrics.context_peak
        ));
        Ok(SessionResult { record, metrics })
    }

    /// A session with the task loop's recovery around it: continuations
    /// after context stops and refused images, capacity caps, and engine
    /// retries with backoff. None of these charge an attempt.
    pub(crate) async fn session(
        &mut self,
        role: Role,
        prompt: &str,
        ctx: &SessionCtx,
    ) -> Result<SessionResult, RunError> {
        let mut retry = 0;
        let mut capacity = 0;
        let mut continued = 0;
        let mut image_stopped: Option<String> = None;
        let mut turns_stopped = false;
        loop {
            let resumed = std::mem::take(&mut self.ledger.pending_continuation);
            if resumed {
                self.save()?;
            }
            let note = match (&image_stopped, turns_stopped) {
                (Some(refusal), _) => image_continuation(refusal),
                (None, true) => TURN_CONTINUATION.to_owned(),
                (None, false) => CONTINUATION.to_owned(),
            };
            let text = if continued > 0 || resumed {
                format!("{}\n\n{note}", without_map(prompt))
            } else {
                prompt.to_owned()
            };
            let result = self.session_once(role, &text, ctx).await?;
            let label = result.record.label.clone();
            if self.stopping() {
                return Err(self.pause(&label, role, ctx)?);
            }
            let metrics = &result.metrics;
            if !metrics.engine_failed() {
                self.note_engine_held(&label, result.record.memory.as_ref())?;
            }
            if metrics.context_stopped() && continued < self.params.context_continuations {
                continued += 1;
                turns_stopped = false;
                self.journal(JournalRecord::ContextContinuation {
                    label: label.clone(),
                    context_peak: result.record.context_peak,
                    context_budget: result.record.context_budget,
                    continuation: continued,
                });
                self.log(&format!("  {label}: stopped at its context budget; a fresh session continues the same work ({continued}/{}, not counted as an attempt)", self.params.context_continuations));
                continue;
            }
            if let Some(refusal) = &metrics.image_refusal
                && continued < self.params.context_continuations
            {
                continued += 1;
                turns_stopped = false;
                image_stopped = Some(refusal.clone());
                self.journal(JournalRecord::ImageContinuation {
                    label: label.clone(),
                    refusal: refusal.chars().take(300).collect(),
                    continuation: continued,
                });
                self.log(&format!("  {label}: the engine refused an image the session read; a fresh session continues the same work ({continued}/{}, not counted as an attempt)", self.params.context_continuations));
                continue;
            }
            // A session that edits files and ran out of turns was working, not
            // looping (the loop, repeat and idle rules stop those first): its
            // changes are kept, like a context stop, and a fresh session
            // finishes them instead of the attempt counting as failed.
            if metrics.turns_stopped()
                && role.edits_code()
                && continued < self.params.context_continuations
            {
                continued += 1;
                turns_stopped = true;
                image_stopped = None;
                self.journal(JournalRecord::TurnContinuation {
                    label: label.clone(),
                    turns: metrics.turns,
                    continuation: continued,
                });
                self.log(&format!("  {label}: stopped at its turn budget; a fresh session continues the same work ({continued}/{}, not counted as an attempt)", self.params.context_continuations));
                continue;
            }
            image_stopped = None;
            if !metrics.engine_failed() {
                return Ok(result);
            }
            // A prompt larger than the engine's current memory allows fails the same way
            // after any wait. If the reported maximum is still usable, lower the budget and
            // rerun at once; a tiny maximum means something else holds the memory, so wait.
            if let Some(limit) = metrics.capacity_limit()
                && limit >= self.params.context_floor
            {
                if capacity >= self.params.capacity_retries {
                    return Err(RunError::EngineUnavailable(format!(
                        "sessions still overflow the engine's memory after the context budget was lowered to {} (last: {label})",
                        self.ledger.context_cap.unwrap_or(0)
                    )));
                }
                capacity += 1;
                self.lower_context_cap(&label, limit, capacity)?;
                self.deps.clock.sleep(Duration::from_secs(10)).await;
                continue;
            }
            if retry >= self.params.engine_retries {
                return Err(RunError::EngineUnavailable(format!(
                    "{} sessions in a row ended with engine errors (last: {label})",
                    retry + 1
                )));
            }
            let cause = if result.metrics.harness_exit.is_some() {
                "harness"
            } else {
                "engine"
            };
            self.engine_backoff(&label, retry, cause).await;
            retry += 1;
            if self.stopping() {
                return Err(self.pause(&label, role, ctx)?);
            }
            self.wait_for_engine().await;
        }
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
    /// back to the configured one, or to the ceiling a failed raise set. The
    /// engine only says how much it can hold when a prompt overflows it, so
    /// memory freed later is found by trying. Each raise that overflows again
    /// doubles the sessions the next one waits for, up to four times as many;
    /// a raise the engine held starts the count again.
    fn note_engine_held(
        &mut self,
        label: &str,
        memory: Option<&MemoryWindow>,
    ) -> Result<(), RunError> {
        let Some(cap) = self.ledger.context_cap else {
            return Ok(());
        };
        if self.params.capacity_recover_sessions == 0 {
            return Ok(());
        }
        if memory.is_some_and(|m| m.pressure_max >= CRITICAL_PRESSURE) {
            self.ledger.cap_clean_sessions = 0;
            return self.save();
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
        let full = self.ledger.configured_budget_tokens();
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
    async fn engine_backoff(&mut self, label: &str, retry: u32, cause: &str) {
        let wait = (self.params.engine_retry_base_sec * 2u64.pow(retry)).min(900);
        self.journal(JournalRecord::EngineRetry {
            label: label.to_owned(),
            retry: retry + 1,
            wait_s: wait,
            cause: Some(cause.to_owned()),
        });
        let what = if cause == "harness" {
            "the harness ended before the turn settled"
        } else {
            "engine error"
        };
        self.log(&format!("  {label}: {what}; retrying the same session in {wait} s ({}/{}, not counted as an attempt)", retry + 1, self.params.engine_retries));
        if retry == 0 && cause == "engine" {
            self.notify("The engine is failing (often memory). Free memory; the run retries the same session.");
        }
        tokio::select! {
            () = self.deps.clock.sleep(Duration::from_secs(wait)) => {}
            () = self.stop.cancelled() => {}
        }
    }
}

/// One model response to record.
struct RequestInput {
    usage: crate::session::Usage,
    stop_reason: crate::session::StopReason,
    server: Option<crate::session::ServerTimings>,
    sent: Option<crate::session::RequestSettings>,
    timing: ClientTiming,
}

/// Records requests off the session loop: reading the engine's counters may
/// take a second per response, and the loop must keep up with the harness.
struct RequestRecorder {
    tx: Option<tokio::sync::mpsc::UnboundedSender<RequestInput>>,
    task: tokio::task::JoinHandle<Vec<RequestRecord>>,
}

impl RequestRecorder {
    fn start(
        label: &str,
        project: Option<crate::project::Project>,
        source: Option<Arc<dyn crate::ports::EngineMetricsSource>>,
        clock: Arc<dyn crate::ports::Clock>,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RequestInput>();
        let label = label.to_owned();
        let task = tokio::spawn(async move {
            let mut before: Option<EngineCounters> = match &source {
                Some(source) => source.counters().await,
                None => None,
            };
            let mut requests: Vec<RequestRecord> = Vec::new();
            let mut matched_once = false;
            while let Some(input) = rx.recv().await {
                // Wait up to a second for the counters only once the engine has
                // been seen serving this session; otherwise a foreign engine's
                // counters would cost a second per response.
                let waits = if matched_once { 10 } else { 2 };
                let engine = match (&source, before.clone()) {
                    (Some(source), Some(prior)) => {
                        engine_delta(source.as_ref(), clock.as_ref(), &prior, &mut before, waits)
                            .await
                    }
                    _ => None,
                };
                let sequence = u32::try_from(requests.len() + 1).unwrap_or(u32::MAX);
                let mut record = request_record(
                    &label,
                    sequence,
                    &input.usage,
                    &input.stop_reason,
                    input.timing,
                )
                .with_engine(engine);
                record.server = input.server;
                record.sent = input.sent;
                matched_once |= record.engine_matched;
                if let Some(project) = &project
                    && let Ok(line) = serde_json::to_string(&record)
                {
                    let _ = project.append(".tenaz/private/requests.jsonl", &format!("{line}\n"));
                }
                requests.push(record);
            }
            requests
        });
        Self { tx: Some(tx), task }
    }

    fn record(&self, input: RequestInput) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(input);
        }
    }

    /// Closes the queue and waits for every record.
    async fn finish(mut self) -> Vec<RequestRecord> {
        self.tx.take();
        self.task.await.unwrap_or_default()
    }
}

/// The engine counters' change since the previous response. The engine may
/// publish them just after the stream closes, so wait briefly.
async fn engine_delta(
    source: &dyn crate::ports::EngineMetricsSource,
    clock: &dyn crate::ports::Clock,
    prior: &EngineCounters,
    before: &mut Option<EngineCounters>,
    waits: u32,
) -> Option<EngineCounters> {
    let mut now = source.counters().await;
    for _ in 0..waits {
        match &now {
            Some(n) if n.get("requests") == prior.get("requests") => {
                clock.sleep(Duration::from_millis(100)).await;
                now = source.counters().await;
            }
            _ => break,
        }
    }
    let now = now?;
    let change = delta(&now, prior);
    *before = Some(now);
    Some(change)
}

/// The prompt for a builder session that ended with no passing check of its
/// task.
fn no_own_checks(task: &TaskId, summary: &str) -> String {
    format!(
        "[orchestrator] `./verify --task {task}` runs no passing check with ID prefix `{task}.`, so this attempt fails however the rest went. Add the card's acceptance checks to `verify` with IDs `{task}.<name>`, run `./verify --task {task}` until they pass, then stop.\n\n{summary}"
    )
}

/// Sessions the engine must hold before a lowered budget is raised: the
/// configured count, doubled per raise that failed, at most four times it.
fn raise_wait(recover_sessions: u32, failed_raises: u32) -> u32 {
    recover_sessions.saturating_mul(1 << failed_raises.min(2))
}

/// The budget after a raise from `cap`: halfway to the configured `full`
/// budget, or to `ceiling` when that is lower, and all the way when less
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

/// How much of the harness's stderr a harness-exit note quotes.
const STDERR_TAIL_BYTES: usize = 2000;

/// What the journal says when the harness ended before its turn settled.
fn harness_exit_note(code: Option<i32>, stderr_log: Option<&std::path::Path>) -> String {
    let code = code.map_or_else(|| "no exit code".to_owned(), |c| format!("exit code {c}"));
    let tail = stderr_log
        .and_then(|path| std::fs::read(path).ok())
        .map(|bytes| {
            let start = bytes.len().saturating_sub(STDERR_TAIL_BYTES);
            String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
        })
        .filter(|t| !t.is_empty());
    match tail {
        Some(tail) => {
            format!("the harness ended before the turn settled ({code}); stderr tail:\n{tail}")
        }
        None => format!("the harness ended before the turn settled ({code}); its stderr was empty"),
    }
}

/// Why a session stopped when its last reply was cut at the output limit,
/// for the next attempt's briefing.
const OUTPUT_LIMIT: &str = "the reply reached the output limit while the model was still reasoning, before it acted; reason briefly, act in small steps and write files early";

/// Why a session stopped when its harness crashed during `call`, for the
/// next attempt's briefing.
fn crashed_running(call: &str, note: &str) -> String {
    let mut reason =
        format!("the harness crashed while running this tool call; do not repeat it: {call}");
    if note.contains("valid UTF-8") {
        reason.push_str(". Its output had bytes that are not valid UTF-8, which the harness cannot read: do not cut output by bytes (`head -c`, `cut -b`, `dd`); cut by lines (`head -n`, `tail -n`) instead");
    }
    reason
}

/// Client-side timings of one turn, in monotonic milliseconds.
#[derive(Clone, Copy, Debug)]
struct ClientTiming {
    turn_started: Option<u64>,
    first_token: Option<u64>,
    now: u64,
}

fn seconds_between(from: u64, to: u64) -> f64 {
    f64::from(u32::try_from(to.saturating_sub(from)).unwrap_or(u32::MAX)) / 1000.0
}

/// The harness's view of one model response; engine deltas are attached by the caller.
fn request_record(
    label: &str,
    sequence: u32,
    usage: &crate::session::Usage,
    stop_reason: &crate::session::StopReason,
    timing: ClientTiming,
) -> RequestRecord {
    RequestRecord {
        session: label.to_owned(),
        sequence,
        stop_reason: Some(stop_reason.as_str().to_owned()),
        prompt_tokens: usage.input + usage.cache_read,
        cached_tokens: usage.cache_read,
        output_tokens: usage.output,
        reasoning_tokens: usage.reasoning,
        client_first_token_s: timing
            .turn_started
            .zip(timing.first_token)
            .map(|(t, f)| seconds_between(t, f)),
        client_total_s: timing.turn_started.map(|t| seconds_between(t, timing.now)),
        engine: None,
        engine_matched: false,
        server: None,
        sent: None,
    }
}

/// The repository facts the ownership rules need, read from git.
struct RepoView<'a>(&'a dyn crate::ports::Vcs);

impl ownership::RepoFacts for RepoView<'_> {
    fn in_head(&self, path: &str) -> bool {
        self.0.in_head(path)
    }
    fn head_content(&self, path: &str) -> Option<String> {
        self.0.head_content(path)
    }
}

fn event_name(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::PromptRejected { .. } => "prompt_rejected",
        SessionEvent::TurnStart => "turn_start",
        SessionEvent::FirstToken => "first_token",
        SessionEvent::ToolCallStart { .. } => "tool_call_start",
        SessionEvent::ToolCallEnd { .. } => "tool_call_end",
        SessionEvent::MessageEnd { .. } => "message_end",
        SessionEvent::TurnEnd => "turn_end",
        SessionEvent::CompactionStart { .. } => "compaction_start",
        SessionEvent::PermissionRequest { .. } => "permission_request",
        SessionEvent::Settled { .. } => "settled",
        SessionEvent::Exited { .. } => "exited",
    }
}

fn describe(action: &Action) -> String {
    match action {
        Action::Steer { key, .. } => format!("steer:{key}"),
        Action::Abort { reason } => format!("abort — {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{StopReason, Usage};

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

    /// Pi reports reasoning per message; the request row keeps it, and rows
    /// written before the field existed still read.
    #[test]
    fn request_rows_keep_reasoning_tokens() {
        let usage = Usage {
            input: 1871,
            output: 111,
            cache_read: 2048,
            reasoning: 57,
            ..Default::default()
        };
        let timing = ClientTiming {
            turn_started: Some(1_000),
            first_token: Some(4_424),
            now: 9_420,
        };
        let row = request_record("001-bootstrap", 1, &usage, &StopReason::ToolUse, timing);
        assert_eq!(
            (
                row.prompt_tokens,
                row.cached_tokens,
                row.output_tokens,
                row.reasoning_tokens
            ),
            (3919, 2048, 111, 57)
        );
        assert_eq!(row.client_first_token_s, Some(3.424));
        let old: RequestRecord = serde_json::from_str(
            r#"{"session":"s","sequence":1,"stop_reason":"stop","prompt_tokens":1,"cached_tokens":0,"output_tokens":1,"client_first_token_s":null,"client_total_s":null,"engine":null,"engine_matched":false}"#,
        )
        .unwrap();
        assert_eq!(old.reasoning_tokens, 0);
    }
}
