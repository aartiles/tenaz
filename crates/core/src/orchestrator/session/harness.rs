//! One run of the harness: open the session, send the prompts, feed the
//! watchdog, answer permissions, steer, interrupt and abort, then close it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::metrics::RequestRecord;
use crate::orchestrator::phases::{AttemptFailure, PlannerSession, error_list};
use crate::orchestrator::{RunError, Runner};
use crate::ports::{Clock, PermissionDecision, Session, SessionModel, SessionSpec};
use crate::project::conflict_path;
use crate::role::Role;
use crate::session::{SessionEvent, SessionMetrics};
use crate::task::TaskId;
use crate::util::clip;
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
    /// The model the session runs.
    pub model: SessionModel,
    pub watch: Option<WatchConfig>,
    /// Keep the raw stream and the harness state under `.tenaz/private/`.
    pub persist: bool,
    /// A builder's task: a session that answered every prompt while
    /// `./verify --task` runs no passing check of it gets one more prompt
    /// asking for them; one that left a conflict file while its attempt
    /// would pass gets one more saying the file does not stand (ADR 0034).
    pub builder_task: Option<&'a TaskId>,
    /// A split's or replan's role and backlog from before it: a session
    /// that answered every prompt having left cards Tenaz would throw away
    /// gets one more prompt naming why (ADR 0033), and a replan that set a
    /// blocked task back to `todo` that Tenaz would block again gets one
    /// more prompt saying so (ADR 0018).
    pub planner: Option<PlannerSession<'a>>,
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

/// A builder session's task and the prompts it was already sent after
/// answering every prompt; each is sent at most once per harness session.
struct BuilderFollowUps<'a> {
    task: &'a TaskId,
    asked_checks: bool,
    asked_conflict: bool,
}

/// A planner session and the prompts it was already sent after answering
/// every prompt; each is sent at most once per harness session.
struct PlannerFollowUps<'a> {
    session: PlannerSession<'a>,
    asked_cards: bool,
    asked_reopens: bool,
}

