//! Run parameters. They are pinned in `run.json` at `init` and can be
//! overridden with `--set key=value`, using the task loop's camelCase names.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::task::frontmatter::{Value, parse_value};

/// Tunable parameters of a run. Defaults are the task loop's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunParams {
    /// Fresh-context review of every task diff.
    pub review: bool,
    /// The orchestrator packs spec, map, card and notes into the prompt.
    pub briefing: bool,
    /// Build attempts per task before splitting.
    pub max_attempts: u32,
    /// `T007 -> T007a -> T007aa`, then block.
    pub max_split_depth: usize,
    /// Done tasks between replan sessions (0 disables).
    pub replan_every: u32,
    /// Default turns per session (a card may override).
    pub turn_budget: u32,
    /// Fraction of the context window a session should stay under.
    pub context_budget: f64,
    /// The same tool call N times in a row: steer.
    pub loop_steer: u32,
    /// The same tool call N times in a row: abort.
    pub loop_abort: u32,
    /// Window of recent calls for the repeat rule.
    pub repeat_window: usize,
    /// The same call with the same result N times in the window: steer.
    pub repeat_steer: u32,
    /// The same call with the same result N times in the window: abort.
    pub repeat_abort: u32,
    /// Turns without a file change: steer.
    pub idle_steer: u32,
    /// Turns without a file change: abort.
    pub idle_abort: u32,
    /// Provider errors in one session: abort.
    pub provider_error_abort: u32,
    /// Sessions ended by engine errors are retried, uncounted, this many times.
    pub engine_retries: u32,
    /// First backoff; doubles per retry up to 15 minutes.
    pub engine_retry_base_sec: u64,
    /// When the engine reports its maximum context, later sessions are steered at this fraction of it.
    pub capacity_headroom: f64,
    /// A reported maximum below this is memory starvation: wait instead of shrinking the budget.
    pub context_floor: u64,
    /// Reruns after the context budget was lowered, before blocking.
    pub capacity_retries: u32,
    /// Seconds to wait before each of those reruns, while the engine frees
    /// the memory the failed request held (0 reruns at once).
    pub capacity_retry_wait_sec: u64,
    /// Sessions in a row without a memory failure before a lowered context
    /// budget is raised halfway back; doubles after each raise that fails,
    /// up to four times as many (0 keeps it lowered).
    pub capacity_recover_sessions: u32,
    /// A single tool result above this (about 4 characters per token) gets a steer.
    pub big_output_tokens: usize,
    /// A session past this multiple of its context budget is stopped.
    pub context_abort: f64,
    /// Stopped sessions are continued by a fresh session, uncounted, this many times.
    pub context_continuations: u32,
    /// A session is steered to wrap up once, at the pace its context has
    /// grown so far, it would reach the stop within this many turns
    /// (0: steer only at the budget).
    pub context_steer_turns: u32,
    /// An attempt that used up its context continuations and still fails
    /// `verify` is split at once, when the split depth allows, instead of
    /// being retried against the same budget.
    pub context_split: bool,
    /// A builder's briefing shows only the spec requirements its card
    /// requires (the whole spec when the card names none the spec has).
    pub cited_spec: bool,
    /// Wall-clock limit of one session.
    pub session_minutes: u64,
    /// Time limit of one tool call. Past it the turn is stopped and the
    /// session told why; the second time ends the session. 0 turns it off.
    pub tool_minutes: u64,
    /// Time limit of one `verify` run.
    pub verify_timeout_sec: u64,
    /// The most a heavy job a builder declares may ask for (ADR 0010);
    /// 0 turns jobs off.
    pub job_minutes: u64,
    /// Job files one attempt may have, valid or not; one past them is
    /// removed unread and the attempt is judged.
    pub jobs_per_attempt: u32,
    /// Bootstrap sessions per step before the run blocks.
    pub max_bootstrap_attempts: u32,
    /// Final review cycles before the run completes with gaps.
    pub max_final_cycles: u32,
    /// A review diff larger than this is clipped.
    pub diff_limit_bytes: usize,
    /// The `verify` summary shown to a model is clipped to this.
    pub verify_clip_bytes: usize,
    /// Handover notes are clipped to this.
    pub note_clip_bytes: usize,
    /// The repository map is clipped to this.
    pub map_clip_bytes: usize,
    /// Memory sampling period (0 disables).
    pub memory_sample_sec: u64,
    /// Desktop notifications when a run starts, waits on the engine, blocks, ends or crashes.
    pub notify: bool,
    /// The project had code before the run: `verify` is `.tenaz/verify`, a
    /// wrapper around the project's own tests, and the plan fits the change.
    /// Left unset, `init` sets it from whether the repository has commits.
    pub existing_project: Option<bool>,
    /// Let the coding agent also load the skills it finds by itself (its own
    /// skill folders, and with fx those of the folders above the project).
    /// Off, Pi sees only the skills pinned with `init --skill`, if any, and
    /// fx's prompt lists no others, though fx 0.0.14 can still find and load
    /// them (see "Known gaps" in docs/COMPATIBILITY.md).
    pub agent_skills: bool,
    /// The engine proxy parses back tool-call arguments the engine sent as
    /// JSON text where the tool's schema accepts only an object, and logs
    /// each repair in `.tenaz/private/tool-repairs.jsonl`.
    pub repair_tool_args: bool,
    /// Run the coding agent and `verify` in an OS sandbox (Seatbelt on macOS,
    /// Landlock on Linux): they can write only to the project, its git
    /// directory, the temporary directories, on macOS the per-user cache,
    /// and `sandboxAllowWrite`, and never to the git hooks and config or
    /// `.tenaz/run.json`.
    pub sandbox: bool,
    /// More directories a sandboxed run may write to, comma-separated
    /// (`~` is the home directory); the ones that do not exist are skipped.
    pub sandbox_allow_write: String,
    /// Directories a sandboxed run may not read, comma-separated.
    pub sandbox_deny_read: String,
    /// The sampling temperature the engine proxy puts in requests that
    /// carry none (fx sends none). Unset, the engine's own default applies,
    /// which differs between engines (0.8 in mlx-serve's app, for example).
    pub temperature: Option<f64>,
}

