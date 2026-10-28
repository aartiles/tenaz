//! One session, start to journal record: bookkeeping, the harness run,
//! the reverts of out-of-role changes, and the record.

use crate::ledger::{Inflight, JournalRecord, SessionRecord};
use crate::memory::MemoryWindow;
use crate::metrics::{RequestRecord, engine_summary};
use crate::orchestrator::{RunError, Runner};
use crate::ownership;
use crate::ports::Vcs;
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::task::TaskId;
use crate::watchdog::WatchConfig;

use super::{HarnessRun, SessionCtx, SessionResult};

/// A session as it started.
struct Started<'a> {
    label: String,
    role: Role,
    ctx: &'a SessionCtx,
    prompt: &'a str,
    watch: WatchConfig,
}

/// What a finished session left besides its metrics.
struct Left {
    requests: Vec<RequestRecord>,
    memory: Option<MemoryWindow>,
    reverted: Vec<String>,
}

impl Runner {
    /// One session, start to journal record.
    pub(crate) async fn session_once(
        &mut self,
        role: Role,
        prompt: &str,
        ctx: &SessionCtx,
    ) -> Result<SessionResult, RunError> {
        let started = self.start_session(role, prompt, ctx).await?;
        let (metrics, requests) = self
            .run_harness(HarnessRun {
                label: &started.label,
                prompts: &[started.prompt.to_owned()],
                cwd: self.project.root().to_path_buf(),
                watch: Some(started.watch),
                persist: true,
                own_checks: started
                    .ctx
                    .task
                    .as_ref()
                    .filter(|_| started.role == Role::Builder),
            })
            .await?;
        let left = self.after_harness(&started, requests).await?;
        let record = session_record(&started, &metrics, left);
        self.end_session(&record, &metrics)?;
        Ok(SessionResult { record, metrics })
    }

    /// Commits what is left before the session, marks it in flight, keeps
    /// its prompt, and sets its watch, effort and memory sampling.
    async fn start_session<'a>(
        &mut self,
        role: Role,
        prompt: &'a str,
        ctx: &'a SessionCtx,
    ) -> Result<Started<'a>, RunError> {
        if !self.deps.vcs.changed_paths()?.is_empty() {
            self.deps
                .vcs
                .commit_all(&format!("tenaz: bookkeeping before {role}"))?;
        }
        let label = self.ledger.next_label(role, ctx.task.as_ref());
        self.ledger.inflight = Some(Inflight {
            label: label.clone(),
            role,
            task: ctx.task.clone(),
            started_at: self.now(),
        });
        self.save()?;
        self.project
            .write(&format!(".tenaz/private/{label}.prompt.md"), prompt)?;
        self.log(&format!("Session {label} started"));
        let watch = WatchConfig {
            turns: ctx.turns.unwrap_or(self.params.turn_budget),
            context_tokens: self.ledger.context_budget_tokens(ctx.context_tokens),
            idle: role.edits_code(),
            wrap_up: role.wrap_up(),
        };
        if let Some(dial) = &self.deps.effort {
            dial.set_lowered(self.ledger.lower_effort);
            if self.ledger.lower_effort {
                self.log(&format!(
                    "  {label}: the last reply stopped at the output limit; this session reasons at a lower effort"
                ));
            }
        }
        if let Some(sampler) = &self.sampler {
            sampler.begin(&label).await;
        }
        Ok(Started {
            label,
            role,
            ctx,
            prompt,
            watch,
        })
    }

    /// What the session left once its harness ended: the memory window,
    /// the reverts of out-of-role changes, and its requests.
    async fn after_harness(
        &mut self,
        started: &Started<'_>,
        requests: Vec<RequestRecord>,
    ) -> Result<Left, RunError> {
        let memory = match &self.sampler {
            Some(sampler) => sampler.end().await,
            None => None,
        };
        let reverted = self.enforce_ownership(started.role, started.ctx.task.as_ref())?;
        self.check_skills()?;
        if !reverted.is_empty() {
            self.log(&format!(
                "  {}: reverted out-of-role changes: {}",
                started.label,
                reverted.join(", ")
            ));
        }
        Ok(Left {
            requests,
            memory,
            reverted,
        })
    }

    /// Journals the session, clears it from flight, and logs how it ended.
    fn end_session(
        &mut self,
        record: &SessionRecord,
        metrics: &SessionMetrics,
    ) -> Result<(), RunError> {
        self.journal(JournalRecord::Session(record.clone()));
        self.ledger.inflight = None;
        self.ledger.lower_effort = metrics.output_truncated();
        self.save()?;
        self.log(&format!(
            "Session {} {} after {} turns, peak context {}",
            record.label,
            outcome(record.ok, metrics),
            metrics.turns,
            metrics.context_peak
        ));
        Ok(())
    }

    /// Reverts the changes a session left where its role may not write.
    fn enforce_ownership(
        &self,
        role: Role,
        task: Option<&TaskId>,
    ) -> Result<Vec<String>, RunError> {
        let vcs = self.deps.vcs.as_ref();
        let changed = vcs.changed_paths()?;
        let reverted = ownership::partition(
            role,
            changed.iter().map(String::as_str),
            task,
            &RepoView(vcs),
        );
        for path in &reverted {
            vcs.revert_path(path)?;
        }
        Ok(reverted)
    }
}

