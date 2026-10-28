//! One run of the harness: open the session, send the prompts, feed the
//! watchdog, answer permissions, steer, interrupt and abort, then close it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::metrics::RequestRecord;
use crate::orchestrator::{RunError, Runner};
use crate::ports::{Clock, PermissionDecision, Session, SessionSpec};
use crate::session::{SessionEvent, SessionMetrics};
use crate::task::TaskId;
use crate::verify::retarget;
use crate::watchdog::{Action, WatchConfig, Watchdog};

use super::requests::{RequestRecorder, TurnTiming};

/// The grace given to a harness between an abort and a kill, in
/// milliseconds.
const ABORT_GRACE_MS: u64 = 60_000;

/// The grace given to a harness between a stop (Ctrl-C) and a kill, in
/// milliseconds.
const STOP_GRACE_MS: u64 = 5_000;

/// How much of the harness's stderr a harness-exit note quotes.
const STDERR_TAIL_BYTES: usize = 2000;

/// Why a session stopped when its last reply was cut at the output limit,
/// for the next attempt's briefing.
const OUTPUT_LIMIT: &str = "the reply reached the output limit while the model was still reasoning, before it acted; reason briefly, act in small steps and write files early";

/// One run of the harness.
pub(crate) struct HarnessRun<'a> {
    pub label: &'a str,
    /// Sent one after another (the preflight sends several).
    pub prompts: &'a [String],
    pub cwd: PathBuf,
    pub watch: Option<WatchConfig>,
    /// Keep the raw stream and the harness state under `.tenaz/private/`.
    pub persist: bool,
    /// A builder's task: a session that answered every prompt while
    /// `./verify --task` runs no passing check of it gets one more prompt
    /// asking for them.
    pub own_checks: Option<&'a TaskId>,
}

/// What the session loop does after an event or a wake-up.
#[derive(PartialEq, Eq)]
enum Flow {
    Go,
    Stop,
}

/// Why the wait for the next event ended without one.
enum Woke {
    /// A deadline or a stop was handled; look again.
    Again,
    /// The kill timer fired.
    Killed,
}

/// A running session and what the loop keeps about it.
struct SessionLoop<'a> {
    runner: &'a Runner,
    label: &'a str,
    prompts: &'a [String],
    /// Sent so far.
    prompt_index: usize,
    own_checks: Option<&'a TaskId>,
    session: Box<dyn Session>,
    watchdog: Watchdog,
    clock: Arc<dyn Clock>,
    started: u64,
    recorder: RequestRecorder,
    timing: TurnTiming,
    /// When the session is killed, once it was aborted or stopped.
    kill_at: Option<u64>,
    /// What to prompt once a turn stopped at its tool time limit settles.
    resume: Option<String>,
}

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

    /// Runs the harness once, until its prompts are answered, it is
    /// stopped, or it ends.
    pub(crate) async fn run_harness(
        &self,
        run: HarnessRun<'_>,
    ) -> Result<(SessionMetrics, Vec<RequestRecord>), RunError> {
        let spec = self.session_spec(run.label, run.cwd, run.persist);
        let stderr_log = spec.stderr_log.clone();
        let session = self.deps.harness.open(spec).await?;
        let mut watchdog = Watchdog::new(self.params.clone(), run.watch);
        if run.watch.is_some_and(|w| w.idle) {
            watchdog.set_fingerprint(self.deps.vcs.worktree_fingerprint().ok());
        }
        // Only watched sessions run in the project; the preflight runs in a
        // directory of its own.
        watchdog.set_write_scope(run.watch.and(self.deps.write_scope.clone()));
        let clock = self.deps.clock.clone();
        let started = clock.monotonic_ms();
        let recorder = RequestRecorder::start(
            run.label,
            run.persist.then(|| self.project.clone()),
            self.deps.engine_metrics.clone(),
            clock.clone(),
        );
        let mut session = SessionLoop {
            runner: self,
            label: run.label,
            prompts: run.prompts,
            prompt_index: 0,
            own_checks: run.own_checks,
            session,
            watchdog,
            clock,
            started,
            recorder,
            timing: TurnTiming::default(),
            kill_at: None,
            resume: None,
        };
        session.run().await?;
        Ok(session.finish(stderr_log.as_deref()).await)
    }
}

