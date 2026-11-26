//! The watchdog: the task loop's rules over the session event stream. It
//! only decides; the session runner sends the steers and aborts.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::briefing::count_turns;
use crate::novelty::Novelty;
use crate::params::RunParams;
use crate::session::{
    HARNESS_COMPACTION, OUTPUT_LIMIT_LOWERED, PROVIDER_ERROR, SECOND_COMPACTION, SessionEvent,
    SessionMetrics, StopReason, ToolEffect, WatchdogEvent, capacity_limit, command_arg,
    is_image_refusal, is_out_of_memory, is_request_refusal, path_arg, tool_effect,
};
use crate::util::{approx_tokens, sha256_hex};
use crate::write_scope::WriteScope;

/// Requests the context pace is measured over.
const PACE_WINDOW: usize = 6;

/// How much of a tool call still running at exit the metrics keep.
const IN_FLIGHT_CHARS: usize = 400;

/// What a watched session is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    pub turns: u32,
    pub context_tokens: u64,
    /// Count turns without a file change (builder and bootstrap sessions).
    /// Such a session may also run past its turn stop while it writes
    /// (ADR 0044).
    pub idle: bool,
    /// What a steer asks the session to do before it stops ([`Role::wrap_up`]).
    pub wrap_up: &'static str,
}

/// A context stop held back for the tool calls of the reply that crossed
/// it, which may carry the session's work (a whole file to write).
#[derive(Debug)]
struct HeldStop {
    reason: String,
    /// The reply's tool calls ran and all ended.
    calls_ended: bool,
}

impl HeldStop {
    /// Whether `event` starts the next reply, where the stop applies. Pi
    /// starts a turn after the tool calls; an ACP agent may start it
    /// before them or between them (each call starts when it runs), so
    /// there the stop waits for the reply's first token or its end. A
    /// session that ends instead ends on the stop.
    fn applies_at(&self, event: &SessionEvent) -> bool {
        match event {
            SessionEvent::TurnStart => self.calls_ended,
            SessionEvent::FirstToken | SessionEvent::Settled { .. } => true,
            _ => false,
        }
    }
}

/// What the session runner must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Steer {
        key: String,
        message: String,
    },
    Abort {
        reason: String,
    },
    /// Stop the running turn and, once it settles, prompt `message` in the
    /// same session.
    Interrupt {
        message: String,
    },
}

/// Rule state plus the metrics of one session.
#[derive(Debug)]
pub struct Watchdog {
    params: RunParams,
    watch: Option<WatchConfig>,
    metrics: SessionMetrics,
    steered: HashSet<String>,
    last_key: Option<String>,
    same_count: u32,
    idle_turns: i64,
    /// The turns that had ended when the session last wrote a file of its
    /// work, for the turn stop (ADR 0044).
    wrote_at: Option<u32>,
    /// Open write calls on the session's work: they count once they end
    /// without an error.
    open_writes: HashSet<String>,
    recent: VecDeque<String>,
    call_args: HashMap<String, (String, serde_json::Value)>,
    /// When each open tool call started (ms into the session).
    call_started: HashMap<String, u64>,
    tool_timeouts: u32,
    /// A tool call was stopped; the aborted turn is not the session's end.
    interrupting: bool,
    /// The context stop, held back while the tool calls of the reply that
    /// crossed it run.
    held_stop: Option<HeldStop>,
    /// What the session has read so far: reading something new is progress.
    novelty: Novelty,
    fingerprint: Option<String>,
    /// Where writes belong; `None` checks none (a sandboxed run).
    write_scope: Option<WriteScope>,
}

impl Watchdog {
    /// `watch: None` observes without ever steering or aborting (preflight).
    pub fn new(params: RunParams, watch: Option<WatchConfig>) -> Self {
        Self {
            params,
            watch,
            metrics: SessionMetrics::default(),
            steered: HashSet::new(),
            last_key: None,
            same_count: 0,
            idle_turns: 0,
            wrote_at: None,
            open_writes: HashSet::new(),
            recent: VecDeque::new(),
            call_args: HashMap::new(),
            call_started: HashMap::new(),
            tool_timeouts: 0,
            interrupting: false,
            held_stop: None,
            novelty: Novelty::default(),
            fingerprint: None,
            write_scope: None,
        }
    }

    pub fn metrics(&self) -> &SessionMetrics {
        &self.metrics
    }

    pub fn into_metrics(self) -> SessionMetrics {
        self.metrics
    }

    pub fn aborting(&self) -> bool {
        self.metrics.abort_reason.is_some()
    }

    /// The idle rule compares the working tree between turns; the runner
    /// provides the first fingerprint before the session starts.
    pub fn set_fingerprint(&mut self, fingerprint: Option<String>) {
        self.fingerprint = fingerprint;
    }

    /// Checks the files the write tools name against `scope`, and reads
    /// against its root: a path counts the same absolute or relative.
    pub fn set_write_scope(&mut self, scope: Option<WriteScope>) {
        self.write_scope = scope;
    }

    /// Records something the runner did outside the rules (the wall-clock
    /// limit, an abort it decided itself).
    pub fn abort_external(&mut self, reason: &str, elapsed_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        self.abort(reason, elapsed_ms, &mut actions);
        actions
    }

    /// When the oldest open tool call passes `toolMinutes`, if one is open.
    pub fn tool_deadline(&self) -> Option<u64> {
        let limit = self.params.tool_minutes.saturating_mul(60_000);
        if limit == 0 {
            return None;
        }
        self.call_started.values().min().map(|s| s + limit)
    }

    /// The oldest open tool call passed `toolMinutes`. The first time, stop
    /// the turn and tell the session which call and what to do; the second,
    /// end the session.
    pub fn tool_timeout(&mut self, elapsed_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        let minutes = self.params.tool_minutes;
        self.tool_timeouts += 1;
        if self.tool_timeouts >= 2 {
            self.abort(
                &format!("a tool call ran over {minutes} minutes twice"),
                elapsed_ms,
                &mut actions,
            );
            return actions;
        }
        let call = self.oldest_call();
        self.event("tool-timeout", Some(&call), elapsed_ms);
        self.interrupting = true;
        // A call the harness does not stop starts its time again, so it
        // ends the session at the next deadline.
        for start in self.call_started.values_mut() {
            *start = elapsed_ms;
        }
        actions.push(Action::Interrupt {
            message: format!(
                "[orchestrator] Your call `{call}` ran for more than {minutes} minutes and was stopped. It was probably waiting for input, serving, or looping. Find out why by running it again with your shell tool's own time limit. Do not use the `timeout` command; macOS does not have it. Fix the cause, and go on with your task. Give any command that may run long a time limit: a call that runs over {minutes} minutes again ends this session."
            ),
        });
        actions
    }

    /// The open tool call that started first: its command, path or
    /// arguments.
    fn oldest_call(&self) -> String {
        let Some(id) = self
            .call_started
            .iter()
            .min_by_key(|(id, start)| (**start, id.as_str()))
            .map(|(id, _)| id)
        else {
            return String::new();
        };
        let Some((name, args)) = self.call_args.get(id) else {
            return String::new();
        };
        let shown = command_arg(args)
            .or_else(|| path_arg(args))
            .map_or_else(|| format!("{name} {args}"), str::to_owned);
        match shown.char_indices().nth(200) {
            Some((cut, _)) => format!("{}…", &shown[..cut]),
            None => shown,
        }
    }

    /// A new prompt was sent: the session is not settled until it is
    /// answered, so a harness that dies before then ended early. How the
    /// last answer stopped says nothing about the new one.
    pub fn prompted(&mut self) {
        self.metrics.settled = false;
        self.metrics.last_stop_reason = None;
    }

    /// Records something the runner sent the session outside the rules.
    pub fn note(&mut self, kind: &str, elapsed_ms: u64) {
        self.event(kind, None, elapsed_ms);
    }

    /// Sets the final process facts once the harness has exited.
    pub fn finish(&mut self, exit_code: Option<i32>, duration_ms: u64) {
        self.metrics.exit_code = exit_code;
        self.metrics.duration_ms = duration_ms;
        self.metrics.tool_in_flight = self.call_in_flight().map(|(name, args)| {
            let call = format!("{name} {args}");
            match call.char_indices().nth(IN_FLIGHT_CHARS) {
                Some((cut, _)) => format!("{}…", &call[..cut]),
                None => call,
            }
        });
    }

    /// The whole shell command of the call [`Self::finish`] records as in
    /// flight, which the journal keeps cut; `None` when that call runs no
    /// command.
    pub fn command_in_flight(&self) -> Option<&str> {
        self.call_in_flight()
            .and_then(|(_, args)| command_arg(args))
    }

    /// The first call started and not ended, by ID: its name and arguments.
    fn call_in_flight(&self) -> Option<&(String, serde_json::Value)> {
        self.call_args
            .iter()
            .min_by_key(|(id, _)| id.as_str())
            .map(|(_, call)| call)
    }

