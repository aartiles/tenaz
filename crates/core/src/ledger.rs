//! `run.json` (the ledger: where the run is) and `journal.jsonl` (append-only
//! facts, one JSON object per line). Field names are kept stable so
//! scripts that read them (`jq` over the journal) keep working.

use serde::{Deserialize, Serialize};

use crate::metrics::EngineSummary;
use crate::params::RunParams;
use crate::role::Role;
use crate::session::{StopReason, Usage, WatchdogEvent};
use crate::spec::Coverage;
use crate::task::TaskId;

/// Where the run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Phase {
    Bootstrap,
    Loop,
    Final,
    Complete,
    CompleteWithGaps,
    Blocked,
}

impl Phase {
    /// The run can continue from this phase.
    pub fn is_running(self) -> bool {
        matches!(self, Phase::Bootstrap | Phase::Loop | Phase::Final)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Bootstrap => "BOOTSTRAP",
            Phase::Loop => "LOOP",
            Phase::Final => "FINAL",
            Phase::Complete => "COMPLETE",
            Phase::CompleteWithGaps => "COMPLETE_WITH_GAPS",
            Phase::Blocked => "BLOCKED",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The model and harness a run is pinned to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// `fx`, `pi`, ...
    pub harness: String,
    pub harness_version: String,
    /// The model as the harness names it (`sushi/Qwen3.8-Flash-Next-Sushi-2.6bpw`).
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub context_window: u64,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

impl ModelConfig {
    pub fn supports_images(&self) -> bool {
        self.input.iter().any(|i| i == "image")
    }
}

/// The engine preflight (reasoning, prefix cache, vision).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preflight {
    pub at: String,
    pub ok: bool,
    pub settled: bool,
    pub reasoning: bool,
    pub cache: bool,
    pub vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_answer: Option<String>,
    pub reasoning_chars: usize,
    pub second_turn_cache_read: u64,
    pub duration_ms: u64,
    pub engine_metrics: bool,
    pub engine: Option<EngineSummary>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accepted_weak: bool,
}

/// A skill copied read-only into `.agent/skills/` and hashed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPin {
    pub name: String,
    pub source: String,
    pub sha256: String,
    pub files: usize,
}

/// A builder attempt the orchestrator stopped whose work still passed `verify`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedAttempt {
    pub attempt: u32,
    pub reason: String,
}

/// The task being built.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurrentTask {
    pub task: TaskId,
    /// The commit the task started from.
    pub base: String,
    pub history: Vec<String>,
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped: Vec<StoppedAttempt>,
}

/// The session running right now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inflight {
    pub label: String,
    pub role: Role,
    pub task: Option<TaskId>,
    pub started_at: String,
}

/// `run.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub schema_version: u32,
    pub created_at: String,
    pub brief_sha256: String,
    pub templates_sha256: String,
    pub runner_version: String,
    pub config: ModelConfig,
    pub params: RunParams,
    #[serde(default)]
    pub skills: Vec<SkillPin>,
    pub phase: Phase,
    pub session_seq: u32,
    pub bootstrap_attempts: u32,
    pub done_since_replan: u32,
    pub stuck_replanned: bool,
    pub final_cycles: u32,
    pub current: Option<CurrentTask>,
    pub inflight: Option<Inflight>,
    pub preflight: Option<Preflight>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_cap: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_errors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending_continuation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_before_block: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Ledger {
    pub const SCHEMA_VERSION: u32 = 1;

    /// A fresh ledger at `init`.
    pub fn new(
        created_at: String,
        brief_sha256: String,
        templates_sha256: String,
        runner_version: String,
        config: ModelConfig,
        params: RunParams,
        skills: Vec<SkillPin>,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            created_at,
            brief_sha256,
            templates_sha256,
            runner_version,
            config,
            params,
            skills,
            phase: Phase::Bootstrap,
            session_seq: 0,
            bootstrap_attempts: 0,
            done_since_replan: 0,
            stuck_replanned: false,
            final_cycles: 0,
            current: None,
            inflight: None,
            preflight: None,
            checkpoint: None,
            context_cap: None,
            last_errors: None,
            pending_continuation: false,
            paused_at: None,
            ended_at: None,
            phase_before_block: None,
            blocked_reason: None,
            last_error: None,
        }
    }

    /// The budget a session is steered at: the configured share of the
    /// window, or less once the engine has said it cannot hold more.
    pub fn context_budget_tokens(&self, requested: Option<u64>) -> u64 {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let default =
            (self.config.context_window as f64 * self.params.context_budget).floor() as u64;
        let wanted = requested.unwrap_or(default);
        self.context_cap.map_or(wanted, |cap| wanted.min(cap))
    }

    /// `003-builder-T007`.
    pub fn next_label(&mut self, role: Role, task: Option<&TaskId>) -> String {
        self.session_seq += 1;
        match task {
            Some(task) => format!("{:03}-{}-{task}", self.session_seq, role),
            None => format!("{:03}-{}", self.session_seq, role),
        }
    }
}