impl SessionLoop<'_> {
    /// What a passed deadline asks, logged: the session's wall clock, then
    /// the oldest tool call's time limit. `None` before either.
    fn deadline_actions(&mut self, elapsed: u64) -> Option<Vec<Action>> {
        let minutes = self.runner.params.session_minutes;
        let actions = if elapsed >= minutes * 60_000 {
            self.watchdog
                .abort_external(&format!("session exceeded {minutes} minutes"), elapsed)
        } else if self.watchdog.tool_deadline().is_some_and(|d| elapsed >= d) {
            self.watchdog.tool_timeout(elapsed)
        } else {
            return None;
        };
        for action in &actions {
            self.runner
                .log(&format!("  {}: {}", self.label, describe(action)));
        }
        Some(actions)
    }

    /// The prompt for a builder session that answered every prompt while
    /// `./verify --task` runs no passing check of its task; `None` when it
    /// does, or after the first time. The attempt would fail on that alone,
    /// and the session still holds what it learned, so it adds them now.
    async fn own_checks_prompt(&mut self) -> Option<String> {
        let task = self.own_checks.take()?;
        let own = self.runner.run_verify(&["--task", task.as_str()]).await;
        if own.passed() > 0 {
            return None;
        }
        self.runner.log(&format!(
            "  {}: no passing {task} check; asking for one",
            self.label
        ));
        Some(retarget(
            &no_own_checks(task, &own.summary),
            self.runner.params.verify_path(),
        ))
    }

    /// Sends the first prompt, then handles events until the session is
    /// done, killed, or its stream ends.
    async fn run(&mut self) -> Result<(), RunError> {
        if let Some(first) = self.prompts.first() {
            self.session.prompt(first).await?;
            self.prompt_index = 1;
        }
        loop {
            let now = self.clock.monotonic_ms();
            let elapsed = now.saturating_sub(self.started);
            // Deadlines are checked here, not only in the timers below: with a
            // backlog of ready events a fresh timer future never gets to fire.
            if self.kill_at.is_some_and(|k| now >= k) {
                tracing::debug!("{}: kill deadline passed", self.label);
                return Ok(());
            }
            if self.kill_at.is_none()
                && let Some(actions) = self.deadline_actions(elapsed)
            {
                self.apply(actions, now).await;
                continue;
            }
            let event = match self.wait(elapsed, now).await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    tracing::debug!("{}: event stream ended", self.label);
                    return Ok(());
                }
                Err(Woke::Again) => continue,
                Err(Woke::Killed) => return Ok(()),
            };
            if self.on_event(event, elapsed).await? == Flow::Stop {
                return Ok(());
            }
        }
    }

    /// Waits for the next event (`None` when the harness's stream ended),
    /// or until the kill timer, the session's deadline or a stop.
    async fn wait(&mut self, elapsed: u64, now: u64) -> Result<Option<SessionEvent>, Woke> {
        // The session's wall clock, or sooner the oldest tool call's
        // limit: a hung call sends no event to wake on.
        let wall_ms = self.runner.params.session_minutes * 60_000;
        let deadline = self
            .watchdog
            .tool_deadline()
            .map_or(wall_ms, |d| d.min(wall_ms));
        let deadline_left = Duration::from_millis(deadline.saturating_sub(elapsed));
        let kill_left = Duration::from_millis(self.kill_at.map_or(0, |k| k.saturating_sub(now)));
        let clock = self.clock.clone();
        tokio::select! {
            event = self.session.next_event() => Ok(event),
            () = clock.sleep(kill_left), if self.kill_at.is_some() => {
                tracing::debug!("{}: kill timer fired", self.label);
                Err(Woke::Killed)
            }
            // What the deadline asks starts when the wait ends: `now` is
            // from before it, and an abort's grace would already be over.
            () = clock.sleep(deadline_left), if self.kill_at.is_none() => {
                let now = clock.monotonic_ms();
                if let Some(actions) = self.deadline_actions(now.saturating_sub(self.started)) {
                    self.apply(actions, now).await;
                }
                Err(Woke::Again)
            }
            () = self.runner.stop.cancelled(), if self.kill_at.is_none() => {
                let _ = self.session.abort().await;
                self.kill_at = Some(clock.monotonic_ms() + STOP_GRACE_MS);
                Err(Woke::Again)
            }
        }
    }

    /// Handles one event: answers a permission, records a response, and
    /// applies what the watchdog asks. `elapsed` is from before the wait.
    async fn on_event(&mut self, event: SessionEvent, elapsed: u64) -> Result<Flow, RunError> {
        // The wait above may have been long; the event's time is now, not
        // the time the wait started.
        let now = self.clock.monotonic_ms();
        tracing::trace!(
            "{}: event {} (kill_at {:?}, now {now})",
            self.label,
            event_name(&event),
            self.kill_at
        );
        match &event {
            SessionEvent::Exited { .. } => return Ok(Flow::Stop),
            SessionEvent::PermissionRequest { id, .. } => {
                let _ = self
                    .session
                    .answer_permission(id, PermissionDecision::Allow)
                    .await;
                return Ok(Flow::Go);
            }
            _ => self.timing.observe(&event, now, &self.recorder),
        }
        let vcs = self.runner.deps.vcs.clone();
        let noted = self.watchdog.metrics().outside_writes.len();
        let actions = self.watchdog.handle(&event, elapsed, move || {
            vcs.worktree_fingerprint().unwrap_or_default()
        });
        for path in &self.watchdog.metrics().outside_writes[noted..] {
            self.runner.log(&format!(
                "  {}: wrote outside the project: {path}",
                self.label
            ));
        }
        for action in &actions {
            self.runner
                .log(&format!("  {}: {}", self.label, describe(action)));
        }
        self.apply(actions, now).await;
        match event {
            SessionEvent::Settled { aborted } => self.on_settled(aborted).await,
            _ => Ok(Flow::Go),
        }
    }

    /// A settled turn: the prompt that resumes an interrupted turn, else
    /// the next prompt, else one asking for the task's own checks; or the
    /// end of the session.
    async fn on_settled(&mut self, aborted: bool) -> Result<Flow, RunError> {
        if let Some(note) = self.resume.take()
            && !self.ending()
        {
            self.session.prompt(&note).await?;
            return Ok(Flow::Go);
        }
        if aborted || self.ending() {
            return Ok(Flow::Stop);
        }
        if let Some(next) = self.prompts.get(self.prompt_index) {
            self.session.prompt(next).await?;
            self.prompt_index += 1;
            return Ok(Flow::Go);
        }
        let Some(message) = self.own_checks_prompt().await else {
            return Ok(Flow::Stop);
        };
        self.watchdog.note(
            "own-checks",
            self.clock.monotonic_ms().saturating_sub(self.started),
        );
        self.session.prompt(&message).await?;
        Ok(Flow::Go)
    }

    /// The watchdog is aborting the session, or the run is stopping.
    fn ending(&self) -> bool {
        self.watchdog.aborting() || self.runner.stopping()
    }

    /// Sends steers and aborts to the session; an abort starts the kill
    /// timer, an interrupt keeps the prompt that resumes the turn.
    async fn apply(&mut self, actions: Vec<Action>, now: u64) {
        let verify_path = self.runner.params.verify_path();
        for action in actions {
            match action {
                Action::Steer { message, .. } => {
                    let message = retarget(&message, verify_path);
                    let _ = self.session.steer(&message).await;
                }
                Action::Interrupt { message } => {
                    let _ = self.session.abort().await;
                    self.resume = Some(retarget(&message, verify_path));
                }
                Action::Abort { .. } => {
                    tracing::debug!(
                        "aborting the session; kill in {:?}",
                        Duration::from_millis(ABORT_GRACE_MS)
                    );
                    let _ = self.session.abort().await;
                    self.kill_at.get_or_insert(now + ABORT_GRACE_MS);
                }
            }
        }
    }

    /// Closes the session (at once when its kill time passed) and gives its
    /// metrics and requests.
    async fn finish(mut self, stderr_log: Option<&Path>) -> (SessionMetrics, Vec<RequestRecord>) {
        let grace = if self.kill_at.is_some_and(|k| self.clock.monotonic_ms() >= k) {
            Duration::ZERO
        } else {
            Duration::from_millis(ABORT_GRACE_MS)
        };
        let exit = self.session.close(grace).await;
        self.watchdog.finish(
            exit.code,
            self.clock.monotonic_ms().saturating_sub(self.started),
        );
        let requests = self.recorder.finish().await;
        let mut metrics = self.watchdog.into_metrics();
        if metrics.ended_early() && !self.runner.stopping() {
            explain_early_end(&mut metrics, exit.code, stderr_log);
        }
        (metrics, requests)
    }
}

