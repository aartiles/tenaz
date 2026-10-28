//! The bootstrap phase: one session per step until the step's files are
//! right (ADR 0003). What a failed attempt means is decided in
//! `orchestrator::bootstrap` (`policy` here).

use crate::backlog::Backlog;
use crate::briefing;
use crate::ledger::{BootstrapStep, JournalRecord, Phase};
use crate::orchestrator::bootstrap as policy;
use crate::orchestrator::session::SessionCtx;
use crate::orchestrator::{RunError, Runner};
use crate::project::BRIEF;
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::spec::{Requirement, coverage};

use super::PromptParts;

impl Runner {
    /// What is wrong with the files of the bootstrap up to `step`: the spec
    /// and map, then the task cards (Tasks step), then a deterministic
    /// passing `./verify --all` (every step but Spec).
    async fn validate_bootstrap(&self, step: BootstrapStep) -> (Vec<String>, Backlog) {
        let reqs = self.project.requirements();
        let mut errors = Vec::new();
        if reqs.is_empty() {
            errors.push(".tenaz/spec.md has no requirement lines like \"- R1: ...\"".to_owned());
        }
        if !self.project.exists(".tenaz/map.md") {
            errors.push(".tenaz/map.md is missing".to_owned());
        }
        let backlog = if step == BootstrapStep::Tasks {
            let (backlog, card_errors) = self.load_cards(&reqs);
            errors.extend(card_errors);
            backlog
        } else {
            Backlog::default()
        };
        if step != BootstrapStep::Spec {
            errors.extend(self.verify_all_error().await);
        }
        (errors, backlog)
    }

    /// The backlog the task cards the bootstrap wrote make, and what is
    /// wrong with them.
    fn load_cards(&self, reqs: &[Requirement]) -> (Backlog, Vec<String>) {
        let unfenced = self.project.unfence_cards();
        if !unfenced.is_empty() {
            self.log(&format!(
                "Removed the code fence around task cards: {}",
                unfenced.join(", ")
            ));
        }
        let loaded = self.project.load_backlog();
        let mut errors = loaded.errors;
        let backlog = loaded.backlog;
        if backlog.is_empty() {
            errors.push("no task cards in .tenaz/tasks/".to_owned());
        }
        let cov = coverage(reqs, backlog.cards(), false);
        if !cov.missing.is_empty() {
            errors.push(format!(
                "requirements without a task: {}",
                cov.missing.join(", ")
            ));
        }
        (backlog, errors)
    }

    /// Why `./verify --all` is not a deterministic pass, unless it is one.
    async fn verify_all_error(&self) -> Option<String> {
        let first = self.run_verify(&["--all"]).await;
        if !first.ok {
            return Some(format!("./verify --all does not pass:\n{}", first.summary));
        }
        let second = self.run_verify(&["--all"]).await;
        if second.ok && second.json == first.json {
            return None;
        }
        Some(format!(
            "./verify --all is not deterministic:\nrun 1: {}\nrun 2: {}",
            first
                .json
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            second.summary
        ))
    }

    /// One step of the bootstrap: a session for it unless its files are
    /// already right, then the step accepted, retried, or the run blocked.
    pub(crate) async fn bootstrap_phase(&mut self) -> Result<(), RunError> {
        let step = self.ledger.bootstrap_step;
        // A step whose files are already right (written by an earlier step's
        // session, or before a resume) needs no session of its own.
        if self.ledger.bootstrap_attempts == 0 && self.ledger.bootstrap_uncounted == 0 {
            let (errors, backlog) = self.validate_bootstrap(step).await;
            if errors.is_empty() {
                self.log(&format!("Bootstrap step {step} is already done"));
                return self.bootstrap_step_done(step, &backlog);
            }
        }
        if self.ledger.bootstrap_attempts >= self.params.max_bootstrap_attempts {
            let attempts = policy::Attempts {
                counted: self.ledger.bootstrap_attempts,
                tool_failures: self.ledger.bootstrap_tool_failures,
            };
            return self.block(&policy::exhausted(
                step,
                attempts,
                self.ledger.config.harness,
                self.ledger.last_errors.as_deref(),
            ));
        }
        self.ledger.bootstrap_attempts += 1;
        let prompt = self.bootstrap_prompt(step)?;
        let s = self
            .session(Role::Bootstrap, &prompt, &SessionCtx::default())
            .await?;
        let (errors, backlog) = self.validate_bootstrap(step).await;
        let errors = policy::diagnose(errors, &s.metrics, &s.record.reverted);
        self.journal(JournalRecord::BootstrapValidation {
            step,
            attempt: self.ledger.bootstrap_attempts,
            errors: errors.clone(),
            session_ok: s.metrics.ok(),
        });
        if errors.is_empty() {
            return self.bootstrap_step_done(step, &backlog);
        }
        self.bootstrap_failed(step, errors, &s.metrics)
    }

