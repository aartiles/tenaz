//! The watchdog: the task loop's rules over the session event stream. It
//! only decides; the session runner sends the steers and aborts.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::params::RunParams;
use crate::session::{
    SessionEvent, SessionMetrics, StopReason, WatchdogEvent, capacity_limit, is_image_refusal,
    is_write_tool,
};
use crate::util::{approx_tokens, sha256_hex};

/// Requests the context pace is measured over.
const PACE_WINDOW: usize = 6;

/// What a watched session is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    pub turns: u32,
    pub context_tokens: u64,
    /// Count turns without a file change (builder and bootstrap sessions).
    pub idle: bool,
}

/// What the session runner must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Steer { key: String, message: String },
    Abort { reason: String },
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
    fingerprint: Option<String>,
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
            fingerprint: None,
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

    /// Records something the runner did outside the rules (the wall-clock
    /// limit, an abort it decided itself).
    pub fn abort_external(&mut self, reason: &str, elapsed_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        self.abort(reason, elapsed_ms, &mut actions);
        actions
    }

    /// Sets the final process facts once the harness has exited.
    pub fn finish(&mut self, exit_code: Option<i32>, duration_ms: u64) {
        self.metrics.exit_code = exit_code;
        self.metrics.duration_ms = duration_ms;
    }

    /// Feeds one event. `fingerprint` is evaluated only when the idle rule
    /// needs the working tree at the end of a turn.
    pub fn handle(
        &mut self,
        event: &SessionEvent,
        elapsed_ms: u64,
        fingerprint: impl FnOnce() -> String,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
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
                        self.abort("second context compaction", elapsed_ms, &mut actions);
                    } else {
                        self.steer("compaction", "Context is being compacted, which loses detail. Finish the current step, run ./verify, write your handoff notes, and stop.", elapsed_ms, &mut actions);
                    }
                }
            }
            SessionEvent::Settled { aborted } => {
                if *aborted || self.metrics.abort_reason.is_some() {
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

    #[allow(clippy::too_many_arguments)]
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
            self.metrics.provider_errors += 1;
            let message = error_message.unwrap_or("");
            self.event("provider-error", Some(message), elapsed_ms);
            if let Some(limit) = capacity_limit(message) {
                self.metrics.capacity_limits.push(limit);
            }
            if is_image_refusal(message) {
                self.metrics.image_refusal = Some(message.to_owned());
            }
            if self.watch.is_some()
                && self.metrics.provider_errors >= self.params.provider_error_abort
            {
                self.abort(
                    &format!("{} provider errors", self.metrics.provider_errors),
                    elapsed_ms,
                    actions,
                );
            }
        }
        if let Some(watch) = self.watch {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let hard = (watch.context_tokens as f64 * self.params.context_abort) as u64;
            if self.metrics.context_peak > hard {
                self.abort(
                    &format!(
                        "context budget exceeded ({}/{} tokens)",
                        self.metrics.context_peak, watch.context_tokens
                    ),
                    elapsed_ms,
                    actions,
                );
            } else if self.metrics.context_peak > watch.context_tokens {
                let message = format!(
                    "This session has used {} tokens of context, over its budget of {}. Finish the smallest coherent step, run ./verify, write your handoff notes, and stop. The next session continues from the repository.",
                    self.metrics.context_peak, watch.context_tokens
                );
                self.steer("context", &message, elapsed_ms, actions);
            } else if let Some(turns) = self.turns_to_stop(hard)
                && turns <= u64::from(self.params.context_steer_turns)
            {
                let message = format!(
                    "This session has used {} tokens of context. At its pace so far it reaches the stop at {hard} tokens in about {turns} turns, and a stopped session leaves no handoff. Finish the smallest coherent step, run ./verify, write your handoff notes, and stop. The next session continues from the repository.",
                    self.metrics.context_peak
                );
                self.steer("context-pace", &message, elapsed_ms, actions);
            }
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
                    &format!("{idle} turns without changing any file"),
                    elapsed_ms,
                    actions,
                );
            } else if idle >= self.params.idle_steer {
                let message = format!(
                    "You have gone {idle} turns without changing any file. Stop investigating and make the change the task needs, or write your notes explaining what blocks you and stop."
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
                "You have used your turn budget ({}). Finish the current step, run ./verify, write your handoff notes, and stop.",
                watch.turns
            );
            self.steer("turns", &message, elapsed_ms, actions);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Usage;

    fn watched() -> Watchdog {
        Watchdog::new(
            RunParams::default(),
            Some(WatchConfig {
                turns: 40,
                context_tokens: 60_000,
                idle: true,
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
        let stop = message(&mut w, 80_000, StopReason::ToolUse, None);
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
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty()
        );
        assert!(
            w.handle(&SessionEvent::TurnEnd, 0, || "b".into())
                .is_empty()
        );
        let abort = w.handle(&SessionEvent::TurnEnd, 0, || "b".into());
        assert_eq!(
            abort,
            vec![Action::Abort {
                reason: "3 turns without changing any file".into()
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
                reason: "second context compaction".into()
            }]
        );
        w.handle(&SessionEvent::Settled { aborted: true }, 0, String::new);
        w.finish(Some(0), 1234);
        let m = w.into_metrics();
        assert!(m.aborted && !m.settled && !m.ok());
        assert_eq!(m.compactions, 2);
        assert_eq!(m.duration_ms, 1234);
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
}
