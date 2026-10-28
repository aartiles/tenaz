//! The phases: bootstrap, the task loop (build, verify, review, split,
//! block, replan) and the final review.

use std::time::Duration;

use crate::backlog::Backlog;
use crate::briefing::{self, Briefing};
use crate::ledger::{BootstrapStep, JournalRecord, Phase, TaskOutcome, Verdict};
use crate::project::BRIEF;
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::spec::{self, coverage};
use crate::task::{TaskCard, TaskId, TaskStatus};
use crate::verify::{VerifyOutcome, interpret, retarget};

use super::bootstrap;
use super::session::SessionCtx;
use super::{RunError, Runner};

mod task;

/// What a review session is given.
struct ReviewInput<'a> {
    card: &'a TaskCard,
    card_text: &'a str,
    base: &'a str,
    attempt: u32,
    all: &'a VerifyOutcome,
    own: &'a VerifyOutcome,
    /// Why the orchestrator stopped the builder, when its work still passed.
    stopped: Option<&'a str>,
}

/// The reviewer's verdict.
enum Review {
    Accept,
    Changes(String),
    NoVerdict,
}

/// A planner session about one task (split).
struct PlannerTask {
    id: TaskId,
    card_text: String,
    history: Vec<String>,
}

/// Replan sessions in a row that may fix invalid task cards before the run blocks.
const CARD_REPAIRS: u32 = 2;

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

    /// A split or replan session; the cards it leaves are validated and
    /// either committed or thrown away.
    async fn planner_session(
        &mut self,
        role: Role,
        task: Option<&PlannerTask>,
        reason: &str,
    ) -> Result<bool, RunError> {
        let before = self
            .deps
            .vcs
            .commit_all(&format!("tenaz: bookkeeping before {role}"))?;
        let tasks = self.project.load_backlog().backlog;
        let mut extra = vec![
            ("Task list".to_owned(), tasks.table()),
            ("Why this session".to_owned(), reason.to_owned()),
        ];
        if let (Role::Split, Some(task)) = (role, task) {
            extra.push(("Failure history".to_owned(), task.history.join("\n\n")));
        }
        let journal = self.project.journal();
        let recent: Vec<String> = journal
            .iter()
            .rev()
            .take(12)
            .map(super::super::ledger::JournalEntry::summary)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        extra.push(("Recent orchestrator journal".to_owned(), recent.join("\n")));
        if let Some(planner) = self.project.note("PLANNER") {
            extra.push(("Previous planner notes".to_owned(), planner));
        }
        let fields = task
            .map(|t| vec![("Task".to_owned(), t.id.to_string())])
            .unwrap_or_default();
        let card = task.map(|t| (t.id.card_path(), t.card_text.clone()));
        let prompt = self.briefing(
            role,
            fields,
            card.as_ref().map(|(p, t)| (p.as_str(), t.as_str())),
            extra,
            &[],
        )?;
        self.session(
            role,
            &prompt,
            &SessionCtx {
                task: task.map(|t| t.id.clone()),
                ..Default::default()
            },
        )
        .await?;
        let loaded = self.project.load_backlog();
        let after = loaded.backlog;
        let mut errors = loaded.errors;
        let cov = coverage(&self.project.requirements(), after.cards(), false);
        if !cov.missing.is_empty() {
            errors.push(format!(
                "requirements left uncovered: {}",
                cov.missing.join(", ")
            ));
        }
        for t in after.cards() {
            match tasks.get(&t.id) {
                None if t.status != TaskStatus::Todo => self.project.reset_card(&t.id)?,
                Some(old)
                    if old.status != t.status
                        && !(t.status == TaskStatus::Dropped
                            && matches!(old.status, TaskStatus::Todo | TaskStatus::Blocked)) =>
                {
                    self.project.set_status(&t.id, old.status)?;
                }
                _ => {}
            }
        }
        for id in tasks.ids() {
            if !after.contains(id) {
                errors.push(format!("card {id} was deleted"));
            }
        }
        let task_id = task.map(|t| t.id.clone());
        if !errors.is_empty() {
            self.deps.vcs.restore_paths(&before, &[".tenaz/tasks"])?;
            self.journal(match role {
                Role::Split => JournalRecord::SplitRejected {
                    task: task_id,
                    errors: errors.clone(),
                },
                _ => JournalRecord::ReplanRejected {
                    task: task_id,
                    errors: errors.clone(),
                },
            });
            self.log(&format!("{role} rejected: {}", errors.join("; ")));
            return Ok(false);
        }
        let suffix = task.map(|t| format!(" {}", t.id)).unwrap_or_default();
        self.deps
            .vcs
            .commit_all(&format!("tenaz: {role}{suffix}"))?;
        let added: Vec<TaskId> = after
            .ids()
            .filter(|id| !tasks.contains(id))
            .cloned()
            .collect();
        self.journal(match role {
            Role::Split => JournalRecord::SplitAccepted {
                task: task_id,
                added,
            },
            _ => JournalRecord::ReplanAccepted {
                task: task_id,
                added,
            },
        });
        Ok(true)
    }

    /// A task out of attempts, or too big for its context budget: split it, or block it.
    async fn escalate(
        &mut self,
        task: &TaskCard,
        history: Vec<String>,
        why: &str,
    ) -> Result<(), RunError> {
        let base = self
            .ledger
            .current
            .as_ref()
            .map(|c| c.base.clone())
            .unwrap_or_default();
        self.deps.vcs.reset_hard(&base)?;
        if task.id.split_depth() < self.params.max_split_depth {
            let planner = PlannerTask {
                id: task.id.clone(),
                card_text: self.project.card_text(&task.id)?,
                history: history.clone(),
            };
            let ok = self
                .planner_session(Role::Split, Some(&planner), why)
                .await?;
            let children: Vec<TaskId> = self
                .project
                .load_backlog()
                .backlog
                .children(&task.id)
                .iter()
                .map(|c| c.id.clone())
                .collect();
            if ok && children.len() >= 2 {
                self.project.set_status(&task.id, TaskStatus::Split)?;
                let names = children
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                self.ledger.checkpoint = Some(
                    self.deps
                        .vcs
                        .commit_all(&format!("tenaz: {} split into {names}", task.id))?,
                );
                self.journal(JournalRecord::Task {
                    task: task.id.clone(),
                    outcome: TaskOutcome::Split,
                    children: Some(children),
                    history: None,
                    attempts: None,
                    reviewed: None,
                    stopped_sessions: None,
                });
                return Ok(());
            }
            if ok {
                self.deps.vcs.reset_hard(&base)?;
            }
        }
        self.project.set_status(&task.id, TaskStatus::Blocked)?;
        self.ledger.checkpoint = Some(
            self.deps
                .vcs
                .commit_all(&format!("tenaz: {} blocked", task.id))?,
        );
        self.journal(JournalRecord::Task {
            task: task.id.clone(),
            outcome: TaskOutcome::Blocked,
            children: None,
            history: Some(history),
            attempts: None,
            reviewed: None,
            stopped_sessions: None,
        });
        self.log(&format!("{} blocked", task.id));
        Ok(())
    }

    /// Fresh-context review of a task's diff. A reviewer that ends without
    /// a verdict (for example, stuck repeating itself in its reasoning) gets
    /// one fresh retry before `verify` alone decides.
    async fn review_task(&mut self, input: &ReviewInput<'_>) -> Result<Review, RunError> {
        let task = &input.card.id;
        let mut diff = self.deps.vcs.diff(input.base, "HEAD", true)?;
        let images = briefing::changed_images(&diff);
        if diff.len() > self.params.diff_limit_bytes {
            let stat = self.deps.vcs.diff_stat(input.base, Some("HEAD"), false)?;
            let cut = diff.floor_char_boundary(self.params.diff_limit_bytes);
            diff = format!(
                "{}\n\n[Diff of {} bytes clipped. Read the changed files directly.]\n\n{}",
                stat.trim(),
                diff.len(),
                &diff[..cut]
            );
        }
        let review_path = format!(".tenaz/reviews/{task}.md");
        self.project.remove(&review_path)?;
        let mut verdict: Option<Verdict> = None;
        let mut text: Option<String> = None;
        for review in 1..=2 {
            if verdict.is_some() {
                break;
            }
            if review > 1 {
                self.log(&format!(
                    "  reviewer left no verdict for {task}; retrying the review"
                ));
            }
            let mut extra = Vec::new();
            if let Some(stopped) = input.stopped {
                extra.push(("Builder session".to_owned(), format!("The orchestrator stopped the builder session ({stopped}). The checks pass, but look for unfinished work.")));
            }
            extra.push((
                "Builder notes".to_owned(),
                self.project.note(task.as_str()).unwrap_or_default(),
            ));
            if !images.is_empty() {
                extra.push((
                    "Images this change adds or replaces".to_owned(),
                    format!(
                        "{}\n\nLook at each one before you judge the task (see the role prompt).",
                        images
                            .iter()
                            .map(|path| format!("- {path}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                ));
            }
            extra.push((
                "Diff since the task started".to_owned(),
                format!("```diff\n{diff}\n```"),
            ));
            extra.push((
                "Verify results".to_owned(),
                format!("{}\n\n{}", input.all.summary, input.own.summary),
            ));
            let fields = vec![
                ("Task".to_owned(), task.to_string()),
                ("Review file".to_owned(), review_path.clone()),
            ];
            let prompt = self.briefing(
                Role::Reviewer,
                fields,
                Some((&input.card.file, input.card_text)),
                extra,
                &[],
            )?;
            let s = self
                .session(
                    Role::Reviewer,
                    &prompt,
                    &SessionCtx {
                        task: Some(task.clone()),
                        attempt: Some(input.attempt),
                        turns: Some(25),
                        context_tokens: None,
                    },
                )
                .await?;
            let (mut v, mut t) = self.project.verdict(&review_path);
            // A reviewer that gave its verdict in its last message but did
            // not write it (or wrote it to the wrong path) is taken at its word.
            if v.is_none()
                && let Some(said) = s
                    .metrics
                    .texts
                    .iter()
                    .rev()
                    .find(|text| crate::project::verdict_in(text).is_some())
            {
                self.project.write(&review_path, said)?;
                self.log(&format!(
                    "  {task}: the reviewer wrote no review file; its verdict is taken from its last message"
                ));
                v = crate::project::verdict_in(said);
                t = Some(said.clone());
            }
            verdict = v;
            text = t;
            self.journal(JournalRecord::Review {
                task: task.clone(),
                attempt: input.attempt,
                review,
                verdict,
            });
        }
        Ok(match verdict {
            Some(Verdict::Accept) => Review::Accept,
            Some(Verdict::Changes)
                if text
                    .as_deref()
                    .is_some_and(crate::project::only_minor_findings) =>
            {
                self.log(&format!(
                    "  {task}: the review asks for changes with MINOR findings only; accepted"
                ));
                Review::Accept
            }
            Some(Verdict::Changes) => Review::Changes(text.unwrap_or_default()),
            None => Review::NoVerdict,
        })
    }

    pub(crate) async fn loop_phase(&mut self) -> Result<(), RunError> {
        let loaded = self.project.load_backlog();
        if !loaded.errors.is_empty() {
            // A planner wrote cards the orchestrator cannot read: a replan
            // session gets the problems to fix before the run blocks.
            if self.ledger.card_repairs < CARD_REPAIRS {
                self.ledger.card_repairs += 1;
                self.save()?;
                self.log(&format!(
                    "Task cards are invalid ({}); a replan session fixes them ({} of {CARD_REPAIRS})",
                    loaded.errors.join("; "),
                    self.ledger.card_repairs
                ));
                self.planner_session(
                    Role::Replan,
                    None,
                    &format!(
                        "The orchestrator cannot read these task cards. Fix them (cards that are not done only), change nothing else:\n{}",
                        loaded
                            .errors
                            .iter()
                            .map(|e| format!("- {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                )
                .await?;
                return Ok(());
            }
            return self.block(&format!(
                "task cards are invalid: {}",
                loaded.errors.join("; ")
            ));
        }
        if self.ledger.card_repairs > 0 {
            self.ledger.card_repairs = 0;
            self.save()?;
        }
        let backlog = loaded.backlog;
        if self.params.replan_every > 0
            && self.ledger.done_since_replan >= self.params.replan_every
            && self.ledger.current.is_none()
        {
            self.ledger.done_since_replan = 0;
            self.save()?;
            self.planner_session(
                Role::Replan,
                None,
                &format!(
                    "Periodic replan after {} completed tasks.",
                    self.params.replan_every
                ),
            )
            .await?;
            return Ok(());
        }
        if let Some(task) = backlog.ready().first().copied().cloned() {
            return self.run_task(&backlog, &task).await;
        }
        let open: Vec<String> = backlog
            .open()
            .iter()
            .map(|t| format!("{} [{}]", t.id, t.status))
            .collect();
        if open.is_empty() {
            self.ledger.phase = Phase::Final;
            return self.save();
        }
        if !self.ledger.stuck_replanned {
            self.ledger.stuck_replanned = true;
            self.save()?;
            self.planner_session(Role::Replan, None, &format!("No task is ready. Blocked or unreachable: {}. Route the work around the blocked tasks.", open.join(", "))).await?;
            return Ok(());
        }
        self.block(&format!(
            "no task is ready; open tasks: {}",
            open.join(", ")
        ))
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
