//! `status`: where a run is, from files only, safe while the run works.

use crate::ledger::JournalRecord;
use crate::memory::{MemorySample, memory_line};
use crate::metrics::{RequestRecord, engine_line};
use crate::project::Project;

/// The status text, or a note when the project is not initialized.
pub fn status_text(project: &Project) -> String {
    let Ok(ledger) = project.load_ledger() else {
        return "Not initialized.\n".to_owned();
    };
    let backlog = project.load_backlog().backlog;
    let mut counts: Vec<(String, usize)> = Vec::new();
    for card in backlog.cards() {
        let key = card.status.to_string();
        match counts.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => counts.push((key, 1)),
        }
    }
    let sessions: Vec<_> = project
        .journal()
        .into_iter()
        .filter_map(|e| match e.record {
            JournalRecord::Session(s) => Some(s),
            _ => None,
        })
        .collect();
    let requests: Vec<RequestRecord> = project
        .read_if(".agent/private/requests.jsonl")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let samples: Vec<MemorySample> = project
        .read_if(".agent/private/memory.jsonl")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let preflight = ledger.preflight.as_ref().map_or_else(
        || "not run".to_owned(),
        |p| {
            format!(
                "reasoning={} cache={}{}",
                p.reasoning,
                p.cache,
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
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let budget =
        (ledger.config.context_window as f64 * ledger.params.context_budget).floor() as u64;
    let cap = ledger
        .context_cap
        .map(|c| format!(", lowered to {c} after the engine ran out of memory"))
        .unwrap_or_default();
    let lines = [
        format!(
            "Phase: {}{}",
            ledger.phase,
            ledger
                .blocked_reason
                .as_ref()
                .map(|r| format!(" ({r})"))
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
    format!("{}\n", lines.join("\n"))
}