    /// Feeds one event. `fingerprint` is evaluated only when the idle rule
    /// needs the working tree at the end of a turn.
    pub fn handle(
        &mut self,
        event: &SessionEvent,
        elapsed_ms: u64,
        fingerprint: impl FnOnce() -> String,
    ) -> Vec<Action> {
        let mut actions = self.release_held_stop(event, elapsed_ms);
        self.metrics.trail.observe(event);
        match event {
            SessionEvent::PromptRejected { reason } => self.abort(
                &format!("prompt rejected: {reason}"),
                elapsed_ms,
                &mut actions,
            ),
            call @ SessionEvent::ToolCallStart { .. } => {
                self.tool_call(call, elapsed_ms, &mut actions);
            }
            end @ SessionEvent::ToolCallEnd { .. } => {
                self.tool_ended(end, elapsed_ms, &mut actions);
            }
            SessionEvent::MessageEnd {
                usage,
                stop_reason,
                error_message,
                thinking_chars,
                text,
                ..
            } => {
                self.message_end(
                    usage,
                    stop_reason,
                    error_message.as_deref(),
                    *thinking_chars,
                    text,
                    elapsed_ms,
                    &mut actions,
                );
            }
            SessionEvent::TurnEnd => self.turn_end(elapsed_ms, fingerprint, &mut actions),
            SessionEvent::LateText { text } => self.push_text(text),
            SessionEvent::CompactionStart { reason } => {
                self.compaction_started(reason, elapsed_ms, &mut actions);
            }
            SessionEvent::CompactionRefused => self.compaction_refused(elapsed_ms, &mut actions),
            SessionEvent::OutputLimitLowered { asked, fits } => {
                self.output_limit_lowered(*asked, *fits, elapsed_ms);
            }
            SessionEvent::Settled { aborted } => self.settled(*aborted),
            SessionEvent::TurnStart => self.metrics.turn_started = true,
            SessionEvent::FirstToken
            | SessionEvent::PermissionRequest { .. }
            | SessionEvent::Exited { .. } => {}
        }
        actions
    }

    /// The harness finished every queued prompt, or was `aborted`.
    fn settled(&mut self, aborted: bool) {
        self.call_started.clear();
        let interrupted = std::mem::take(&mut self.interrupting);
        let aborting = self.aborting();
        let tool_timeout = aborted && interrupted && !aborting;
        if tool_timeout {
            // The turn a tool timeout stopped; the session goes on.
        } else if aborted || aborting {
            self.metrics.aborted = true;
        } else {
            self.metrics.settled = true;
        }
    }

    /// A held context stop, applied once the next reply starts.
    fn release_held_stop(&mut self, event: &SessionEvent, elapsed_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.held_stop.as_ref().is_some_and(|h| h.applies_at(event))
            && let Some(held) = self.held_stop.take()
        {
            self.abort(&held.reason, elapsed_ms, &mut actions);
        }
        actions
    }

    /// A tool call started: a held stop waits for it too.
    fn call_began(&mut self, id: &str, elapsed_ms: u64) {
        self.call_started.insert(id.to_owned(), elapsed_ms);
        if let Some(held) = self.held_stop.as_mut() {
            held.calls_ended = false;
        }
    }

    /// A tool call ended: a held stop waits for no more calls.
    fn call_ended(&mut self, id: &str) {
        self.call_started.remove(id);
        if let Some(held) = self.held_stop.as_mut() {
            held.calls_ended = self.call_started.is_empty();
        }
    }

    /// The harness compacted its context: a watched session is steered to
    /// wrap up at the first and stopped at the second.
    fn compaction_started(&mut self, reason: &str, elapsed_ms: u64, actions: &mut Vec<Action>) {
        self.metrics.compactions += 1;
        self.event("compaction", Some(reason), elapsed_ms);
        let Some(watch) = self.watch else { return };
        if self.metrics.compactions >= 2 {
            self.abort(SECOND_COMPACTION, elapsed_ms, actions);
        } else {
            let message = format!(
                "Context is being compacted, which loses detail. {}",
                watch.wrap_up
            );
            self.steer("compaction", &message, elapsed_ms, actions);
        }
    }

    /// The proxy refused the harness's request to compact: a watched
    /// session ends there, as one out of context (ADR 0022).
    fn compaction_refused(&mut self, elapsed_ms: u64, actions: &mut Vec<Action>) {
        self.metrics.compactions += 1;
        self.event("compaction", Some("refused by the proxy"), elapsed_ms);
        if self.watch.is_some() {
            self.abort(HARNESS_COMPACTION, elapsed_ms, actions);
        }
    }

    fn output_limit_lowered(&mut self, asked: u64, fits: u64, elapsed_ms: u64) {
        let detail = format!(
            "the engine had room for {fits} of the {asked} output tokens asked; \
             the proxy sent the request again with {fits}"
        );
        self.event(OUTPUT_LIMIT_LOWERED, Some(&detail), elapsed_ms);
    }

    fn event(&mut self, kind: &str, detail: Option<&str>, elapsed_ms: u64) {
        self.metrics.events.push(WatchdogEvent {
            kind: kind.to_owned(),
            detail: detail.filter(|d| !d.is_empty()).map(str::to_owned),
            turn: self.metrics.turns,
            ms: elapsed_ms,
        });
    }

    fn steer(&mut self, key: &str, message: &str, elapsed_ms: u64, actions: &mut Vec<Action>) {
        if self.steered.contains(key) || self.metrics.abort_reason.is_some() {
            return;
        }
        self.steered.insert(key.to_owned());
        self.event(&format!("steer:{key}"), None, elapsed_ms);
        actions.push(Action::Steer {
            key: key.to_owned(),
            message: format!("[orchestrator] {message}"),
        });
    }

    fn abort(&mut self, reason: &str, elapsed_ms: u64, actions: &mut Vec<Action>) {
        if self.metrics.abort_reason.is_some() {
            return;
        }
        self.metrics.abort_reason = Some(reason.to_owned());
        self.event("abort", Some(reason), elapsed_ms);
        actions.push(Action::Abort {
            reason: reason.to_owned(),
        });
    }

    /// A tool call starting: the loop rules, then its effect on files. A
    /// write, or a read of something new ([`Novelty`]), is progress for the
    /// idle rule; a write is also counted and its path checked.
    fn tool_call(&mut self, call: &SessionEvent, elapsed_ms: u64, actions: &mut Vec<Action>) {
        let SessionEvent::ToolCallStart {
            id,
            name,
            args,
            effect,
        } = call
        else {
            return;
        };
        self.tool_start(id, name, args, elapsed_ms, actions);
        let scope = self.write_scope.as_ref();
        let progress = match tool_effect(name, *effect) {
            Some(ToolEffect::Write) => {
                self.metrics.writes += 1;
                if self.writes_work(args) {
                    self.open_writes.insert(id.clone());
                }
                self.check_write_target(args, elapsed_ms, actions);
                true
            }
            Some(ToolEffect::Read) => self.novelty.read(args, scope),
            None => self.novelty.other(args, scope),
        };
        if progress {
            self.idle_turns = -1;
        }
    }

    fn tool_start(
        &mut self,
        id: &str,
        name: &str,
        args: &serde_json::Value,
        elapsed_ms: u64,
        actions: &mut Vec<Action>,
    ) {
        self.metrics.tool_calls += 1;
        self.call_args
            .insert(id.to_owned(), (name.to_owned(), args.clone()));
        self.call_began(id, elapsed_ms);
        let key = format!("{name}:{args}");
        self.same_count = if self.last_key.as_deref() == Some(&key) {
            self.same_count + 1
        } else {
            1
        };
        self.last_key = Some(key.clone());
        if self.watch.is_some() {
            if self.same_count >= self.params.loop_abort {
                self.abort(
                    &format!("same {name} call {} times in a row", self.same_count),
                    elapsed_ms,
                    actions,
                );
            } else if self.same_count >= self.params.loop_steer {
                let message = format!(
                    "You have made the same {name} call {} times in a row with the same arguments. Repeating it will not change the result. Read the relevant code or output, change your approach, or finish your handoff files and stop.",
                    self.same_count
                );
                self.steer(
                    &format!("loop-{}", &sha256_hex(&key)[..8]),
                    &message,
                    elapsed_ms,
                    actions,
                );
            }
        }
    }

    /// A write tool named a file outside the project: noted once per
    /// file, and the session told once where its files belong. The write
    /// itself is not stopped (ADR 0008).
    fn check_write_target(
        &mut self,
        args: &serde_json::Value,
        elapsed_ms: u64,
        actions: &mut Vec<Action>,
    ) {
        let Some(scope) = &self.write_scope else {
            return;
        };
        let Some(path) = scope.outside(args) else {
            return;
        };
        if self.metrics.outside_writes.contains(&path) {
            return;
        }
        let root = scope.root().display().to_string();
        self.event("outside-write", Some(&path), elapsed_ms);
        self.metrics.outside_writes.push(path.clone());
        if self.watch.is_some() {
            let message = format!(
                "You wrote `{path}`, which is outside the project, where the orchestrator does not look for it. Every file of your task belongs in the project, at {root}: name files by paths relative to it, such as `.tenaz/spec.md`, never with `../`, `~` or another absolute path. Write that file again there."
            );
            self.steer("outside-write", &message, elapsed_ms, actions);
        }
    }

    /// A tool call ended: counted if it failed, a write of the session's
    /// work if it was one and did not, then the output and repeat rules.
    fn tool_ended(&mut self, end: &SessionEvent, elapsed_ms: u64, actions: &mut Vec<Action>) {
        let SessionEvent::ToolCallEnd {
            id,
            name,
            result_text,
            is_error,
        } = end
        else {
            return;
        };
        if *is_error {
            self.count_tool_error(name, result_text);
        }
        if self.open_writes.remove(id) && !*is_error {
            self.wrote_at = Some(self.metrics.turns);
        }
        self.tool_end(id, name, result_text, elapsed_ms, actions);
    }

