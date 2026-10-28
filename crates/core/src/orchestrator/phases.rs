//! The phases: bootstrap, the task loop (build, verify, review, split,
//! block, replan) and the final review.

use std::time::Duration;

use crate::backlog::Backlog;
use crate::briefing::{self, Briefing};
use crate::ledger::{BootstrapStep, JournalRecord, Phase, Verdict};
use crate::project::BRIEF;
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::spec::{self, coverage};
use crate::task::TaskId;
use crate::verify::{VerifyOutcome, interpret, retarget};

use super::bootstrap;
use super::session::SessionCtx;
use super::{RunError, Runner};

mod planner;
mod review;
mod task;
mod task_loop;

impl Runner {
    pub(crate) async fn run_verify(&self, args: &[&str]) -> VerifyOutcome {
        let timeout = Duration::from_secs(self.params.verify_timeout_sec);
        let raw = self.deps.verify.run(args, timeout).await;
        interpret(
            args,
            &raw,
            self.params.verify_timeout_sec,
            self.params.verify_clip_bytes,
        )
    }

    /// Composes a prompt from the project's role prompt, spec and map.
    fn briefing(
        &self,
        role: Role,
        fields: Vec<(String, String)>,
        card: Option<(&str, &str)>,
        extra: Vec<(String, String)>,
        cites: &[String],
    ) -> Result<String, RunError> {
        let mut fields = fields;
        let root = self.project.root().display().to_string();
        if root.chars().any(char::is_whitespace) {
            // Models write unquoted paths; a space splits them in the shell.
            fields.push((
                "Project folder".to_owned(),
                format!(
                    "{root} (it contains spaces: quote it in shell commands, or use relative paths)"
                ),
            ));
        }
        let protocol = self.project.protocol()?;
        let mut role_prompt = self.project.role_prompt(role)?;
        if role == Role::Bootstrap {
            role_prompt =
                briefing::bootstrap_step(&role_prompt, self.ledger.bootstrap_step.number());
        }
        let spec = self
            .project
            .read_if(".tenaz/spec.md")
            .map(|spec| spec::cited(&spec, cites).unwrap_or(spec));
        let map = self.project.read_if(".tenaz/map.md");
        let b = Briefing {
            protocol: &protocol,
            role_prompt: &role_prompt,
            spec: spec.as_deref(),
            map: map.as_deref(),
            map_clip_bytes: self.params.map_clip_bytes,
            packed: self.params.briefing || role != Role::Builder,
            fields,
            card,
            extra,
        };
        Ok(retarget(
            &briefing::build(role, &b),
            self.params.verify_path(),
        ))
    }

