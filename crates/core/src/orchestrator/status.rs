//! `status`: where a run is, from files only, safe while the run works.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::ledger::{CacheCheck, JournalRecord, Ledger, Phase, SessionRecord};
use crate::memory::{MemorySample, memory_line};
use crate::metrics::{RequestRecord, engine_line, prompt_cache_percent};
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
    let requests = requests(project);
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
                p.cache_check(),
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
    let mut lines = vec![
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
    ];
    lines.extend(model_lines(&ledger));
    lines.extend([
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
        context_budget_line(&ledger),
        format!(
            "Sessions: {} ({} stopped or failed), compactions {}",
            sessions.len(),
            sessions.iter().filter(|s| !s.ok).count(),
            sessions.iter().map(|s| s.compactions).sum::<u32>()
        ),
        engine_line(&requests, cache_percent(&ledger, &requests)),
        memory_line(&samples),
    ]);
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

/// `Context budget: N tokens`, the builder's when roles have their own
/// models, and the cap after a memory failure when it holds the builder
/// back (a raise toward a larger model's budget can pass it).
fn context_budget_line(ledger: &Ledger) -> String {
    let whose = if ledger.config.role_models.is_empty() {
        ""
    } else {
        " for the builder"
    };
    let budget = ledger.builder_budget_tokens();
    let cap = ledger
        .context_cap
        .filter(|c| *c < budget)
        .map(|c| format!(", lowered to {c} after the engine ran out of memory"))
        .unwrap_or_default();
    format!("Context budget: {budget} tokens{whose}{cap}")
}

/// The `Model:` line and, when some roles have a model of their own, the
/// `Role models:` line after it.
fn model_lines(ledger: &Ledger) -> Vec<String> {
    let config = &ledger.config;
    let mut lines = vec![format!(
        "Model: {} (thinking {}), {} {}",
        config.model,
        config.thinking.as_deref().unwrap_or("default"),
        config.harness,
        config.harness_version
    )];
    if !config.role_models.is_empty() {
        let models: Vec<String> = config
            .role_models
            .iter()
            .map(|(role, m)| format!("{role} {} ({} tokens)", m.model, m.context_window))
            .collect();
        lines.push(format!("Role models: {}", models.join(", ")));
    }
    lines
}

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

fn requests(project: &Project) -> Vec<RequestRecord> {
    project
        .read_if(".tenaz/private/requests.jsonl")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Whether the engine reports cached tokens, as the preflight saw it.
fn cache_reported(ledger: &Ledger) -> Option<bool> {
    ledger.preflight.as_ref().and_then(|p| p.cache_reported)
}

/// The share of prompt tokens read from the cache; `None` without
/// requests, or when the engine does not report cached tokens.
fn cache_percent(ledger: &Ledger, requests: &[RequestRecord]) -> Option<f64> {
    let unreported = ledger
        .preflight
        .as_ref()
        .is_some_and(|p| p.cache_check() == CacheCheck::NotReported);
    prompt_cache_percent(requests).filter(|_| !unreported)
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
        "prompt_cache_reported": cache_reported(&ledger),
        "prompt_cache_percent": cache_percent(&ledger, &requests(project)),
        "model": ledger.config.model,
        "role_models": ledger
            .config
            .role_models
            .iter()
            .map(|(role, m)| (role.as_str(), json!({
                "model": m.model,
                "context_window": m.context_window,
            })))
            .collect::<BTreeMap<_, _>>(),
        "harness": ledger.config.harness,
        "origin": ledger.origin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{HarnessKind, ModelConfig};
    use crate::params::RunParams;
    use crate::role::Role;
    use crate::role_models::RoleModel;

    fn project_with(role_models: BTreeMap<Role, RoleModel>) -> (tempfile::TempDir, Project) {
        project_capped(role_models, None)
    }

    fn project_capped(
        role_models: BTreeMap<Role, RoleModel>,
        context_cap: Option<u64>,
    ) -> (tempfile::TempDir, Project) {
        let work = tempfile::tempdir().unwrap();
        let project = Project::new(work.path());
        let config = ModelConfig {
            harness: HarnessKind::Fx,
            harness_version: "0.0.14".into(),
            model: "sushi/A".into(),
            provider: Some("sushi".into()),
            id: "A".into(),
            base_url: None,
            context_window: 131_072,
            input: vec!["text".into()],
            thinking: None,
            role_models,
        };
        let mut ledger = Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config,
            RunParams::default(),
            vec![],
        );
        ledger.context_cap = context_cap;
        project.save_ledger(&ledger).unwrap();
        (work, project)
    }

    #[test]
    fn status_names_the_roles_played_by_another_model() {
        let reviewer = RoleModel {
            model: "sushi/B".into(),
            id: "B".into(),
            context_window: 65536,
            input: vec!["text".into()],
        };
        let (_work, project) = project_with(BTreeMap::from([(Role::Reviewer, reviewer)]));
        let text = status_text(&project);
        assert!(
            text.contains(
                "Model: sushi/A (thinking default), fx 0.0.14\nRole models: reviewer sushi/B (65536 tokens)\n"
            ),
            "{text}"
        );
        let json = status_json(&project);
        assert_eq!(json["model"], "sushi/A");
        assert_eq!(
            json["role_models"],
            json!({"reviewer": {"model": "sushi/B", "context_window": 65536}})
        );
    }

    #[test]
    fn the_context_budget_is_the_builder_s_model_s() {
        let builder = RoleModel {
            model: "sushi/B".into(),
            id: "B".into(),
            context_window: 65536,
            input: vec!["text".into()],
        };
        let (_work, project) = project_with(BTreeMap::from([(Role::Builder, builder)]));
        let text = status_text(&project);
        assert!(
            text.contains("Context budget: 42598 tokens for the builder\n"),
            "{text}"
        );
    }

    /// A cap raised toward a larger model's budget no longer holds the
    /// builder back.
    #[test]
    fn a_cap_is_shown_only_while_it_lowers_the_builder_s_budget() {
        let line = |cap| {
            let (_work, project) = project_capped(BTreeMap::new(), Some(cap));
            let text = status_text(&project);
            text.lines()
                .find(|l| l.starts_with("Context budget:"))
                .map(str::to_owned)
        };
        assert_eq!(
            line(49_190).as_deref(),
            Some(
                "Context budget: 85196 tokens, lowered to 49190 after the engine ran out of memory"
            )
        );
        assert_eq!(
            line(109_791).as_deref(),
            Some("Context budget: 85196 tokens")
        );
    }

    #[test]
    fn without_role_models_status_has_no_line_and_an_empty_map() {
        let (_work, project) = project_with(BTreeMap::new());
        let text = status_text(&project);
        assert!(!text.contains("Role models"), "{text}");
        assert!(text.contains("Context budget: 85196 tokens\n"), "{text}");
        assert_eq!(status_json(&project)["role_models"], json!({}));
    }
}