/// The harness went away before its turn settled and nobody stopped it.
/// Between tool calls that says nothing about the task, so it is handled
/// like an engine failure (rerun, uncounted). During a tool call, the call
/// ended it and a rerun would make the same call (fx dies on output that is
/// not UTF-8), so the session stops with that call as the reason and the
/// attempt counts. The stderr is kept for diagnosis either way.
fn explain_early_end(metrics: &mut SessionMetrics, code: Option<i32>, stderr_log: Option<&Path>) {
    let note = harness_exit_note(code, stderr_log);
    if let Some(call) = &metrics.tool_in_flight {
        metrics.abort_reason = Some(crashed_running(call, &note));
    } else if metrics.output_truncated() {
        // fx exits when a reply is cut at the output limit; the model
        // was still reasoning, and would again in an unchanged rerun.
        metrics.abort_reason = Some(OUTPUT_LIMIT.to_owned());
    }
    metrics.harness_exit = Some(note);
}

/// The prompt for a builder session that ended with no passing check of its
/// task.
fn no_own_checks(task: &TaskId, summary: &str) -> String {
    format!(
        "[orchestrator] `./verify --task {task}` runs no passing check with ID prefix `{task}.`, so this attempt fails however the rest went. Add the card's acceptance checks to `verify` with IDs `{task}.<name>`, run `./verify --task {task}` until they pass, then stop.\n\n{summary}"
    )
}

/// What the journal says when the harness ended before its turn settled.
fn harness_exit_note(code: Option<i32>, stderr_log: Option<&Path>) -> String {
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
        Action::Interrupt { .. } => {
            "tool call over its time limit; stopped, the session goes on".to_owned()
        }
    }
}