/// A running session and what the loop keeps about it.
struct SessionLoop<'a> {
    runner: &'a Runner,
    label: &'a str,
    prompts: &'a [String],
    /// Sent so far.
    prompt_index: usize,
    /// A builder session's follow-ups (`HarnessRun::builder_task`).
    builder: Option<BuilderFollowUps<'a>>,
    /// A planner session's follow-ups (`HarnessRun::planner`).
    planner: Option<PlannerFollowUps<'a>>,
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
    /// How to open the session of `run`; `persist` keeps its logs and
    /// state under `.tenaz/private/`.
    fn session_spec(&self, run: &HarnessRun<'_>) -> SessionSpec {
        let private = self.project.private_dir();
        let label = run.label;
        let log = |suffix: &str| {
            run.persist
                .then(|| private.join(format!("{label}.{suffix}")))
        };
        SessionSpec {
            label: label.to_owned(),
            cwd: run.cwd.clone(),
            persist_dir: run.persist.then(|| private.join("sessions")),
            raw_log: log("rpc.jsonl"),
            thinking_log: log("thinking.md"),
            stderr_log: log("stderr.log"),
            model: run.model.clone(),
        }
    }

    /// Runs the harness once, until its prompts are answered, it is
    /// stopped, or it ends.
    pub(crate) async fn run_harness(
        &self,
        run: HarnessRun<'_>,
    ) -> Result<(SessionMetrics, Vec<RequestRecord>), RunError> {
        let spec = self.session_spec(&run);
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
            builder: run.builder_task.map(|task| BuilderFollowUps {
                task,
                asked_checks: false,
                asked_conflict: false,
            }),
            planner: run.planner.map(|session| PlannerFollowUps {
                session,
                asked_cards: false,
                // A split has no reopen to ask about (ADR 0018): once told
                // about its cards, it is not judged again.
                asked_reopens: session.role != Role::Replan,
            }),
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

    /// The next prompt for a builder session that answered every prompt:
    /// about the conflict file of its task, when it left one; else about
    /// its missing checks. A builder that declared a conflict is never
    /// asked for checks: asking it for a passing check would talk it out
    /// of the conflict (ADR 0009).
    async fn builder_follow_up(&mut self) -> Option<(&'static str, String)> {
        let task = self.builder.as_ref()?.task;
        if self.runner.project.conflict_text(task.as_str()).is_some() {
            return self
                .passing_conflict_prompt(task)
                .await
                .map(|message| (PASSING_CONFLICT_NOTE, message));
        }
        self.own_checks_prompt(task)
            .await
            .map(|message| (OWN_CHECKS_NOTE, message))
    }

    /// The prompt for a builder that left a conflict file of `task` while
    /// its checks would pass as Tenaz judges them (`Runner::verify_attempt`):
    /// a conflict is read only when the attempt's checks fail, so the file
    /// does not stand, and the builder is told while it still holds its
    /// work (ADR 0034). `None` when the checks would fail, so the conflict
    /// is read, or after the first time in this session.
    async fn passing_conflict_prompt(&mut self, task: &TaskId) -> Option<String> {
        if self.builder.as_ref()?.asked_conflict {
            return None;
        }
        // A conflict is read only when the checks fail; changes `verify`
        // made to the working tree (ADR 0035) do not make it stand.
        let failure = self.runner.verify_attempt(task).await.failure;
        if matches!(failure, Some(AttemptFailure::Checks(_))) {
            return None;
        }
        self.builder.as_mut()?.asked_conflict = true;
        self.runner.log(&format!(
            "  {}: {} left while its checks pass, so it does not stand; asking once more",
            self.label,
            conflict_path(task.as_str())
        ));
        Some(passing_conflict(task))
    }

    /// The prompt for a builder session while `./verify --task` runs no
    /// passing check of `task`: the attempt would fail on that alone, and
    /// the session still holds what it learned, so it adds them now. `None`
    /// when it does, or after the first time in this session.
    async fn own_checks_prompt(&mut self, task: &TaskId) -> Option<String> {
        if self.builder.as_ref()?.asked_checks {
            return None;
        }
        self.builder.as_mut()?.asked_checks = true;
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

    /// The next prompt for a split or replan session that answered every
    /// prompt, from one judgement of its cards as Tenaz judges them when it
    /// ends (`Runner::judge_planner`): first the errors that would get its
    /// cards thrown away, so it fixes them while it still holds its plan
    /// (ADR 0033); then the blocked tasks a replan set back to `todo` that
    /// Tenaz would block again, so the run does not find no task ready (ADR
    /// 0018). Each is sent at most once per session.
    fn planner_follow_up(&mut self) -> Option<(&'static str, String)> {
        let planner = self.planner.as_mut()?;
        if planner.asked_cards && planner.asked_reopens {
            return None;
        }
        let judgement = self.runner.judge_planner(planner.session);
        if !planner.asked_cards && !judgement.errors.is_empty() {
            planner.asked_cards = true;
            let errors = &judgement.errors;
            let limit = self.runner.params.note_clip_bytes;
            self.runner.log(&format!(
                "  {}: its cards would be thrown away ({} errors: {}); asking once more",
                self.label,
                errors.len(),
                clip(&errors.join("; "), limit).replace('\n', " ")
            ));
            return Some((INVALID_CARDS_NOTE, invalid_cards(errors, limit)));
        }
        let refused: Vec<String> = judgement
            .refused_reopens()
            .map(ToString::to_string)
            .collect();
        if planner.asked_reopens || refused.is_empty() {
            return None;
        }
        planner.asked_reopens = true;
        let refused = refused.join(", ");
        self.runner.log(&format!(
            "  {}: {refused} set back to todo, but Tenaz would block each again; asking once more",
            self.label
        ));
        Some((REFUSED_REOPENS_NOTE, refused_reopens(&refused)))
    }

    /// The next prompt a session that answered every prompt gets, with the
    /// watchdog's name for it: a builder's conflict that does not stand, or
    /// its missing checks; a planner's cards Tenaz would throw away; a
    /// replan's refused reopens. A planner may get the last two, one after
    /// the other.
    async fn follow_up(&mut self) -> Option<(&'static str, String)> {
        if let Some(follow_up) = self.builder_follow_up().await {
            return Some(follow_up);
        }
        self.planner_follow_up()
    }

    /// Sends the first prompt, then handles events until the session is
    /// done, killed, or its stream ends.
    async fn run(&mut self) -> Result<(), RunError> {
        if let Some(first) = self.prompts.first() {
            self.send(first).await?;
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
            self.send(&note).await?;
            return Ok(Flow::Go);
        }
        if aborted || self.ending() {
            return Ok(Flow::Stop);
        }
        if let Some(next) = self.prompts.get(self.prompt_index) {
            self.send(next).await?;
            self.prompt_index += 1;
            return Ok(Flow::Go);
        }
        let Some((kind, message)) = self.follow_up().await else {
            return Ok(Flow::Stop);
        };
        self.watchdog
            .note(kind, self.clock.monotonic_ms().saturating_sub(self.started));
        self.send(&message).await?;
        Ok(Flow::Go)
    }

    /// Sends `text` as the session's next prompt.
    async fn send(&mut self, text: &str) -> Result<(), RunError> {
        self.watchdog.prompted();
        Ok(self.session.prompt(text).await?)
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

/// The watchdog's name for the prompt asking a builder for its own checks.
pub(crate) const OWN_CHECKS_NOTE: &str = "own-checks";

/// What the prompt about a conflict that does not stand says; the scripted
/// harness knows it by this.
pub(crate) const PASSING_CONFLICT: &str = "a conflict stands only when the attempt's checks fail";

/// The watchdog's name for that prompt in the session's record.
pub(crate) const PASSING_CONFLICT_NOTE: &str = "passing-conflict";

/// The prompt for a builder that left a conflict file of `task` while its
/// checks pass (ADR 0034).
fn passing_conflict(task: &TaskId) -> String {
    let path = conflict_path(task.as_str());
    format!(
        "[orchestrator] You left `{path}`, but this attempt's checks pass, and {PASSING_CONFLICT}: as things stand Tenaz removes the file, and no card may amend the checks it names. If {task} cannot be done as its card and the spec say without changing those checks, make {task}'s checks test what the card asks, even though they then fail, keep the file, and stop: {task} is blocked, this attempt's work is undone, and a replan may amend those checks. Otherwise delete the file, say in your notes what, if anything, the card asks that the work does not do, and stop."
    )
}

/// What the prompt about cards Tenaz would throw away says first; the
/// scripted harness knows it by this.
pub(crate) const INVALID_CARDS: &str =
    "When this session ends, Tenaz throws away all your changes to the task cards";

/// The watchdog's name for that prompt in the session's record.
pub(crate) const INVALID_CARDS_NOTE: &str = "invalid-cards";

/// The watchdog's name for the prompt about refused reopens in the
/// session's record.
pub(crate) const REFUSED_REOPENS_NOTE: &str = "refused-reopens";

/// The prompt for a planner session whose cards have `errors`, the list
/// cut to `limit` bytes.
fn invalid_cards(errors: &[String], limit: usize) -> String {
    format!(
        "[orchestrator] {INVALID_CARDS}, as they stand now, because:\n{}\nFix each, keep the rest of your plan, update your notes, and stop.",
        error_list(errors, limit)
    )
}

/// What the prompt about refused reopens says first; the scripted harness
/// knows it by this.
pub(crate) const REFUSED_REOPENS: &str = "when this session ends Tenaz blocks each again";

/// The prompt for a replan session that set the blocked tasks `refused`
/// back to `todo` with no amend to wait for.
fn refused_reopens(refused: &str) -> String {
    format!(
        "[orchestrator] You set {refused} back to `todo`, but {REFUSED_REOPENS}: a blocked task goes back to `todo` only while another card still to build lists in `amends` a check named in the task's `.tenaz/conflicts/<ID>.md`, and one with no such file cannot go back at all. For each, either add those checks to the `amends` of the card that changes what they test, or route around it: set it to `status: dropped` and give its work to new cards. Then stop."
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
        SessionEvent::LateText { .. } => "late_text",
        SessionEvent::CompactionStart { .. } => "compaction_start",
        SessionEvent::CompactionRefused => "compaction_refused",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each error is on a line of its own, and a long list is cut.
    #[test]
    fn the_invalid_cards_prompt_lists_each_error() {
        let errors = [
            "card T002 was deleted".to_owned(),
            "T005: amends `T001.x`".to_owned(),
        ];
        let prompt = invalid_cards(&errors, 1_000);
        assert!(
            prompt.contains("because:\n- card T002 was deleted\n- T005: amends `T001.x`\n"),
            "{prompt}"
        );
        let clipped = invalid_cards(&errors, 30);
        assert!(clipped.contains("- card T002 was deleted"), "{clipped}");
        assert!(!clipped.contains("T001.x"), "{clipped}");
    }
}