/// The session's journal record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub label: String,
    pub role: Role,
    pub task: Option<TaskId>,
    pub attempt: Option<u32>,
    pub ok: bool,
    pub abort_reason: Option<String>,
    pub exit_code: Option<i32>,
    pub last_stop_reason: Option<StopReason>,
    pub turns: u32,
    pub tool_calls: u32,
    pub writes: u32,
    pub usage: Usage,
    pub reasoning_chars: usize,
    pub context_peak: u64,
    pub context_budget: u64,
    pub compactions: u32,
    pub provider_errors: u32,
    pub capacity_limits: Vec<u64>,
    pub image_refusal: Option<String>,
    pub big_outputs: u32,
    pub watchdog: Vec<WatchdogEvent>,
    pub reverted: Vec<String>,
    pub prompt_chars: usize,
    pub duration_ms: u64,
    pub engine: Option<EngineSummary>,
    pub memory: Option<crate::memory::MemoryWindow>,
    /// The harness ended before the turn settled without being stopped:
    /// exit code and stderr tail. Such a session is rerun, uncounted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_exit: Option<String>,
}

/// How a task ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskOutcome {
    Done,
    Split,
    Blocked,
}

/// One line of `journal.jsonl`, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum JournalRecord {
    Session(SessionRecord),
    Paused {
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<Role>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<TaskId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
    },
    Resumed {
        paused_at: String,
    },
    Interrupted {
        label: String,
        role: Role,
        task: Option<TaskId>,
        started_at: String,
    },
    ContextContinuation {
        label: String,
        context_peak: u64,
        context_budget: u64,
        continuation: u32,
    },
    ImageContinuation {
        label: String,
        refusal: String,
        continuation: u32,
    },
    ContextCap {
        label: String,
        engine_max_context: u64,
        context_cap: u64,
        retry: u32,
    },
    EngineRetry {
        label: String,
        retry: u32,
        wait_s: u64,
        /// `engine` (a provider error) or `harness` (the harness ended early).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<String>,
    },
    SkillChanged {
        skill: String,
    },
    BootstrapValidation {
        attempt: u32,
        errors: Vec<String>,
        session_ok: bool,
    },
    SplitRejected {
        task: Option<TaskId>,
        errors: Vec<String>,
    },
    ReplanRejected {
        task: Option<TaskId>,
        errors: Vec<String>,
    },
    SplitAccepted {
        task: Option<TaskId>,
        added: Vec<TaskId>,
    },
    ReplanAccepted {
        task: Option<TaskId>,
        added: Vec<TaskId>,
    },
    Task {
        task: TaskId,
        outcome: TaskOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        children: Option<Vec<TaskId>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        history: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reviewed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stopped_sessions: Option<Vec<StoppedAttempt>>,
    },
    Verify {
        task: TaskId,
        attempt: u32,
        all_ok: bool,
        task_ok: bool,
        task_checks: u64,
        all: Option<serde_json::Value>,
        task_json: Option<serde_json::Value>,
    },
    Review {
        task: TaskId,
        attempt: u32,
        review: u32,
        verdict: Option<String>,
    },
    FinalReview {
        cycle: u32,
        verdict: Option<String>,
        verify_ok: bool,
        coverage: Coverage,
        added: Vec<TaskId>,
    },
    Run {
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    TemplatesRefreshed {
        from: Option<String>,
        to: String,
    },
    RunnerChanged {
        from: Option<String>,
        to: String,
    },
    /// The lease changed hands: which engine was unloaded or loaded and how long it took.
    Lease {
        holder: String,
        transition: String,
        ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// A journal line: the time, then the record's fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub at: String,
    #[serde(flatten)]
    pub record: JournalRecord,
}

impl JournalEntry {
    /// A one-line summary for planner briefings.
    pub fn summary(&self) -> String {
        match &self.record {
            JournalRecord::Session(s) => {
                let outcome = if s.ok {
                    "ok".to_owned()
                } else {
                    format!(
                        "failed ({})",
                        s.abort_reason
                            .clone()
                            .or_else(|| s.last_stop_reason.as_ref().map(|r| r.as_str().to_owned()))
                            .unwrap_or_default()
                    )
                };
                format!("{}: {outcome}, {} turns", s.label, s.turns)
            }
            JournalRecord::Task { task, outcome, .. } => format!(
                "task {task}: {}",
                serde_json::to_value(outcome)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
            JournalRecord::Run { outcome, .. } => format!("run: {outcome}"),
            other => {
                let value = serde_json::to_value(other).unwrap_or_default();
                let kind = value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("record");
                match value.get("task").and_then(serde_json::Value::as_str) {
                    Some(task) => format!("{kind} {task}"),
                    None => kind.to_owned(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ModelConfig {
        ModelConfig {
            harness: "fx".into(),
            harness_version: "0.0.14".into(),
            model: "sushi/m".into(),
            provider: Some("sushi".into()),
            id: "m".into(),
            base_url: Some("http://127.0.0.1:12345/v1".into()),
            context_window: 131072,
            input: vec!["text".into(), "image".into()],
            thinking: Some("medium".into()),
        }
    }

    #[test]
    fn ledger_round_trips_and_keeps_task_loop_names() {
        let ledger = Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0.1.0".into(),
            config(),
            RunParams::default(),
            vec![],
        );
        let json = serde_json::to_value(&ledger).unwrap();
        assert_eq!(json["phase"], "BOOTSTRAP");
        assert_eq!(json["config"]["contextWindow"], 131072);
        assert_eq!(json["params"]["maxAttempts"], 3);
        assert!(json.get("context_cap").is_none());
        let back: Ledger = serde_json::from_value(json).unwrap();
        assert_eq!(back, ledger);
        assert!(back.config.supports_images());
    }

    #[test]
    fn context_budget_honours_requests_and_the_cap() {
        let mut ledger = Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config(),
            RunParams::default(),
            vec![],
        );
        assert_eq!(ledger.context_budget_tokens(None), 85196);
        assert_eq!(ledger.context_budget_tokens(Some(50_000)), 50_000);
        ledger.context_cap = Some(49_190);
        assert_eq!(ledger.context_budget_tokens(None), 49_190);
        assert_eq!(ledger.context_budget_tokens(Some(30_000)), 30_000);
    }

    #[test]
    fn labels_are_sequential_and_name_the_task() {
        let mut ledger = Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config(),
            RunParams::default(),
            vec![],
        );
        assert_eq!(ledger.next_label(Role::Bootstrap, None), "001-bootstrap");
        let t = TaskId::parse("T007").unwrap();
        assert_eq!(
            ledger.next_label(Role::Builder, Some(&t)),
            "002-builder-T007"
        );
    }

    #[test]
    fn journal_records_are_tagged_like_the_task_loop() {
        let entry = JournalEntry {
            at: "now".into(),
            record: JournalRecord::ContextCap {
                label: "003-builder-T001".into(),
                engine_max_context: 61488,
                context_cap: 49190,
                retry: 1,
            },
        };
        let line = serde_json::to_string(&entry).unwrap();
        assert!(line.contains("\"type\":\"context-cap\""));
        assert!(line.contains("\"engine_max_context\":61488,\"context_cap\":49190"));
        let back: JournalEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(back, entry);
        let task = JournalEntry {
            at: "n".into(),
            record: JournalRecord::Task {
                task: TaskId::parse("T004").unwrap(),
                outcome: TaskOutcome::Done,
                children: None,
                history: None,
                attempts: Some(1),
                reviewed: Some(true),
                stopped_sessions: Some(vec![]),
            },
        };
        let line = serde_json::to_string(&task).unwrap();
        assert!(line.contains("\"type\":\"task\",\"task\":\"T004\",\"outcome\":\"done\""));
        assert_eq!(task.summary(), "task T004: done");
        assert_eq!(entry.summary(), "context-cap");
        let phase: Phase = serde_json::from_str("\"COMPLETE_WITH_GAPS\"").unwrap();
        assert_eq!(phase, Phase::CompleteWithGaps);
        assert!(!phase.is_running());
    }
}