    fn tool_end(
        &mut self,
        id: &str,
        name: &str,
        result_text: &str,
        elapsed_ms: u64,
        actions: &mut Vec<Action>,
    ) {
        self.call_ended(id);
        let args = self
            .call_args
            .remove(id)
            .map_or(serde_json::Value::Null, |(_, args)| args);
        let key = format!("{name}:{args}:{}", sha256_hex(result_text));
        let output_tokens = approx_tokens(result_text);
        if let Some(watch) = self.watch
            && output_tokens > self.params.big_output_tokens
        {
            self.metrics.big_outputs += 1;
            if self.metrics.big_outputs <= 3 {
                let message = format!(
                    "That {name} result was about {output_tokens} tokens, and this session's context budget is {}. Do not read large files whole: grep for the part you need and read that range (offset/limit), and pipe command output through head, tail or grep.",
                    watch.context_tokens
                );
                self.steer(
                    &format!("big-output-{}", self.metrics.big_outputs),
                    &message,
                    elapsed_ms,
                    actions,
                );
            }
        }
        if self.recent.len() >= self.params.repeat_window {
            self.recent.pop_front();
        }
        self.recent.push_back(key.clone());
        let count =
            u32::try_from(self.recent.iter().filter(|k| **k == key).count()).unwrap_or(u32::MAX);
        if self.watch.is_some() {
            if count >= self.params.repeat_abort {
                self.abort(
                    &format!(
                        "same {name} call with the same result {count} times in the last {} calls",
                        self.recent.len()
                    ),
                    elapsed_ms,
                    actions,
                );
            } else if count >= self.params.repeat_steer {
                let message = format!(
                    "You have made the same {name} call and got the same result {count} times in your last {} tool calls. You are going in circles. Change your approach, or, if the task is done, stop now.",
                    self.recent.len()
                );
                self.steer(
                    &format!("repeat-{}", &sha256_hex(&key)[..8]),
                    &message,
                    elapsed_ms,
                    actions,
                );
            }
        }
    }

    /// Counts a failed tool call, keeping the first one's start.
    fn count_tool_error(&mut self, name: &str, result_text: &str) {
        self.metrics.tool_errors += 1;
        if self.metrics.first_tool_error.is_none() {
            let shown: String = result_text.chars().take(200).collect();
            self.metrics.first_tool_error = Some(format!("{name}: {shown}"));
        }
    }

    /// Keeps what the agent said, when it said anything.
    fn push_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.metrics.texts.push(text.to_owned());
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a separate field of the harness's message-end event"
    )]
    fn message_end(
        &mut self,
        usage: &crate::session::Usage,
        stop_reason: &StopReason,
        error_message: Option<&str>,
        thinking_chars: usize,
        text: &str,
        elapsed_ms: u64,
        actions: &mut Vec<Action>,
    ) {
        // A request cancelled because the runner stopped the turn is part of
        // that stop, not an engine failure. Pi reports it as an error ("This
        // operation was aborted").
        let stop_reason = if *stop_reason == StopReason::Error
            && (self.metrics.abort_reason.is_some() || self.interrupting)
        {
            &StopReason::Aborted
        } else {
            stop_reason
        };
        self.metrics.usage.add(usage);
        self.metrics.reasoning_chars += thinking_chars;
        self.metrics.context_peak = self.metrics.context_peak.max(usage.context());
        self.metrics.last_stop_reason = Some(stop_reason.clone());
        self.push_text(text);
        self.metrics.assistant.push(crate::session::AssistantTurn {
            stop_reason: stop_reason.clone(),
            usage: *usage,
            thinking: thinking_chars,
        });
        if *stop_reason == StopReason::Error {
            self.provider_error(error_message.unwrap_or(""), elapsed_ms, actions);
        } else {
            // A reply after the error: it no longer says how the session ends.
            self.metrics.engine_out_of_memory = false;
            self.metrics.request_refusal = None;
        }
        self.context_rules(stop_reason, elapsed_ms, actions);
    }

    /// A request the engine failed: what its `message` says (a capacity
    /// limit, a refused image, memory run out), and the abort after
    /// `providerErrorAbort` of them.
    fn provider_error(&mut self, message: &str, elapsed_ms: u64, actions: &mut Vec<Action>) {
        self.metrics.provider_errors += 1;
        self.event(PROVIDER_ERROR, Some(message), elapsed_ms);
        if let Some(limit) = capacity_limit(message) {
            self.metrics.capacity_limits.push(limit);
        }
        if is_image_refusal(message) {
            self.metrics.image_refusal = Some(message.to_owned());
        }
        // The last error says how the session ends, as for memory.
        self.metrics.request_refusal =
            (!is_image_refusal(message) && is_request_refusal(message)).then(|| message.to_owned());
        self.metrics.engine_out_of_memory = is_out_of_memory(message);
        if self.watch.is_some() && self.metrics.provider_errors >= self.params.provider_error_abort
        {
            self.abort(
                &format!("{} provider errors", self.metrics.provider_errors),
                elapsed_ms,
                actions,
            );
        }
    }

    /// The context stop and the steers before it, after each reply.
    fn context_rules(
        &mut self,
        stop_reason: &StopReason,
        elapsed_ms: u64,
        actions: &mut Vec<Action>,
    ) {
        let Some(watch) = self.watch else {
            return;
        };
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a context window times a fraction is far below 2^53 and truncates to a positive u64"
        )]
        let hard = (watch.context_tokens as f64 * self.params.context_abort) as u64;
        if self.metrics.context_peak > hard {
            let reason = format!(
                "context budget exceeded ({}/{} tokens)",
                self.metrics.context_peak, watch.context_tokens
            );
            // The reply that crossed the stop may carry the session's
            // work in its tool calls, such as a whole file to write;
            // stopping now would cancel them. They run first, and the
            // session stops before its next reply.
            if *stop_reason == StopReason::ToolUse && self.held_stop.is_none() {
                self.held_stop = Some(HeldStop {
                    reason,
                    calls_ended: false,
                });
            } else {
                self.held_stop = None;
                self.abort(&reason, elapsed_ms, actions);
            }
        } else if self.metrics.context_peak > watch.context_tokens {
            let message = format!(
                "This session has used {} tokens of context, over its budget of {}. {}",
                self.metrics.context_peak, watch.context_tokens, watch.wrap_up
            );
            self.steer("context", &message, elapsed_ms, actions);
        } else if let Some(turns) = self.turns_to_stop(hard)
            && turns <= u64::from(self.params.context_steer_turns)
        {
            let message = format!(
                "This session has used {} tokens of context. At its pace so far it reaches the stop at {hard} tokens in about {turns} turns, and a stopped session's work is lost. {}",
                self.metrics.context_peak, watch.wrap_up
            );
            self.steer("context-pace", &message, elapsed_ms, actions);
        }
    }

    /// Turns until the context reaches `stop` at the pace of the last few
    /// requests; `None` until three requests have reported their context.
    fn turns_to_stop(&self, stop: u64) -> Option<u64> {
        let contexts: Vec<u64> = self
            .metrics
            .assistant
            .iter()
            .map(|turn| turn.usage.context())
            .filter(|&context| context > 0)
            .collect();
        let window = &contexts[contexts.len().saturating_sub(PACE_WINDOW)..];
        let (&first, &last) = (window.first()?, window.last()?);
        if window.len() < 3 || last <= first {
            return None;
        }
        let pace = (last - first) / (window.len() as u64 - 1);
        (pace > 0).then(|| {
            stop.saturating_sub(self.metrics.context_peak)
                .div_ceil(pace)
        })
    }

    fn turn_end(
        &mut self,
        elapsed_ms: u64,
        fingerprint: impl FnOnce() -> String,
        actions: &mut Vec<Action>,
    ) {
        self.metrics.turns += 1;
        let Some(watch) = self.watch else { return };
        if watch.idle {
            let now = fingerprint();
            self.idle_turns = if self.fingerprint.as_deref() != Some(&now) || self.idle_turns < 0 {
                0
            } else {
                self.idle_turns + 1
            };
            self.fingerprint = Some(now);
            let idle = u32::try_from(self.idle_turns).unwrap_or(0);
            if idle >= self.params.idle_abort {
                self.abort(
                    &format!("{idle} turns without changing or reading anything new"),
                    elapsed_ms,
                    actions,
                );
            } else if idle >= self.params.idle_steer {
                let message = format!(
                    "You have gone {idle} turns without changing any file or reading anything new. Write down what you already know in your task's notes file, `.tenaz/notes/<task id>.md`: the files and facts the change depends on, and your plan. Then make the change, or explain in the notes what blocks you and stop."
                );
                self.steer("idle", &message, elapsed_ms, actions);
            }
        }
        self.turn_rule(watch, elapsed_ms, actions);
    }

    /// Steers a session at its turn budget and stops it at its turn stop,
    /// unless it still writes files: then it runs on until a window of
    /// turns passes without a write, or to the ceiling (ADR 0044).
    fn turn_rule(&mut self, watch: WatchConfig, elapsed_ms: u64, actions: &mut Vec<Action>) {
        let turns = self.metrics.turns;
        let stop = self.params.turn_stop(watch.turns);
        if turns < stop {
            if turns >= watch.turns {
                let message = format!(
                    "You have used your turn budget ({}). {}",
                    watch.turns, watch.wrap_up
                );
                self.steer("turns", &message, elapsed_ms, actions);
            }
            return;
        }
        if let Some(ceiling) = self.runs_past_stop(watch) {
            let window = count_turns(self.params.turn_write_window);
            let message = format!(
                "You have reached {stop} turns, where a session is stopped. You are still writing files, so this session goes on, but it is stopped once {window} pass without a file written, and at {ceiling} turns at the latest. Finish the change now and run ./verify, then write your handoff notes and stop."
            );
            self.steer("turns-past-stop", &message, elapsed_ms, actions);
            return;
        }
        self.abort(
            &format!("turn budget exceeded ({turns}/{})", watch.turns),
            elapsed_ms,
            actions,
        );
    }

    /// The ceiling a session past its turn stop runs on to, when it may:
    /// it edits code, the run lets sessions run on, and it wrote a file of
    /// its work in the last `turnWriteWindow` turns.
    fn runs_past_stop(&self, watch: WatchConfig) -> Option<u32> {
        let ceiling = self
            .params
            .turn_extension(watch.turns)
            .filter(|_| watch.idle)?;
        let recent = self
            .wrote_at
            .is_some_and(|at| self.metrics.turns - at <= self.params.turn_write_window);
        (recent && self.metrics.turns < ceiling).then_some(ceiling)
    }

    /// Whether a write call writes the task's work: a project file outside
    /// `.tenaz/`, where the handoff notes are. Without a scope (a sandboxed
    /// run) the project root is unknown, so only a relative path that
    /// stays below it counts. A call that names no path is not counted.
    fn writes_work(&self, args: &serde_json::Value) -> bool {
        let Some(path) = path_arg(args) else {
            return false;
        };
        let relative = match &self.write_scope {
            Some(scope) => scope.relative(path),
            None => below_root(path),
        };
        relative.is_some_and(|path| !path.starts_with(".tenaz"))
    }
}

