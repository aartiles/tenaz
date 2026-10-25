//! `status`: where a run is, from files only, safe while the run works.

use serde_json::{Value, json};
use std::fmt::Write as _;

use crate::ledger::{JournalRecord, Ledger, Phase, SessionRecord};
use crate::memory::{MemorySample, memory_line};
use crate::metrics::{RequestRecord, engine_line};
use crate::project::Project;

/// The status text, or a note when the project is not initialized.
pub fn status_text(project: &Project) -> String {
    if let Some(note) = project.legacy_layout_note() {
        return format!("{note}\n");
    }
    let Ok(ledger) = project.load_ledger() else {
        return "Not initialized.\n".to_owned();
    };
    let counts = task_counts(project);
    let active = project.active_run();
    let sessions = sessions(project);
    let requests: Vec<RequestRecord> = project
        .read_if(".tenaz/private/requests.jsonl")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let samples: Vec<MemorySample> = project
        .read_if(".tenaz/private/memory.jsonl")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let preflight = ledger.preflight.as_ref().map_or_else(
        || "not run".to_owned(),
        |p| {
            format!(
                "reasoning={} cache={}{}{}",
                p.reasoning,
                p.cache,
                p.tools.map(|t| format!(" tools={t}")).unwrap_or_default(),
                if p.accepted_weak {
                    " (accepted weak)"
                } else {
                    ""
                }
            )
        },
    );
    let current = ledger.current.as_ref().map_or_else(
        || "none".to_owned(),
        |c| format!("{} ({} failed attempts)", c.task, c.history.len()),
    );
    let inflight = ledger
        .inflight
        .as_ref()
        .map(|i| format!(", in flight {}", i.label))
        .unwrap_or_default();
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a context window times a fraction is far below 2^53 and floors to a positive u64"
    )]
    let budget =
        (ledger.config.context_window as f64 * ledger.params.context_budget).floor() as u64;
    let cap = ledger
        .context_cap
        .map(|c| format!(", lowered to {c} after the engine ran out of memory"))
        .unwrap_or_default();
    let lines = [
        format!(
            "Phase: {}{}{}",
            ledger.phase,
            if ledger.phase == Phase::Bootstrap {
                format!(", step {}", ledger.bootstrap_step)
            } else {
                String::new()
            },
            ledger
                .blocked_reason
                .as_ref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        format!(
            "State: {}{}",
            run_state(&ledger, active.is_some()),
            active
                .filter(|p| *p > 0)
                .map(|p| format!(" (pid {p})"))
                .unwrap_or_default()
        ),
        format!(
            "Model: {} (thinking {}), {} {}",
            ledger.config.model,
            ledger
                .config
                .thinking
                .clone()
                .unwrap_or_else(|| "default".to_owned()),
            ledger.config.harness,
            ledger.config.harness_version
        ),
        format!("Preflight: {preflight}"),
        format!(
            "Tasks: {}",
            if counts.is_empty() {
                "none".to_owned()
            } else {
                counts
                    .iter()
                    .map(|(k, n)| format!("{k} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
        format!("Current: {current}{inflight}"),
        format!("Context budget: {budget} tokens{cap}"),
        format!(
            "Sessions: {} ({} stopped or failed), compactions {}",
            sessions.len(),
            sessions.iter().filter(|s| !s.ok).count(),
            sessions.iter().map(|s| s.compactions).sum::<u32>()
        ),
        engine_line(&requests),
        memory_line(&samples),
    ];
    let mut text = lines.join("\n");
    let repairs = tool_repairs(project);
    if repairs > 0 {
        let _ = write!(
            text,
            "
Tool-call repairs: {repairs} (tool-call arguments the proxy fixed before the harness saw them; .tenaz/private/{TOOL_REPAIRS})"
        );
    }
    format!("{text}\n")
}

/// The engine proxy's log of repaired tool-call arguments, under `.tenaz/private/`.
pub const TOOL_REPAIRS: &str = "tool-repairs.jsonl";

/// How many tool calls the engine proxy repaired.
fn tool_repairs(project: &Project) -> usize {
    std::fs::read_to_string(project.private_dir().join(TOOL_REPAIRS)).map_or(0, |text| {
        text.lines().filter(|l| !l.trim().is_empty()).count()
    })
}

/// Tasks per status, in the order the backlog first shows each status.
fn task_counts(project: &Project) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for card in project.load_backlog().backlog.cards() {
        let key = card.status.to_string();
        match counts.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => counts.push((key, 1)),
        }
    }
    counts
}

fn sessions(project: &Project) -> Vec<SessionRecord> {
    project
        .journal()
        .into_iter()
        .filter_map(|e| match e.record {
            JournalRecord::Session(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// Where the run stands, in one word: `running` while a `tenaz run` works
/// on it, else `complete`, `complete_with_gaps`, `blocked`, `paused`, or
/// `interrupted` (the last run ended without pausing, killed or failed;
/// `tenaz run` continues it).
pub fn run_state(ledger: &Ledger, active: bool) -> &'static str {
    match ledger.phase {
        _ if active => "running",
        Phase::Complete => "complete",
        Phase::CompleteWithGaps => "complete_with_gaps",
        Phase::Blocked => "blocked",
        _ if ledger.paused_at.is_some() => "paused",
        _ => "interrupted",
    }
}

/// The status as one JSON object, for scripts and agents. Its field names
/// stay stable across versions.
pub fn status_json(project: &Project) -> Value {
    if let Some(note) = project.legacy_layout_note() {
        return json!({"initialized": false, "note": note});
    }
    let Ok(ledger) = project.load_ledger() else {
        return json!({"initialized": false});
    };
    let tasks: serde_json::Map<String, Value> = task_counts(project)
        .into_iter()
        .map(|(k, n)| (k, json!(n)))
        .collect();
    let sessions = sessions(project);
    let pid = project.active_run();
    json!({
        "initialized": true,
        "phase": ledger.phase.as_str(),
        "state": run_state(&ledger, pid.is_some()),
        "pid": pid.filter(|p| *p > 0),
        "blocked_reason": ledger.blocked_reason,
        "last_error": ledger.last_error,
        "paused_at": ledger.paused_at,
        "ended_at": ledger.ended_at,
        "tasks": tasks,
        "current": ledger.current.as_ref().map(|c| json!({
            "task": c.task,
            "failed_attempts": c.history.len(),
        })),
        "inflight": ledger.inflight,
        "sessions": sessions.len(),
        "sessions_failed": sessions.iter().filter(|s| !s.ok).count(),
        "tool_repairs": tool_repairs(project),
        "model": ledger.config.model,
        "harness": ledger.config.harness,
        "origin": ledger.origin,
    })
}