    /// What is wrong with the files of the bootstrap up to `step`: the spec
    /// and map, then a deterministic passing `verify`, then the task cards.
    async fn validate_bootstrap(&self, step: BootstrapStep) -> (Vec<String>, Backlog) {
        let mut errors = Vec::new();
        let reqs = self.project.requirements();
        if reqs.is_empty() {
            errors.push(".tenaz/spec.md has no requirement lines like \"- R1: ...\"".to_owned());
        }
        if !self.project.exists(".tenaz/map.md") {
            errors.push(".tenaz/map.md is missing".to_owned());
        }
        let mut backlog = Backlog::default();
        if step == BootstrapStep::Tasks {
            let unfenced = self.project.unfence_cards();
            if !unfenced.is_empty() {
                self.log(&format!(
                    "Removed the code fence around task cards: {}",
                    unfenced.join(", ")
                ));
            }
            let loaded = self.project.load_backlog();
            errors.extend(loaded.errors);
            backlog = loaded.backlog;
            if backlog.is_empty() {
                errors.push("no task cards in .tenaz/tasks/".to_owned());
            }
            let cov = coverage(&reqs, backlog.cards(), false);
            if !cov.missing.is_empty() {
                errors.push(format!(
                    "requirements without a task: {}",
                    cov.missing.join(", ")
                ));
            }
        }
        if step == BootstrapStep::Spec {
            return (errors, backlog);
        }
        let first = self.run_verify(&["--all"]).await;
        if first.ok {
            let second = self.run_verify(&["--all"]).await;
            if !second.ok || second.json != first.json {
                errors.push(format!(
                    "./verify --all is not deterministic:\nrun 1: {}\nrun 2: {}",
                    first
                        .json
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    second.summary
                ));
            }
        } else {
            errors.push(format!("./verify --all does not pass:\n{}", first.summary));
        }
        (errors, backlog)
    }

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
            let attempts = bootstrap::Attempts {
                counted: self.ledger.bootstrap_attempts,
                tool_failures: self.ledger.bootstrap_tool_failures,
            };
            return self.block(&bootstrap::exhausted(
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
        let errors = bootstrap::diagnose(errors, &s.metrics, &s.record.reverted);
        self.journal(JournalRecord::BootstrapValidation {
            step,
            attempt: self.ledger.bootstrap_attempts,
            errors: errors.clone(),
            session_ok: s.ok(),
        });
        if errors.is_empty() {
            return self.bootstrap_step_done(step, &backlog);
        }
        self.bootstrap_failed(step, errors, &s.metrics)
    }

    /// The bootstrap session's prompt for `step`: the brief, what the
    /// previous attempt got wrong and left, and the files of step 1.
    fn bootstrap_prompt(&self, step: BootstrapStep) -> Result<String, RunError> {
        let mut extra = vec![(format!("Original brief ({BRIEF})"), self.project.brief()?)];
        if self.params.existing_project == Some(true) {
            extra.push((
                "Existing project".to_owned(),
                briefing::EXISTING_PROJECT.to_owned(),
            ));
        }
        extra.extend(self.bootstrap_retry_notes()?);
        if step != BootstrapStep::Spec {
            for (title, path) in [
                ("Spec", ".tenaz/spec.md"),
                ("Repository map", ".tenaz/map.md"),
            ] {
                if let Some(text) = self.project.read_if(path) {
                    extra.push((format!("{title} ({path}), from step 1"), text));
                }
            }
        }
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
        self.briefing(Role::Bootstrap, fields, None, extra, &[])
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
            bootstrap::progress(before, errors.len(), self.ledger.bootstrap_uncounted, max)
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
            bootstrap::tool_failure_note(metrics)
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

    pub(crate) async fn final_phase(&mut self) -> Result<(), RunError> {
        let tasks = self.project.load_backlog().backlog;
        let all = self.run_verify(&["--all"]).await;
        let cov = coverage(&self.project.requirements(), tasks.cards(), true);
        let report = format!(
            "Requirements: {}. Manual: {}. Not covered by a done task: {}.",
            cov.total,
            if cov.manual.is_empty() {
                "none".to_owned()
            } else {
                cov.manual.join(", ")
            },
            if cov.missing.is_empty() {
                "none".to_owned()
            } else {
                cov.missing.join(", ")
            }
        );
        let review_path = ".tenaz/reviews/FINAL.md";
        self.project.remove(review_path)?;
        let fields = vec![(
            "Cycle".to_owned(),
            (self.ledger.final_cycles + 1).to_string(),
        )];
        let extra = vec![
            ("Task list".to_owned(), tasks.table()),
            ("Coverage".to_owned(), report),
            ("./verify --all".to_owned(), all.summary.clone()),
        ];
        let prompt = self.briefing(Role::FinalReviewer, fields, None, extra, &[])?;
        self.session(
            Role::FinalReviewer,
            &prompt,
            &SessionCtx {
                turns: Some(60),
                ..Default::default()
            },
        )
        .await?;
        let (verdict, _) = self.project.verdict(review_path);
        let after = self.project.load_backlog().backlog;
        let added: Vec<TaskId> = after
            .ids()
            .filter(|id| !tasks.contains(id))
            .cloned()
            .collect();
        for id in &added {
            self.project.reset_card(id)?;
        }
        self.ledger.final_cycles += 1;
        self.deps
            .vcs
            .commit_all(&format!("tenaz: final review {}", self.ledger.final_cycles))?;
        self.journal(JournalRecord::FinalReview {
            cycle: self.ledger.final_cycles,
            verdict,
            verify_ok: all.ok,
            coverage: cov.clone(),
            added: added.clone(),
        });
        if verdict == Some(Verdict::Accept) && all.ok && cov.missing.is_empty() {
            return self.finish(Phase::Complete);
        }
        if !added.is_empty() && self.ledger.final_cycles < self.params.max_final_cycles {
            self.ledger.phase = Phase::Loop;
            self.save()?;
            self.log(&format!("Final review added {} task(s)", added.len()));
            return Ok(());
        }
        if self.ledger.final_cycles >= self.params.max_final_cycles {
            return self.finish(Phase::CompleteWithGaps);
        }
        self.log("Final review did not produce a verdict or new tasks; retrying");
        Ok(())
    }
}
