//! The watchdog: the task loop's rules over the session event stream. It
//! only decides; the session runner sends the steers and aborts.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::params::RunParams;
use crate::session::{
    SECOND_COMPACTION, SessionEvent, SessionMetrics, StopReason, WatchdogEvent, capacity_limit,
    is_image_refusal, is_out_of_memory, is_read_tool, is_write_tool,
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
    /// starts a turn after the tool calls; fx before them, so there the
    /// stop waits for the reply's first token or its end. A session that
    /// ends instead ends on the stop.
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
    /// Arguments of every read so far: a read of something new is progress.
    reads: HashSet<String>,
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
            recent: VecDeque::new(),
            call_args: HashMap::new(),
            call_started: HashMap::new(),
            tool_timeouts: 0,
            interrupting: false,
            held_stop: None,
            reads: HashSet::new(),
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

    /// Checks the files the write tools name against `scope`.
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
                "[orchestrator] Your call `{call}` ran for more than {minutes} minutes and was stopped. It was probably waiting for input, serving, or looping. Find out why by running it with a time limit (`timeout 60 <command>`), fix the cause, and go on with your task. Give any command that may run long a time limit: a call that runs over {minutes} minutes again ends this session."
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
        let shown = ["command", "cmd", "path"]
            .iter()
            .find_map(|k| args.get(*k).and_then(serde_json::Value::as_str))
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
        self.metrics.tool_in_flight =
            self.call_args
                .iter()
                .min_by_key(|(id, _)| id.as_str())
                .map(|(_, (name, args))| {
                    let call = format!("{name} {args}");
                    match call.char_indices().nth(IN_FLIGHT_CHARS) {
                        Some((cut, _)) => format!("{}…", &call[..cut]),
                        None => call,
                    }
                });
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
        match event {
            SessionEvent::PromptRejected { reason } => self.abort(
                &format!("prompt rejected: {reason}"),
                elapsed_ms,
                &mut actions,
            ),
            SessionEvent::ToolCallStart { id, name, args } => {
                self.tool_start(id, name, args, elapsed_ms, &mut actions);
            }
            SessionEvent::ToolCallEnd {
                id,
                name,
                result_text,
                is_error,
            } => {
                if *is_error {
                    self.metrics.tool_errors += 1;
                    if self.metrics.first_tool_error.is_none() {
                        let shown: String = result_text.chars().take(200).collect();
                        self.metrics.first_tool_error = Some(format!("{name}: {shown}"));
                    }
                }
                self.tool_end(id, name, result_text, elapsed_ms, &mut actions);
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
            SessionEvent::CompactionStart { reason } => {
                self.metrics.compactions += 1;
                self.event("compaction", Some(reason), elapsed_ms);
                if self.watch.is_some() {
                    if self.metrics.compactions >= 2 {
                        self.abort(SECOND_COMPACTION, elapsed_ms, &mut actions);
                    } else {
                        let wrap_up = self.watch.map_or("", |w| w.wrap_up);
                        let message =
                            format!("Context is being compacted, which loses detail. {wrap_up}");
                        self.steer("compaction", &message, elapsed_ms, &mut actions);
                    }
                }
            }
            SessionEvent::Settled { aborted } => {
                self.call_started.clear();
                let interrupted = std::mem::take(&mut self.interrupting);
                if *aborted && interrupted && self.metrics.abort_reason.is_none() {
                    // The turn a tool timeout stopped; the session goes on.
                } else if *aborted || self.metrics.abort_reason.is_some() {
                    self.metrics.aborted = true;
                } else {
                    self.metrics.settled = true;
                }
            }
            SessionEvent::TurnStart
            | SessionEvent::FirstToken
            | SessionEvent::PermissionRequest { .. }
            | SessionEvent::Exited { .. } => {}
        }
        actions
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

    /// A tool call ended: a held stop waits for no more calls.
    fn call_ended(&mut self, id: &str) {
        self.call_started.remove(id);
        if let Some(held) = self.held_stop.as_mut() {
            held.calls_ended = self.call_started.is_empty();
        }
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
        self.call_started.insert(id.to_owned(), elapsed_ms);
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
        if is_write_tool(name) {
            self.metrics.writes += 1;
            self.idle_turns = -1;
            self.check_write_target(args, elapsed_ms, actions);
        } else if is_read_tool(name) && self.reads.insert(args.to_string()) {
            self.idle_turns = -1;
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
        if !text.is_empty() {
            self.metrics.texts.push(text.to_owned());
        }
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
        }
        self.context_rules(stop_reason, elapsed_ms, actions);
    }

    /// A request the engine failed: what its `message` says (a capacity
    /// limit, a refused image, memory run out), and the abort after
    /// `providerErrorAbort` of them.
    fn provider_error(&mut self, message: &str, elapsed_ms: u64, actions: &mut Vec<Action>) {
        self.metrics.provider_errors += 1;
        self.event("provider-error", Some(message), elapsed_ms);
        if let Some(limit) = capacity_limit(message) {
            self.metrics.capacity_limits.push(limit);
        }
        if is_image_refusal(message) {
            self.metrics.image_refusal = Some(message.to_owned());
        }
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
        let hard = (f64::from(watch.turns) * 1.5).ceil();
        if f64::from(self.metrics.turns) >= hard {
            self.abort(
                &format!(
                    "turn budget exceeded ({}/{})",
                    self.metrics.turns, watch.turns
                ),
                elapsed_ms,
                actions,
            );
        } else if self.metrics.turns >= watch.turns {
            let message = format!(
                "You have used your turn budget ({}). {}",
                watch.turns, watch.wrap_up
            );
            self.steer("turns", &message, elapsed_ms, actions);
        }
    }
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

    #[test]
    fn provider_errors_record_capacity_and_images_then_abort() {
        let mut w = watched();
        let capacity =
            "Prompt too large. Maximum context at KV8 and prefill chunk 512: 61488 tokens.";
        assert!(message(&mut w, 0, StopReason::Error, Some(capacity)).is_empty());
        assert!(
            message(
                &mut w,
                0,
                StopReason::Error,
                Some("This request has at most 2 images allowed")
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
        w.handle(
            &SessionEvent::ToolCallStart {
                id: "c".into(),
                name: "edit".into(),
                args: serde_json::Value::Null,
            },
            0,
            String::new,
        );
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty()
        );
        assert_eq!(w.metrics().writes, 1);
        // So does a read of something not read before, but not a repeat.
        let read = |path: &str| SessionEvent::ToolCallStart {
            id: "r".into(),
            name: "read".into(),
            args: serde_json::json!({ "path": path }),
        };
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

    #[test]
    fn turn_budget_steers_at_the_budget_and_aborts_at_one_and_a_half() {
        let mut w = Watchdog::new(
            RunParams::default(),
            Some(WatchConfig {
                turns: 4,
                context_tokens: 60_000,
                idle: false,
                wrap_up: Role::Builder.wrap_up(),
            }),
        );
        let mut steered_at = None;
        let mut aborted_at = None;
        for turn in 1..=6 {
            for action in w.handle(&SessionEvent::TurnEnd, 0, || {
                panic!("idle off: fingerprint must not be computed")
            }) {
                match action {
                    Action::Steer { key, .. } if key == "turns" => steered_at = Some(turn),
                    Action::Abort { .. } => aborted_at = Some(turn),
                    Action::Steer { key, .. } => panic!("unexpected steer {key}"),
                    Action::Interrupt { message } => panic!("unexpected interrupt {message}"),
                }
            }
        }
        assert_eq!(steered_at, Some(4));
        assert_eq!(aborted_at, Some(6));
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