/// The journal record of a session that `started` and ended with
/// `metrics`, leaving `left`.
fn session_record(started: &Started<'_>, metrics: &SessionMetrics, left: Left) -> SessionRecord {
    SessionRecord {
        label: started.label.clone(),
        role: started.role,
        task: started.ctx.task.clone(),
        attempt: started.ctx.attempt,
        ok: metrics.ok(),
        abort_reason: metrics.abort_reason.clone(),
        exit_code: metrics.exit_code,
        last_stop_reason: metrics.last_stop_reason.clone(),
        turns: metrics.turns,
        tool_calls: metrics.tool_calls,
        writes: metrics.writes,
        usage: metrics.usage,
        reasoning_chars: metrics.reasoning_chars,
        context_peak: metrics.context_peak,
        context_budget: started.watch.context_tokens,
        compactions: metrics.compactions,
        provider_errors: metrics.provider_errors,
        capacity_limits: metrics.capacity_limits.clone(),
        image_refusal: metrics.image_refusal.clone(),
        big_outputs: metrics.big_outputs,
        watchdog: metrics.events.clone(),
        reverted: left.reverted,
        prompt_chars: started.prompt.len(),
        duration_ms: metrics.duration_ms,
        engine: engine_summary(&left.requests),
        memory: left.memory,
        harness_exit: metrics.harness_exit.clone(),
    }
}

/// How a session that was `ok` (or not) ended, for its log line.
fn outcome(ok: bool, metrics: &SessionMetrics) -> String {
    if ok {
        return "settled".to_owned();
    }
    let why = metrics
        .stop_cause()
        .or_else(|| metrics.exit_code.map(|c| c.to_string()))
        .unwrap_or_default();
    format!("failed ({why})")
}

/// The repository facts the ownership rules need, read from git.
struct RepoView<'a>(&'a dyn Vcs);

impl ownership::RepoFacts for RepoView<'_> {
    fn in_head(&self, path: &str) -> bool {
        self.0.in_head(path)
    }
    fn head_content(&self, path: &str) -> Option<String> {
        self.0.head_content(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_session_settled() {
        assert_eq!(outcome(true, &SessionMetrics::default()), "settled");
    }

    #[test]
    fn a_failed_session_says_why_it_stopped() {
        let metrics = SessionMetrics {
            abort_reason: Some("loop".to_owned()),
            exit_code: Some(1),
            ..Default::default()
        };
        assert_eq!(outcome(false, &metrics), "failed (loop)");
    }

    #[test]
    fn a_failed_session_with_no_reason_gives_its_exit_code() {
        let metrics = SessionMetrics {
            exit_code: Some(1),
            ..Default::default()
        };
        assert_eq!(outcome(false, &metrics), "failed (1)");
    }

    #[test]
    fn a_failed_session_that_says_nothing_gives_no_reason() {
        assert_eq!(outcome(false, &SessionMetrics::default()), "failed ()");
    }

    #[test]
    fn a_failed_session_with_no_abort_reason_gives_its_last_stop() {
        let metrics = SessionMetrics {
            last_stop_reason: Some(crate::session::StopReason::Error),
            exit_code: Some(1),
            ..Default::default()
        };
        assert_eq!(outcome(false, &metrics), "failed (error)");
    }

    #[test]
    fn the_record_takes_its_context_from_the_start_and_what_was_left() {
        let ctx = SessionCtx {
            task: TaskId::parse("T004").ok(),
            attempt: Some(2),
            ..Default::default()
        };
        let started = Started {
            label: "007-builder-T004".to_owned(),
            role: Role::Builder,
            ctx: &ctx,
            prompt: "build it",
            watch: WatchConfig {
                turns: 40,
                context_tokens: 32_000,
                idle: true,
                wrap_up: Role::Builder.wrap_up(),
            },
        };
        let left = Left {
            requests: Vec::new(),
            memory: None,
            reverted: vec!["README.md".to_owned()],
        };
        let settled = SessionMetrics {
            settled: true,
            exit_code: Some(0),
            ..Default::default()
        };
        let record = session_record(&started, &settled, left);
        assert_eq!(
            (record.context_budget, record.prompt_chars, record.attempt),
            (32_000, 8, Some(2))
        );
        assert_eq!(record.task, ctx.task);
        assert_eq!(record.reverted, ["README.md"]);
        assert!(record.ok);
    }
}