/// `path` with `.` and `..` taken out, when it is relative, does not climb
/// above where it starts and names a file below it. A path from `~` is
/// the home directory, not a relative one.
fn below_root(path: &str) -> Option<std::path::PathBuf> {
    use std::path::Component;
    if path.starts_with('~') {
        return None;
    }
    let mut below = std::path::PathBuf::new();
    for component in std::path::Path::new(path).components() {
        match component {
            Component::Normal(part) => below.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !below.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!below.as_os_str().is_empty()).then_some(below)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::Role;
    use crate::session::Usage;

    fn watched() -> Watchdog {
        Watchdog::new(
            RunParams::default(),
            Some(WatchConfig {
                turns: 40,
                context_tokens: 60_000,
                idle: true,
                wrap_up: Role::Builder.wrap_up(),
            }),
        )
    }

    fn call(
        w: &mut Watchdog,
        n: u32,
        name: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> Vec<Action> {
        let id = format!("call_{n}");
        let mut actions = w.handle(
            &SessionEvent::ToolCallStart {
                id: id.clone(),
                name: name.into(),
                args: args.clone(),
                effect: None,
            },
            0,
            String::new,
        );
        actions.extend(w.handle(
            &SessionEvent::ToolCallEnd {
                id,
                name: name.into(),
                result_text: result.into(),
                is_error: false,
            },
            0,
            String::new,
        ));
        actions
    }

    fn is_steer(actions: &[Action], key: &str) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, Action::Steer { key: k, .. } if k == key))
    }

    /// The session's metrics keep its trail for a continuation.
    #[test]
    fn tool_calls_and_replies_reach_the_trail() {
        let mut w = watched();
        call(
            &mut w,
            1,
            "write",
            &serde_json::json!({"path": "src/a.js"}),
            "",
        );
        call(
            &mut w,
            2,
            "bash",
            &serde_json::json!({"command": "./verify"}),
            "ok",
        );
        w.handle(&crate::trail::testing::reply("Done.", None), 0, String::new);
        let trail = &w.metrics().trail;
        assert_eq!(trail.written(), ["src/a.js"]);
        assert_eq!(trail.last_words(), "Done.");
        assert_eq!(trail.check().map(|c| c.result.as_str()), Some("ok"));
    }

    #[test]
    fn same_call_in_a_row_is_steered_then_aborted() {
        let mut w = watched();
        let args = serde_json::json!({"command": "ls checks"});
        let mut all = Vec::new();
        for n in 1..=5 {
            all.push(call(&mut w, n, "bash", &args, &format!("out {n}")));
        }
        assert!(all[0].is_empty() && all[1].is_empty());
        assert!(
            all[2]
                .iter()
                .any(|a| matches!(a, Action::Steer { key, .. } if key.starts_with("loop-")))
        );
        assert!(all[3].is_empty(), "the same steer is not repeated");
        assert_eq!(
            all[4],
            vec![Action::Abort {
                reason: "same bash call 5 times in a row".into()
            }]
        );
        assert!(w.aborting());
        assert_eq!(w.metrics().tool_calls, 5);
        assert_eq!(
            w.metrics()
                .events
                .iter()
                .filter(|e| e.kind == "abort")
                .count(),
            1
        );
    }

    #[test]
    fn alternating_calls_with_the_same_results_are_caught_by_the_repeat_rule() {
        let mut w = watched();
        let mut n = 0;
        let mut seen_steer = false;
        let mut aborted = None;
        for _ in 0..6 {
            n += 1;
            let a = call(
                &mut w,
                n,
                "write",
                &serde_json::json!({"path": "notes.md"}),
                "ok",
            );
            n += 1;
            let b = call(
                &mut w,
                n,
                "bash",
                &serde_json::json!({"command": "wc -l notes.md"}),
                "3 notes.md",
            );
            for action in a.into_iter().chain(b) {
                match action {
                    Action::Steer { key, .. } if key.starts_with("repeat-") => seen_steer = true,
                    Action::Steer { key, .. } => panic!("unexpected steer {key}"),
                    Action::Abort { reason } => aborted = Some(reason),
                    Action::Interrupt { message } => panic!("unexpected interrupt {message}"),
                }
            }
        }
        assert!(seen_steer);
        assert_eq!(
            aborted.as_deref(),
            Some("same write call with the same result 6 times in the last 11 calls")
        );
    }

    #[test]
    fn big_outputs_are_steered_at_most_three_times() {
        let mut w = watched();
        let big = "x".repeat(40_000);
        let mut steers = 0;
        for n in 1..=5 {
            let actions = call(
                &mut w,
                n,
                "read",
                &serde_json::json!({"path": format!("f{n}")}),
                &big,
            );
            steers += actions
                .iter()
                .filter(
                    |a| matches!(a, Action::Steer { key, .. } if key.starts_with("big-output-")),
                )
                .count();
        }
        assert_eq!(steers, 3);
        assert_eq!(w.metrics().big_outputs, 5);
    }

    fn message(w: &mut Watchdog, input: u64, stop: StopReason, error: Option<&str>) -> Vec<Action> {
        w.handle(
            &SessionEvent::MessageEnd {
                server: None,
                sent: None,
                usage: Usage {
                    input,
                    output: 10,
                    ..Default::default()
                },
                stop_reason: stop,
                error_message: error.map(str::to_owned),
                thinking_chars: 7,
                text: "hi".into(),
            },
            5,
            String::new,
        )
    }

    #[test]
    fn context_budget_steers_then_stops_the_session() {
        let mut w = watched();
        assert!(message(&mut w, 30_000, StopReason::ToolUse, None).is_empty());
        let over = message(&mut w, 61_000, StopReason::ToolUse, None);
        assert!(is_steer(&over, "context"));
        assert!(message(&mut w, 70_000, StopReason::ToolUse, None).is_empty());
        let stop = message(&mut w, 80_000, StopReason::Stop, None);
        assert_eq!(
            stop,
            vec![Action::Abort {
                reason: "context budget exceeded (80010/60000 tokens)".into()
            }]
        );
        assert!(w.metrics().context_stopped());
        assert_eq!(w.metrics().reasoning_chars, 28);
        assert_eq!(w.metrics().texts.len(), 4);
    }

    #[test]
    fn late_text_is_kept_with_the_messages_text() {
        let mut w = watched();
        message(&mut w, 1_000, StopReason::Stop, None);
        let late = SessionEvent::LateText {
            text: "marmalade-4817".into(),
        };
        let empty = SessionEvent::LateText {
            text: String::new(),
        };
        assert!(w.handle(&late, 6, String::new).is_empty());
        assert!(w.handle(&empty, 7, String::new).is_empty());
        assert_eq!(w.metrics().texts, ["hi", "marmalade-4817"]);
    }

    /// The context stop for a reply of 80,000 input and the helper's 10
    /// output tokens.
    fn context_stop() -> Vec<Action> {
        vec![Action::Abort {
            reason: "context budget exceeded (80010/60000 tokens)".into(),
        }]
    }

    /// A watched session whose reply crossed the context stop with tool
    /// calls (a whole file to write, in the Digger 3D run of 2026-10-27).
    fn crossed_with_tool_calls() -> Watchdog {
        let mut w = watched();
        assert!(message(&mut w, 80_000, StopReason::ToolUse, None).is_empty());
        w
    }

    fn write_file(w: &mut Watchdog) -> Vec<Action> {
        call(
            w,
            9,
            "write",
            &serde_json::json!({ "path": "src/core/simulation.ts" }),
            "ok",
        )
    }

    /// Pi runs the reply's calls, then starts the next turn.
    #[test]
    fn the_context_stop_waits_for_the_reply_s_calls_in_pi_s_order() {
        let mut w = crossed_with_tool_calls();
        assert!(write_file(&mut w).is_empty(), "the write runs");
        assert!(w.handle(&SessionEvent::TurnEnd, 6, String::new).is_empty());
        assert_eq!(
            w.handle(&SessionEvent::TurnStart, 7, String::new),
            context_stop()
        );
    }

    /// fx starts the next turn, then runs the calls.
    #[test]
    fn the_context_stop_waits_for_the_reply_s_calls_in_fx_s_order() {
        let mut w = crossed_with_tool_calls();
        assert!(
            w.handle(&SessionEvent::TurnStart, 6, String::new)
                .is_empty()
        );
        assert!(write_file(&mut w).is_empty(), "the write runs");
        assert_eq!(
            w.handle(&SessionEvent::FirstToken, 7, String::new),
            context_stop()
        );
    }

    /// An ACP agent starts each call when it runs: a call that starts after
    /// another ended holds the stop again.
    #[test]
    fn the_context_stop_waits_for_a_call_that_starts_after_another_ended() {
        let mut w = crossed_with_tool_calls();
        assert!(write_file(&mut w).is_empty(), "the first write runs");
        let second = SessionEvent::ToolCallStart {
            id: "call_10".into(),
            name: "write".into(),
            args: serde_json::json!({ "path": "src/core/render.ts" }),
            effect: None,
        };
        assert!(w.handle(&second, 6, String::new).is_empty());
        assert!(
            w.handle(&SessionEvent::TurnStart, 7, String::new)
                .is_empty(),
            "the second write is still running"
        );
        let ended = SessionEvent::ToolCallEnd {
            id: "call_10".into(),
            name: "write".into(),
            result_text: "ok".into(),
            is_error: false,
        };
        assert!(w.handle(&ended, 8, String::new).is_empty());
        assert_eq!(
            w.handle(&SessionEvent::TurnStart, 9, String::new),
            context_stop(),
            "with every call ended, the next turn applies the stop"
        );
    }

    #[test]
    fn a_session_that_ends_after_crossing_the_stop_ends_on_it() {
        let mut w = crossed_with_tool_calls();
        let ended = w.handle(&SessionEvent::Settled { aborted: false }, 7, String::new);
        assert_eq!(ended, context_stop());
        assert!(w.metrics().aborted && w.metrics().context_stopped());
    }

    /// The stop is held once: a second reply over it stops at once.
    #[test]
    fn a_second_reply_over_the_stop_is_not_held() {
        let mut w = crossed_with_tool_calls();
        assert_eq!(
            message(&mut w, 80_000, StopReason::ToolUse, None),
            context_stop()
        );
    }

    #[test]
    fn a_fast_growing_context_is_steered_before_the_budget() {
        let mut w = watched();
        assert!(message(&mut w, 40_000, StopReason::ToolUse, None).is_empty());
        assert!(message(&mut w, 44_000, StopReason::ToolUse, None).is_empty());
        let pace = message(&mut w, 48_000, StopReason::ToolUse, None);
        assert!(is_steer(&pace, "context-pace"), "{pace:?}");
        let Action::Steer { message: text, .. } = &pace[0] else {
            unreachable!()
        };
        assert!(text.contains("about 8 turns"), "{text}");
        assert!(message(&mut w, 52_000, StopReason::ToolUse, None).is_empty());
        assert!(is_steer(
            &message(&mut w, 61_000, StopReason::ToolUse, None),
            "context"
        ));
    }

    #[test]
    fn a_slow_context_is_steered_only_at_the_budget() {
        let mut w = watched();
        for input in [40_000, 41_000, 42_000, 43_000] {
            assert!(message(&mut w, input, StopReason::ToolUse, None).is_empty());
        }
        let mut params = RunParams::default();
        params.context_steer_turns = 0;
        let mut off = Watchdog::new(params, w.watch);
        for input in [40_000, 44_000, 48_000, 52_000] {
            assert!(message(&mut off, input, StopReason::ToolUse, None).is_empty());
        }
    }

    /// Sushi's refusal of an output limit, as recorded: the prompt's room
    /// in memory, and memory run out.
    #[test]
    fn an_output_limit_refusal_records_the_prompt_s_room() {
        let mut w = watched();
        message(
            &mut w,
            0,
            StopReason::Error,
            Some(crate::testing::SUSHI_OUTPUT_LIMIT_REFUSAL),
        );
        let m = w.metrics();
        assert_eq!(m.capacity_limits, vec![83_456]);
        assert!(m.engine_out_of_memory);
    }

    #[test]
    fn provider_errors_record_capacity_and_images_then_abort() {
        let mut w = watched();
        let capacity =
            "400: Prompt too large. Maximum context at KV8 and prefill chunk 512: 61488 tokens.";
        assert!(message(&mut w, 0, StopReason::Error, Some(capacity)).is_empty());
        assert!(
            message(
                &mut w,
                0,
                StopReason::Error,
                Some("400: This request has at most 2 images allowed")
            )
            .is_empty()
        );
        let third = message(&mut w, 0, StopReason::Error, Some("boom"));
        assert_eq!(
            third,
            vec![Action::Abort {
                reason: "3 provider errors".into()
            }]
        );
        let m = w.metrics();
        assert_eq!(m.capacity_limits, vec![61488]);
        assert!(
            m.image_refusal
                .as_deref()
                .unwrap()
                .contains("at most 2 images")
        );
        assert!(m.engine_failed());
        assert_eq!(m.last_stop_reason, Some(StopReason::Error));
        assert!(!m.engine_out_of_memory);
    }

    /// A request refused for what was in it is recorded as such, apart
    /// from an image refusal.
    #[test]
    fn a_refused_request_is_recorded() {
        let mut w = watched();
        let refusal = "400: messages[26]: video could not be decoded";
        message(&mut w, 0, StopReason::Error, Some(refusal));
        let m = w.metrics();
        assert_eq!(m.request_refusal.as_deref(), Some(refusal));
        assert!(m.image_refusal.is_none());
        assert!(m.engine_failed());
        // A reply after it: the harness got past the refused request.
        message(&mut w, 0, StopReason::Stop, None);
        assert!(w.metrics().request_refusal.is_none());
        // Another error after it says how the session ends.
        message(&mut w, 0, StopReason::Error, Some(refusal));
        message(&mut w, 0, StopReason::Error, Some("503: busy"));
        assert!(w.metrics().request_refusal.is_none());
    }

    /// Sushi's error when Metal cannot get the memory, often because a
    /// render shares the GPU: an engine failure, marked as such.
    #[test]
    fn an_engine_out_of_memory_is_marked() {
        let mut w = watched();
        message(
            &mut w,
            0,
            StopReason::Error,
            Some("error.GenerationOutOfMemory"),
        );
        let m = w.metrics();
        assert!(m.engine_out_of_memory);
        assert!(m.engine_failed());
        assert!(m.capacity_limits.is_empty(), "not a prompt too large");
    }

    /// A harness that retries got past the engine's memory error: a later
    /// reply, or another error, no longer says the session ran out.
    #[test]
    fn only_the_last_engine_error_says_whether_memory_ran_out() {
        let mut w = watched();
        message(
            &mut w,
            0,
            StopReason::Error,
            Some("error.GenerationOutOfMemory"),
        );
        message(&mut w, 0, StopReason::ToolUse, None);
        assert!(!w.metrics().engine_out_of_memory);
        message(
            &mut w,
            0,
            StopReason::Error,
            Some("error.GenerationOutOfMemory"),
        );
        message(&mut w, 0, StopReason::Error, Some("boom"));
        assert!(!w.metrics().engine_out_of_memory);
    }

    #[test]
    fn a_tool_call_over_its_time_is_interrupted_then_ends_the_session() {
        let mut w = watched();
        assert_eq!(w.tool_deadline(), None, "no call open");
        let start = |w: &mut Watchdog, id: &str, at: u64| {
            w.handle(
                &SessionEvent::ToolCallStart {
                    id: id.into(),
                    name: "bash".into(),
                    args: serde_json::json!({ "command": "./verify --all" }),
                    effect: None,
                },
                at,
                String::new,
            )
        };
        start(&mut w, "call_1", 1_000);
        assert_eq!(w.tool_deadline(), Some(601_000), "ten minutes by default");
        let actions = w.tool_timeout(601_000);
        let [Action::Interrupt { message }] = actions.as_slice() else {
            panic!("{actions:?}");
        };
        assert!(
            message.contains("`./verify --all` ran for more than 10 minutes"),
            "{message}"
        );
        assert!(
            message.contains("shell tool's own time limit")
                && message.contains("Do not use the `timeout` command")
                && !message.contains("timeout "),
            "no GNU `timeout` example, which macOS lacks: {message}"
        );
        assert_eq!(
            w.tool_deadline(),
            Some(1_201_000),
            "a call not stopped gets its time again"
        );
        // The stopped turn settles aborted; the session is not.
        w.handle(
            &SessionEvent::Settled { aborted: true },
            601_500,
            String::new,
        );
        assert!(!w.aborting() && !w.metrics().aborted);
        assert_eq!(w.tool_deadline(), None);
        start(&mut w, "call_2", 700_000);
        let actions = w.tool_timeout(1_300_000);
        assert!(
            matches!(actions.as_slice(), [Action::Abort { reason }] if reason == "a tool call ran over 10 minutes twice"),
            "{actions:?}"
        );
        let kinds: Vec<&str> = w.metrics().events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["tool-timeout", "abort"]);

        let mut off = Watchdog::new(
            RunParams {
                tool_minutes: 0,
                ..RunParams::default()
            },
            None,
        );
        start(&mut off, "call_1", 0);
        assert_eq!(off.tool_deadline(), None, "0 turns it off");
    }

    #[test]
    fn idle_turns_count_only_unchanged_worktrees() {
        let mut p = RunParams::default();
        p.idle_steer = 2;
        p.idle_abort = 3;
        let mut w = Watchdog::new(
            p,
            Some(WatchConfig {
                turns: 40,
                context_tokens: 60_000,
                idle: true,
                wrap_up: Role::Builder.wrap_up(),
            }),
        );
        w.set_fingerprint(Some("a".into()));
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "a".into())
                .is_empty()
        );
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty(),
            "a change resets the count"
        );
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty()
        );
        assert!(is_steer(
            &w.handle(&SessionEvent::TurnEnd, 0, || "b".into()),
            "idle"
        ));
        // A write tool call resets the count even before the next fingerprint.
        w.handle(&tool_call("edit", serde_json::Value::Null), 0, String::new);
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty()
        );
        assert_eq!(w.metrics().writes, 1);
        // So does a read of something not read before, but not a repeat.
        let read = |path: &str| tool_call("read", serde_json::json!({ "path": path }));
        w.handle(&read("src/lib.rs"), 0, String::new);
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty()
        );
        w.handle(&read("src/lib.rs"), 0, String::new);
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty()
        );
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty(),
            "the idle steer is sent once"
        );
        let abort = w.handle(&SessionEvent::TurnEnd, 0, || "b".into());
        assert_eq!(
            abort,
            vec![Action::Abort {
                reason: "3 turns without changing or reading anything new".into()
            }]
        );
    }

    /// A tool call that declares no effect.
    fn tool_call(name: &str, args: serde_json::Value) -> SessionEvent {
        SessionEvent::ToolCallStart {
            id: "c".into(),
            name: name.into(),
            args,
            effect: None,
        }
    }

    #[test]
    fn qwen_code_reads_of_new_files_are_not_idle() {
        // Session 043-builder-T007 (Qwen Code 0.26.6) read 20 files with
        // `ReadFile` and was aborted as idle.
        let mut p = RunParams::default();
        p.idle_steer = 2;
        p.idle_abort = 3;
        let mut w = Watchdog::new(
            p,
            Some(WatchConfig {
                turns: 40,
                context_tokens: 60_000,
                idle: true,
                wrap_up: Role::Builder.wrap_up(),
            }),
        );
        w.set_fingerprint(Some("a".into()));
        for n in 0..20 {
            let path = format!("src/{n}.rs");
            let read = tool_call("ReadFile", serde_json::json!({ "file_path": path }));
            w.handle(&read, 0, String::new);
            assert!(
                w.handle(&SessionEvent::TurnEnd, 0, || "a".into())
                    .is_empty(),
                "read {n}"
            );
        }
    }

    #[test]
    fn a_declared_write_counts_whatever_its_name() {
        // Qwen Code titles a call it cannot describe yet `ToolCall`.
        let mut w = Watchdog::new(RunParams::default(), None);
        let write = SessionEvent::ToolCallStart {
            id: "w".into(),
            name: "ToolCall".into(),
            args: serde_json::json!({ "file_path": "src/new.rs" }),
            effect: Some(ToolEffect::Write),
        };
        w.handle(&write, 0, String::new);
        assert_eq!(w.metrics().writes, 1);
    }

    #[test]
    fn a_declared_read_of_a_new_file_is_progress_whatever_its_name() {
        let mut p = RunParams::default();
        p.idle_steer = 1;
        p.idle_abort = 2;
        let config = WatchConfig {
            turns: 40,
            context_tokens: 60_000,
            idle: true,
            wrap_up: Role::Builder.wrap_up(),
        };
        let mut w = Watchdog::new(p, Some(config));
        w.set_fingerprint(Some("a".into()));
        let read = SessionEvent::ToolCallStart {
            id: "r".into(),
            name: "ToolCall".into(),
            args: serde_json::json!({ "file_path": "src/new.rs" }),
            effect: Some(ToolEffect::Read),
        };
        w.handle(&read, 0, String::new);
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "a".into())
                .is_empty()
        );
    }

    /// A builder held to the idle rule: steered after 2 idle turns,
    /// stopped after 3.
    fn idle_watched() -> Watchdog {
        let mut p = RunParams::default();
        p.idle_steer = 2;
        p.idle_abort = 3;
        let mut w = Watchdog::new(
            p,
            Some(WatchConfig {
                turns: 40,
                context_tokens: 60_000,
                idle: true,
                wrap_up: Role::Builder.wrap_up(),
            }),
        );
        w.set_fingerprint(Some("a".into()));
        w
    }

    /// One turn of one shell call, in an unchanged worktree.
    fn shell_turn(w: &mut Watchdog, command: &str) -> Vec<Action> {
        w.handle(
            &tool_call("bash", serde_json::json!({ "command": command })),
            0,
            String::new,
        );
        w.handle(&SessionEvent::TurnEnd, 0, || "a".into())
    }

    #[test]
    fn shell_reads_of_new_files_are_not_idle() {
        // Sessions 054 and 055-builder-T019 (dsh, 2026-11-08) read the
        // checks with `sed -n` and `grep` and were aborted as idle.
        let mut w = idle_watched();
        for n in 0..20 {
            let command = format!("sed -n 1,60p checks/T{n:03}.check.mjs | grep -n barrel");
            assert!(shell_turn(&mut w, &command).is_empty(), "read {n}");
        }
    }

    #[test]
    fn a_probe_script_is_progress_once() {
        let mut w = idle_watched();
        let write = "cat > .tenaz/private/probe.mjs <<'EOF'\nconsole.log(1)\nEOF";
        assert!(shell_turn(&mut w, write).is_empty(), "a new path");
        let run = "node .tenaz/private/probe.mjs 2>&1 | tail -60";
        assert!(shell_turn(&mut w, run).is_empty(), "idle 1: run again");
        let rewrite = "cat > .tenaz/private/probe.mjs <<'EOF'\nconsole.log(2)\nEOF";
        assert!(
            is_steer(&shell_turn(&mut w, rewrite), "idle"),
            "idle 2: rewritten"
        );
        assert!(
            shell_turn(&mut w, "node .tenaz/private/probe2.mjs").is_empty(),
            "a new probe resets the count"
        );
    }

    #[test]
    fn a_shell_read_of_a_file_already_read_is_idle() {
        // Qwen Code reads with absolute paths; the shell names them relative.
        let mut w = scoped(idle_watched());
        let read = tool_call(
            "ReadFile",
            serde_json::json!({ "file_path": "/work/app/src/world.js" }),
        );
        w.handle(&read, 0, String::new);
        w.handle(&SessionEvent::TurnEnd, 0, || "a".into());
        assert!(
            shell_turn(&mut w, "sed -n 1,40p src/world.js").is_empty(),
            "idle 1"
        );
        assert!(is_steer(
            &shell_turn(&mut w, "sed -n 40,80p src/world.js"),
            "idle"
        ));
    }

    #[test]
    fn shell_reads_of_the_same_file_are_steered_then_stopped() {
        let mut w = idle_watched();
        let turns: Vec<Vec<Action>> = (0..4)
            .map(|n| shell_turn(&mut w, &format!("sed -n {n},9p ./src/world.js")))
            .collect();
        assert!(
            turns[0].is_empty() && turns[1].is_empty(),
            "a new path, then idle 1"
        );
        assert!(is_steer(&turns[2], "idle"));
        assert!(
            matches!(&turns[3][..], [Action::Abort { reason }] if reason.starts_with("3 turns"))
        );
    }

    #[test]
    fn reading_back_a_written_file_counts_once() {
        // A write is not a read: the first look at what was written is new.
        let mut w = idle_watched();
        w.handle(
            &tool_call("write", serde_json::json!({ "path": "src/a.js" })),
            0,
            String::new,
        );
        w.handle(&SessionEvent::TurnEnd, 0, || "b".into());
        assert!(shell_turn(&mut w, "cat src/a.js").is_empty(), "new");
        assert!(shell_turn(&mut w, "cat src/a.js").is_empty(), "idle 1");
        assert!(is_steer(&shell_turn(&mut w, "cat src/a.js"), "idle"));
    }

    /// The turns at which a session with a budget of `budget` turns is
    /// steered by the turn rule and stopped by it, over `turns` turns.
    fn turn_rule(turn_abort: f64, budget: u32, turns: u32) -> (Vec<u32>, Vec<u32>) {
        let mut w = Watchdog::new(
            RunParams {
                turn_abort,
                ..RunParams::default()
            },
            Some(WatchConfig {
                turns: budget,
                context_tokens: 60_000,
                idle: false,
                wrap_up: Role::Builder.wrap_up(),
            }),
        );
        let (mut steered, mut stopped) = (Vec::new(), Vec::new());
        for turn in 1..=turns {
            for action in w.handle(&SessionEvent::TurnEnd, 0, || {
                panic!("idle off: fingerprint must not be computed")
            }) {
                match action {
                    Action::Steer { key, .. } if key == "turns" => steered.push(turn),
                    Action::Abort { reason } if reason.starts_with("turn budget exceeded") => {
                        stopped.push(turn);
                    }
                    Action::Steer { key, .. } => panic!("unexpected steer {key}"),
                    Action::Abort { reason } => panic!("unexpected stop {reason}"),
                    Action::Interrupt { message } => panic!("unexpected interrupt {message}"),
                }
            }
        }
        (steered, stopped)
    }

    #[test]
    fn the_default_turn_rule_steers_at_the_budget_and_stops_at_one_and_a_half() {
        let default = RunParams::default().turn_abort;
        assert_eq!(turn_rule(default, 4, 7), (vec![4], vec![6]));
    }

    #[test]
    fn turn_abort_sets_the_stop_as_a_multiple_of_the_turn_budget() {
        assert_eq!(turn_rule(2.0, 4, 9), (vec![4], vec![8]));
    }

    /// 4 turns times 1.1 is 4.4: the stop is the turn after, not the budget.
    #[test]
    fn a_fractional_turn_stop_is_rounded_up() {
        assert_eq!(turn_rule(1.1, 4, 6), (vec![4], vec![5]));
    }

    /// 50 times 1.1 is 55.000000000000007 in floating point.
    #[test]
    fn a_turn_stop_that_is_whole_is_not_rounded_up_by_float_error() {
        assert_eq!(turn_rule(1.1, 50, 57), (vec![50], vec![55]));
    }

    #[test]
    fn a_turn_abort_of_one_stops_at_the_budget_without_a_steer() {
        assert_eq!(turn_rule(1.0, 4, 5), (vec![], vec![4]));
    }

    #[test]
    fn a_turn_abort_below_one_stops_before_the_budget_without_a_steer() {
        assert_eq!(turn_rule(0.5, 4, 3), (vec![], vec![2]));
        assert_eq!(turn_rule(0.0, 4, 2), (vec![], vec![1]));
    }

    /// What one turn of a builder does, for the turn stop past the budget.
    #[derive(Clone, Copy)]
    enum Turn {
        /// Writes a project file with a write tool.
        Writes,
        /// Calls a write tool that fails (an edit whose old text is not
        /// there).
        FailsToWrite,
        /// Writes its handoff notes under `.tenaz/notes/`.
        WritesNotes,
        /// Changes the working tree with a shell command, no write tool.
        Shell,
        /// Reads a file it has not read before.
        Reads,
    }

    /// The parameters these tests are written for, named rather than taken
    /// from the defaults: stop at 1.5 times the budget, run on to 2 times
    /// it while writing, stopped after 5 turns without a write.
    fn run_on_params() -> RunParams {
        RunParams {
            turn_abort: 1.5,
            turn_ceiling: 2.0,
            turn_write_window: 5,
            ..RunParams::default()
        }
    }

    /// A builder with a budget of 40 turns; `idle` off is a role that edits
    /// no code.
    fn builder(params: RunParams, idle: bool) -> Watchdog {
        let mut w = Watchdog::new(
            params,
            Some(WatchConfig {
                turns: 40,
                context_tokens: 60_000,
                idle,
                wrap_up: Role::Builder.wrap_up(),
            }),
        );
        w.set_fingerprint(Some("0".into()));
        w
    }

    /// One call of `name` with `args` that ends with `is_error`.
    fn call_and_end(w: &mut Watchdog, name: &str, args: serde_json::Value, is_error: bool) {
        w.handle(&tool_call(name, args), 0, String::new);
        let end = SessionEvent::ToolCallEnd {
            id: "c".into(),
            name: name.into(),
            result_text: if is_error { "no match" } else { "ok" }.into(),
            is_error,
        };
        w.handle(&end, 0, String::new);
    }

    /// The calls of turn `n`, and the working tree after them.
    fn do_turn(w: &mut Watchdog, n: u32, turn: Turn, tree: &mut u32) {
        let file = |dir: &str| serde_json::json!({ "path": format!("{dir}/{n}.js"), "n": n });
        match turn {
            Turn::Writes => call_and_end(w, "write", file("src"), false),
            Turn::FailsToWrite => call_and_end(w, "edit", file("src"), true),
            Turn::WritesNotes => call_and_end(w, "write", file(".tenaz/notes"), false),
            Turn::Shell => *tree += 1,
            Turn::Reads => call_and_end(w, "read", file("lib"), false),
        }
    }

    /// Plays `turns`, one per turn: the turn the builder was stopped at, if
    /// it was, and the steers it got on the way, with their turn.
    fn play_builder(mut w: Watchdog, turns: &[Turn]) -> (Option<u32>, Vec<(u32, String)>) {
        let mut tree = 0;
        let mut steers = Vec::new();
        for (n, turn) in (1..).zip(turns) {
            do_turn(&mut w, n, *turn, &mut tree);
            for action in w.handle(&SessionEvent::TurnEnd, 0, || tree.to_string()) {
                if let Action::Abort { reason } = action {
                    assert_eq!(reason, format!("turn budget exceeded ({n}/40)"));
                    return (Some(n), steers);
                }
                steers.push((n, steer_key(action)));
            }
        }
        (None, steers)
    }

    fn steer_key(action: Action) -> String {
        match action {
            Action::Steer { key, .. } => key,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// `first` turns like a builder that writes about one turn in three
    /// (HB 013-builder-T005 wrote in 21 of its 60 turns), then `rest`.
    fn writing_then(first: usize, rest: &[Turn]) -> Vec<Turn> {
        let mut turns: Vec<Turn> = (0..first)
            .map(|n| {
                if n % 3 == 2 {
                    Turn::Writes
                } else {
                    Turn::Reads
                }
            })
            .collect();
        turns.extend_from_slice(rest);
        turns
    }

    /// When a builder that writes in `first` turns as [`writing_then`] and
    /// then only does `then` is stopped.
    fn stopped_after(first: usize, then: Turn) -> Option<u32> {
        let turns = writing_then(first, &[then; 40]);
        play_builder(builder(run_on_params(), true), &turns).0
    }

    fn steer_keys(steers: &[(u32, String)]) -> Vec<(u32, &str)> {
        steers.iter().map(|(n, key)| (*n, key.as_str())).collect()
    }

    #[test]
    fn a_builder_still_writing_at_the_turn_stop_runs_on_to_the_ceiling() {
        // Its last write before turn 80 is in turn 78: only the ceiling
        // stops it.
        let turns = writing_then(100, &[]);
        let (stopped, steers) = play_builder(builder(run_on_params(), true), &turns);
        assert_eq!(stopped, Some(80));
        assert_eq!(
            steer_keys(&steers),
            vec![(40, "turns"), (60, "turns-past-stop")]
        );
    }

    #[test]
    fn a_builder_that_stopped_writing_is_stopped_at_the_turn_stop_though_it_reads() {
        // HBi 061-builder-T102cb read on and wrote nothing in its last
        // turns; its next session was stopped at 60 too. Here the last
        // write is in turn 54, so turns 55 to 60 have none.
        let turns = writing_then(54, &[Turn::Reads; 40]);
        let (stopped, steers) = play_builder(builder(run_on_params(), true), &turns);
        assert_eq!(stopped, Some(60));
        assert_eq!(steer_keys(&steers), vec![(40, "turns")]);
    }

    /// The last write is in turn 66: turns 67 to 70 run on, and turn 71,
    /// the fifth without a write, stops it.
    #[test]
    fn a_builder_let_past_the_stop_is_stopped_once_it_stops_writing() {
        let mut turns = writing_then(65, &[Turn::Writes]);
        turns.extend_from_slice(&[Turn::Reads; 20]);
        let (stopped, _) = play_builder(builder(run_on_params(), true), &turns);
        assert_eq!(stopped, Some(71));
    }

    /// Only a write that succeeded on a project file outside `.tenaz/`
    /// counts: not a failed edit, not the handoff notes, not a file the
    /// shell changed (the runs measured write calls only, ADR 0044). The
    /// last write is in turn 57, so the window lapses after turn 62.
    #[test]
    fn failed_writes_notes_and_shell_changes_do_not_keep_a_builder_past_the_stop() {
        for then in [Turn::FailsToWrite, Turn::WritesNotes, Turn::Shell] {
            assert_eq!(stopped_after(57, then), Some(62));
        }
        assert_eq!(stopped_after(57, Turn::Writes), Some(80));
    }

    /// In a run with a write scope, only a file in the project and outside
    /// its `.tenaz/` is the session's work, however the path is written.
    #[test]
    fn only_project_files_outside_tenaz_are_work() {
        let w = scoped(builder(run_on_params(), true));
        let work = |path: &str| w.writes_work(&serde_json::json!({ "path": path }));
        for path in ["src/a.js", "./src/a.js", "/work/app/src/a.js"] {
            assert!(work(path), "{path}");
        }
        for path in [
            "/tmp/a.js",
            "../a.js",
            "/work/app/.tenaz/notes/T001.md",
            "./src/../.tenaz/notes/T001.md",
        ] {
            assert!(!work(path), "{path}");
        }
        assert!(!w.writes_work(&serde_json::json!({ "patch": "*** src/a.js" })));
    }

    /// A sandboxed run has no scope: only a relative path that stays in the
    /// project and outside `.tenaz/` is work, so an absolute path never
    /// keeps a session past its stop.
    #[test]
    fn without_a_scope_only_relative_project_files_outside_tenaz_are_work() {
        let w = builder(run_on_params(), true);
        let work = |path: &str| w.writes_work(&serde_json::json!({ "path": path }));
        for path in ["src/a.js", "./src/a.js", "src/x/../a.js"] {
            assert!(work(path), "{path}");
        }
        for path in [
            "/work/app/src/a.js",
            "/work/app/.tenaz/notes/T001.md",
            "../a.js",
            "src/../../a.js",
            ".tenaz/notes/T001.md",
            "src/../.tenaz/notes/T001.md",
            "~/a.js",
            "",
            "src/..",
        ] {
            assert!(!work(path), "{path}");
        }
    }

    #[test]
    fn a_session_that_edits_no_code_is_stopped_at_the_turn_stop_while_it_writes() {
        // A reviewer or planner (`idle` off).
        let turns = writing_then(100, &[]);
        let (stopped, _) = play_builder(builder(run_on_params(), false), &turns);
        assert_eq!(stopped, Some(60));
    }

    #[test]
    fn a_ceiling_at_the_stop_or_a_window_of_0_keeps_no_session_past_the_stop() {
        let turns = writing_then(100, &[]);
        let at_stop = RunParams {
            turn_ceiling: 1.5,
            ..run_on_params()
        };
        let no_window = RunParams {
            turn_write_window: 0,
            ..run_on_params()
        };
        for params in [at_stop, no_window] {
            let (stopped, steers) = play_builder(builder(params, true), &turns);
            assert_eq!(stopped, Some(60));
            assert_eq!(steer_keys(&steers), vec![(40, "turns")]);
        }
    }

    #[test]
    fn compaction_steers_then_aborts_and_settled_marks_the_outcome() {
        let mut w = watched();
        assert!(is_steer(
            &w.handle(
                &SessionEvent::CompactionStart {
                    reason: "full".into()
                },
                0,
                String::new
            ),
            "compaction"
        ));
        assert_eq!(
            w.handle(
                &SessionEvent::CompactionStart {
                    reason: "full".into()
                },
                0,
                String::new
            ),
            vec![Action::Abort {
                reason: SECOND_COMPACTION.into()
            }]
        );
        w.handle(&SessionEvent::Settled { aborted: true }, 0, String::new);
        w.finish(Some(0), 1234);
        let m = w.into_metrics();
        assert!(m.aborted && !m.settled && !m.ok());
        assert_eq!(m.compactions, 2);
        assert_eq!(m.duration_ms, 1234);
    }

    /// A compaction the proxy refused ends the session at once, as one out
    /// of context, so a fresh session continues the work.
    #[test]
    fn a_refused_compaction_stops_the_session_out_of_context() {
        let mut w = watched();
        assert_eq!(
            w.handle(&SessionEvent::CompactionRefused, 0, String::new),
            vec![Action::Abort {
                reason: HARNESS_COMPACTION.into()
            }]
        );
        w.handle(&SessionEvent::Settled { aborted: true }, 0, String::new);
        w.finish(Some(0), 1);
        let m = w.into_metrics();
        assert!(m.aborted && m.context_stopped() && !m.compaction_stopped());
        assert_eq!(m.compactions, 1);
    }

    /// A request the proxy sent again with the output limit the engine had
    /// room for is journaled, and is not an engine error: the session goes
    /// on.
    #[test]
    fn a_lowered_output_limit_is_recorded_and_fails_nothing() {
        let mut w = watched();
        let lowered = SessionEvent::OutputLimitLowered {
            asked: 16_384,
            fits: 13_186,
        };
        assert!(w.handle(&lowered, 5, String::new).is_empty());
        message(&mut w, 0, StopReason::Stop, None);
        let m = w.metrics();
        assert_eq!(m.provider_errors, 0);
        assert!(!m.engine_failed() && !m.engine_out_of_memory);
        let event = &m.events[0];
        assert_eq!(event.kind, OUTPUT_LIMIT_LOWERED);
        let detail = event.detail.as_deref().unwrap();
        assert!(
            detail.contains("13186") && detail.contains("16384"),
            "{detail}"
        );
    }

    /// A session nobody watches (the preflight) is only measured: the
    /// refusal counts as a compaction and stops nothing.
    #[test]
    fn an_unwatched_session_is_not_stopped_by_a_refused_compaction() {
        let mut w = Watchdog::new(RunParams::default(), None);
        assert!(
            w.handle(&SessionEvent::CompactionRefused, 0, String::new)
                .is_empty()
        );
        assert_eq!(w.metrics().compactions, 1);
        assert!(w.metrics().abort_reason.is_none());
    }

    /// A session answers its first prompt and is sent another: until that
    /// one is answered it has not settled, and a reply cut at the output
    /// limit in the first does not explain an exit in the second.
    #[test]
    fn a_new_prompt_unsettles_the_session_until_it_is_answered() {
        let mut w = watched();
        message(&mut w, 1000, StopReason::MaxTokens, None);
        w.handle(&SessionEvent::Settled { aborted: false }, 0, String::new);
        assert!(w.metrics().settled);
        w.prompted();
        assert!(w.metrics().ended_early(), "{:?}", w.metrics());
        assert!(!w.metrics().output_truncated());
        w.handle(&SessionEvent::Settled { aborted: false }, 0, String::new);
        assert!(w.metrics().settled);
    }

    #[test]
    fn an_unwatched_session_only_measures() {
        let mut w = Watchdog::new(RunParams::default(), None);
        for n in 1..=8 {
            assert!(call(&mut w, n, "bash", &serde_json::json!({"c": 1}), "same").is_empty());
        }
        assert!(message(&mut w, 1_000_000, StopReason::Stop, None).is_empty());
        assert!(
            w.handle(
                &SessionEvent::CompactionStart { reason: "x".into() },
                0,
                String::new
            )
            .is_empty()
        );
        w.handle(&SessionEvent::Settled { aborted: false }, 0, String::new);
        w.finish(Some(0), 1);
        assert!(w.metrics().ok());
        assert_eq!(w.metrics().tool_calls, 8);
        assert_eq!(w.metrics().tool_in_flight, None, "every call ended");
    }

    #[test]
    fn a_tool_call_still_running_at_exit_is_kept() {
        let mut w = watched();
        call(&mut w, 1, "read", &serde_json::json!({"path": "a"}), "x");
        w.handle(
            &SessionEvent::ToolCallStart {
                id: "c9".into(),
                name: "shell".into(),
                args: serde_json::json!({"command": "head -c 400 x.json"}),
                effect: None,
            },
            0,
            String::new,
        );
        w.finish(Some(1), 1);
        assert_eq!(
            w.metrics().tool_in_flight.as_deref(),
            Some(r#"shell {"command":"head -c 400 x.json"}"#)
        );
    }

    #[test]
    fn the_command_in_flight_is_kept_whole() {
        let mut w = watched();
        assert_eq!(w.command_in_flight(), None, "no call");
        let start = |w: &mut Watchdog, id: &str, name: &str, args| {
            w.handle(
                &SessionEvent::ToolCallStart {
                    id: id.into(),
                    name: name.into(),
                    args,
                    effect: None,
                },
                0,
                String::new,
            );
        };
        start(&mut w, "c1", "read", serde_json::json!({"path": "a"}));
        assert_eq!(w.command_in_flight(), None, "a call that runs no command");
        let mut w = watched();
        let long = format!("{}cat -e a", "x".repeat(IN_FLIGHT_CHARS));
        start(
            &mut w,
            "c1",
            "shell",
            serde_json::json!({ "command": long }),
        );
        w.finish(Some(1), 1);
        assert!(
            w.metrics()
                .tool_in_flight
                .as_deref()
                .unwrap()
                .ends_with('…')
        );
        assert_eq!(w.command_in_flight(), Some(long.as_str()));
    }

    #[test]
    fn prompt_rejection_and_external_aborts_are_recorded_once() {
        let mut w = watched();
        let first = w.handle(
            &SessionEvent::PromptRejected {
                reason: "handled".into(),
            },
            1,
            String::new,
        );
        assert_eq!(
            first,
            vec![Action::Abort {
                reason: "prompt rejected: handled".into()
            }]
        );
        assert!(
            w.abort_external("session exceeded 90 minutes", 2)
                .is_empty()
        );
        assert_eq!(
            w.metrics().abort_reason.as_deref(),
            Some("prompt rejected: handled")
        );
    }

    fn scoped(mut w: Watchdog) -> Watchdog {
        w.set_write_scope(Some(WriteScope::new(
            std::path::Path::new("/work/app"),
            None,
            Vec::new(),
        )));
        w
    }

    fn write(w: &mut Watchdog, n: u32, path: &str) -> Vec<Action> {
        call(w, n, "write", &serde_json::json!({ "path": path }), "ok")
    }

    fn steer_message(actions: &[Action]) -> Option<&str> {
        actions.iter().find_map(|a| match a {
            Action::Steer { key, message } if key == "outside-write" => Some(message.as_str()),
            _ => None,
        })
    }

    #[test]
    fn a_write_in_the_project_is_not_noted() {
        let mut w = scoped(watched());
        assert!(write(&mut w, 1, ".tenaz/spec.md").is_empty());
        assert!(w.metrics().outside_writes.is_empty());
    }

    #[test]
    fn a_write_outside_the_project_is_steered_with_the_file_and_the_project() {
        let mut w = scoped(watched());
        let actions = write(&mut w, 1, "../.tenaz/spec.md");
        let message = steer_message(&actions).expect("an outside-write steer");
        assert!(message.contains("`/work/.tenaz/spec.md`"), "{message}");
        assert!(message.contains("/work/app"), "{message}");
    }

    #[test]
    fn a_session_is_steered_once_for_its_outside_writes() {
        let mut w = scoped(watched());
        write(&mut w, 1, "../.tenaz/spec.md");
        assert!(write(&mut w, 2, "/elsewhere/map.md").is_empty());
    }

    #[test]
    fn each_file_outside_is_noted_once_with_its_event() {
        let mut w = scoped(watched());
        for (n, path) in [(1, "../a"), (2, "/elsewhere/b"), (3, "../a")] {
            write(&mut w, n, path);
        }
        let m = w.metrics();
        assert_eq!(m.outside_writes, ["/work/a", "/elsewhere/b"]);
        let noted: Vec<_> = m
            .events
            .iter()
            .filter(|e| e.kind == "outside-write")
            .filter_map(|e| e.detail.as_deref())
            .collect();
        assert_eq!(noted, ["/work/a", "/elsewhere/b"]);
    }

    #[test]
    fn a_write_outside_still_counts_as_a_write() {
        let mut w = scoped(watched());
        write(&mut w, 1, "/elsewhere/x");
        assert_eq!(w.metrics().writes, 1);
    }

    #[test]
    fn without_a_scope_no_write_is_judged() {
        let mut w = watched();
        assert!(write(&mut w, 1, "/elsewhere/x").is_empty());
        assert!(w.metrics().outside_writes.is_empty());
    }

    #[test]
    fn a_shell_command_that_writes_elsewhere_is_not_seen() {
        let mut w = scoped(watched());
        let shell = serde_json::json!({ "command": "echo x > ../.tenaz/spec.md" });
        assert!(call(&mut w, 1, "bash", &shell, "").is_empty());
        assert!(w.metrics().outside_writes.is_empty());
    }
}