impl Default for RunParams {
    fn default() -> Self {
        Self {
            review: true,
            briefing: true,
            max_attempts: 3,
            max_split_depth: 2,
            replan_every: 5,
            turn_budget: 40,
            context_budget: 0.65,
            loop_steer: 3,
            loop_abort: 5,
            repeat_window: 12,
            repeat_steer: 4,
            repeat_abort: 6,
            idle_steer: 12,
            idle_abort: 20,
            provider_error_abort: 3,
            engine_retries: 6,
            engine_retry_base_sec: 60,
            capacity_headroom: 0.8,
            context_floor: 32768,
            capacity_retries: 4,
            capacity_retry_wait_sec: 10,
            capacity_recover_sessions: 4,
            big_output_tokens: 8000,
            context_abort: 1.3,
            context_continuations: 3,
            context_steer_turns: 10,
            context_split: true,
            cited_spec: true,
            session_minutes: 90,
            tool_minutes: 10,
            verify_timeout_sec: 120,
            job_minutes: 60,
            jobs_per_attempt: 3,
            max_bootstrap_attempts: 3,
            max_final_cycles: 2,
            diff_limit_bytes: 40000,
            verify_clip_bytes: 4000,
            note_clip_bytes: 1500,
            map_clip_bytes: 8000,
            memory_sample_sec: 30,
            notify: true,
            existing_project: None,
            temperature: None,
            agent_skills: false,
            repair_tool_args: true,
            sandbox: false,
            sandbox_allow_write: "~/.cargo/registry,~/.cargo/git,~/.npm,~/.cache,~/Library/Caches"
                .to_owned(),
            sandbox_deny_read: "~/.ssh,~/.aws,~/.gnupg".to_owned(),
        }
    }
}

/// An integral `f64` as `i64`, saturating at the ends of the range.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the cast saturates at the ends of the range, which is what this wants"
)]
fn integral(n: f64) -> i64 {
    n as i64
}

/// A rejected `--set key=value`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParamError {
    #[error("unknown parameter {0}; known: {1}")]
    Unknown(String, String),
    #[error("parameter {key} does not accept {value}: {reason}")]
    BadValue {
        key: String,
        value: String,
        reason: String,
    },
    #[error("expected key=value, got {0}")]
    Syntax(String),
}

impl RunParams {
    /// Builders may declare heavy jobs (ADR 0010): `jobMinutes` is not 0.
    pub fn jobs_on(&self) -> bool {
        self.job_minutes > 0
    }