    /// The bootstrap session's prompt for `step`: the brief, what the
    /// previous attempt got wrong and left, and what its step builds on.
    fn bootstrap_prompt(&self, step: BootstrapStep) -> Result<String, RunError> {
        let mut extra = vec![(format!("Original brief ({BRIEF})"), self.project.brief()?)];
        if self.params.existing_project == Some(true) {
            extra.push((
                "Existing project".to_owned(),
                briefing::EXISTING_PROJECT.to_owned(),
            ));
        }
        extra.extend(self.bootstrap_retry_notes()?);
        extra.extend(self.bootstrap_step_sections(step));
        let fields = vec![
            ("Step".to_owned(), format!("{step}; do only this step")),
            (
                "Attempt".to_owned(),
                format!(
                    "{} of {}",
                    self.ledger.bootstrap_attempts, self.params.max_bootstrap_attempts
                ),
            ),
        ];
        self.briefing(
            Role::Bootstrap,
            PromptParts {
                fields,
                extra,
                ..PromptParts::default()
            },
        )
    }

    /// What `step` builds on: the files of step 1 after it, and for the
    /// task cards the builders' context budget they are sized to.
    fn bootstrap_step_sections(&self, step: BootstrapStep) -> Vec<(String, String)> {
        let mut sections = Vec::new();
        if step != BootstrapStep::Spec {
            for (title, path) in [
                ("Spec", ".tenaz/spec.md"),
                ("Repository map", ".tenaz/map.md"),
            ] {
                if let Some(text) = self.project.read_if(path) {
                    sections.push((format!("{title} ({path}), from step 1"), text));
                }
            }
        }
        if step == BootstrapStep::Tasks {
            sections.push(self.card_sizing());
        }
        sections
    }

    /// What a retried bootstrap session is told about the attempt before
    /// it: the problems found, and the files it left.
    fn bootstrap_retry_notes(&self) -> Result<Vec<(String, String)>, RunError> {
        let Some(errors) = self.ledger.last_errors.as_ref().filter(|e| !e.is_empty()) else {
            return Ok(Vec::new());
        };
        let mut notes = vec![(
            "Problems the orchestrator found in the previous bootstrap session".to_owned(),
            errors
                .iter()
                .map(|e| format!("- {e}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )];
        if let Some(left) = self.bootstrap_left()? {
            notes.push((
                "Files the previous bootstrap sessions left (kept)".to_owned(),
                left,
            ));
        }
        Ok(notes)
    }

    /// Records an attempt at `step` that left `errors`. An attempt that left
    /// fewer problems than the one before is progress and is not counted.
    fn bootstrap_failed(
        &mut self,
        step: BootstrapStep,
        errors: Vec<String>,
        metrics: &SessionMetrics,
    ) -> Result<(), RunError> {
        if metrics.every_tool_call_failed() {
            self.ledger.bootstrap_tool_failures += 1;
        }
        let max = self.params.max_bootstrap_attempts;
        let before = self.ledger.last_errors.as_ref().map(Vec::len);
        if let Some(before) =
            policy::progress(before, errors.len(), self.ledger.bootstrap_uncounted, max)
        {
            self.ledger.bootstrap_attempts -= 1;
            self.ledger.bootstrap_uncounted += 1;
            self.log(&format!(
                "Bootstrap made progress ({before} -> {} problems); this attempt is not counted ({} of {max} such)",
                errors.len(),
                self.ledger.bootstrap_uncounted,
            ));
        }
        self.log(&format!(
            "Bootstrap step {step} not accepted: {} problem(s){}",
            errors.len(),
            policy::tool_failure_note(metrics)
        ));
        self.ledger.last_errors = Some(errors);
        self.save()
    }

    /// Moves the bootstrap to the step after `step`, whose attempts count
    /// from zero, or accepts it after the last one.
    fn bootstrap_step_done(
        &mut self,
        step: BootstrapStep,
        backlog: &Backlog,
    ) -> Result<(), RunError> {
        self.ledger.last_errors = None;
        self.ledger.bootstrap_attempts = 0;
        self.ledger.bootstrap_tool_failures = 0;
        self.ledger.bootstrap_uncounted = 0;
        if let Some(next) = step.next() {
            self.ledger.bootstrap_step = next;
            self.deps
                .vcs
                .commit_all(&format!("tenaz: bootstrap step {} done", step.number()))?;
            self.save()?;
            self.log(&format!("Bootstrap step {step} done; next: step {next}"));
            return Ok(());
        }
        for id in backlog.ids() {
            self.project.reset_card(id)?;
        }
        self.ledger.phase = Phase::Loop;
        self.ledger.checkpoint = Some(self.deps.vcs.commit_all("tenaz: bootstrap accepted")?);
        self.save()?;
        self.log(&format!(
            "Bootstrap accepted: {} tasks, {} requirements",
            backlog.len(),
            self.project.requirements().len()
        ));
        Ok(())
    }

    /// What earlier bootstrap sessions wrote since `init`, for the next one
    /// to continue from instead of starting over. Commits it first, so new
    /// files show too.
    fn bootstrap_left(&self) -> Result<Option<String>, RunError> {
        let Some(base) = self.ledger.checkpoint.clone() else {
            return Ok(None);
        };
        self.deps
            .vcs
            .commit_all(&format!("tenaz: bookkeeping before {}", Role::Bootstrap))?;
        let stat = self.deps.vcs.diff_stat(&base, Some("HEAD"), false)?;
        if stat.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "{}\n\nThese are already in the project. Read them and continue from them: fix the problems above and write what is still missing. Do not start over or rewrite what is already right.",
            stat.trim()
        )))
    }
}