    /// The pause before a rerun after the context budget was lowered.
    pub fn capacity_retry_wait(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.capacity_retry_wait_sec)
    }

    /// Where the `verify` executable is, relative to the project root.
    pub fn verify_path(&self) -> &'static str {
        if self.existing_project == Some(true) {
            ".tenaz/verify"
        } else {
            "verify"
        }
    }

    /// The parameter names, in declaration order.
    pub fn names() -> Vec<String> {
        match serde_json::to_value(Self::default()) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// Applies one `key=value` override, parsed like a card value.
    pub fn apply_override(&mut self, pair: &str) -> Result<(), ParamError> {
        let (key, raw) = pair
            .split_once('=')
            .ok_or_else(|| ParamError::Syntax(pair.to_owned()))?;
        self.set(key, raw)
    }

    /// Sets one parameter from its textual value.
    pub fn set(&mut self, key: &str, raw: &str) -> Result<(), ParamError> {
        let Ok(serde_json::Value::Object(mut map)) = serde_json::to_value(&*self) else {
            return Err(ParamError::Syntax(key.to_owned()));
        };
        if !map.contains_key(key) {
            return Err(ParamError::Unknown(
                key.to_owned(),
                Self::names().join(", "),
            ));
        }
        let value = match parse_value(raw) {
            Value::Number(n) if n.fract() == 0.0 => serde_json::json!(integral(n)),
            Value::Number(n) => serde_json::json!(n),
            Value::Bool(b) => serde_json::json!(b),
            Value::Text(s) => serde_json::json!(s),
            Value::List(_) | Value::Map(_) => {
                return Err(ParamError::BadValue {
                    key: key.to_owned(),
                    value: raw.to_owned(),
                    reason: "a parameter is a number, a boolean or a string".to_owned(),
                });
            }
        };
        map.insert(key.to_owned(), value);
        let updated: Self =
            serde_json::from_value(serde_json::Value::Object(map)).map_err(|error| {
                ParamError::BadValue {
                    key: key.to_owned(),
                    value: raw.to_owned(),
                    reason: error.to_string(),
                }
            })?;
        *self = updated;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let p = RunParams::default();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.context_floor, 32768);
        assert!((p.context_abort - 1.3).abs() < f64::EPSILON);
        assert!(p.notify);
    }

    #[test]
    fn overrides_use_camel_case_keys() {
        let mut p = RunParams::default();
        p.apply_override("replanEvery=2").unwrap();
        p.apply_override("notify=false").unwrap();
        p.apply_override("contextBudget=0.5").unwrap();
        assert_eq!(p.replan_every, 2);
        assert!(!p.notify);
        assert!((p.context_budget - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_run_can_pin_its_temperature() {
        let mut p = RunParams::default();
        assert_eq!(p.temperature, None);
        p.apply_override("temperature=0").unwrap();
        assert_eq!(p.temperature, Some(0.0));
        p.apply_override("temperature=0.6").unwrap();
        assert_eq!(p.temperature, Some(0.6));
    }

    #[test]
    fn unknown_key_lists_the_known_ones() {
        let mut p = RunParams::default();
        let error = p.apply_override("nope=1").unwrap_err();
        assert!(
            matches!(error, ParamError::Unknown(ref k, ref known) if k == "nope" && known.contains("maxAttempts"))
        );
    }

    #[test]
    fn wrong_type_is_rejected() {
        let mut p = RunParams::default();
        assert!(matches!(
            p.apply_override("maxAttempts=soon"),
            Err(ParamError::BadValue { .. })
        ));
        assert!(matches!(
            p.apply_override("maxAttempts"),
            Err(ParamError::Syntax(_))
        ));
        assert_eq!(p, RunParams::default());
    }

    #[test]
    fn an_existing_project_keeps_verify_under_tenaz() {
        let mut p = RunParams::default();
        assert_eq!(p.existing_project, None);
        assert_eq!(p.verify_path(), "verify");
        p.apply_override("existingProject=true").unwrap();
        assert_eq!(p.verify_path(), ".tenaz/verify");
        p.apply_override("existingProject=false").unwrap();
        assert_eq!(p.verify_path(), "verify");
    }

    #[test]
    fn the_sandbox_is_off_unless_asked_for() {
        let mut p = RunParams::default();
        assert!(!p.sandbox);
        p.apply_override("sandbox=true").unwrap();
        p.apply_override("sandboxAllowWrite=~/.m2,/opt/cache")
            .unwrap();
        assert!(p.sandbox);
        assert_eq!(p.sandbox_allow_write, "~/.m2,/opt/cache");
    }

    #[test]
    fn serializes_with_task_loop_names() {
        let json = serde_json::to_value(RunParams::default()).unwrap();
        assert_eq!(json["engineRetryBaseSec"], 60);
        assert_eq!(json["capacityRetryWaitSec"], 10);
        assert_eq!(json["bigOutputTokens"], 8000);
    }
}
