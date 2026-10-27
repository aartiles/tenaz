//! The phases: bootstrap, the task loop (build, verify, review, split,
//! block, replan) and the final review.

use std::time::Duration;

use crate::backlog::Backlog;
use crate::briefing::{self, Briefing};
use crate::ledger::{
    BootstrapStep, CurrentTask, JournalRecord, Phase, StoppedAttempt, TaskOutcome, Verdict,
};
use crate::project::BRIEF;
use crate::role::Role;
use crate::spec::{self, coverage};
use crate::task::{TaskCard, TaskId, TaskStatus};
use crate::util::clip;
use crate::verify::{VerifyOutcome, interpret, retarget};

use super::session::SessionCtx;
use super::{RunError, Runner};

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

/// A builder attempt that passed `verify`.
struct Built {
    card: TaskCard,
    card_text: String,
    all: VerifyOutcome,
    own: VerifyOutcome,
    /// Why the orchestrator stopped the builder, when its work still passed.
    stopped: Option<String>,
}

/// How a builder attempt ended.
enum Attempt {
    Passed(Box<Built>),
    /// `outgrown`: its session used up the context continuations.
    Failed {
        failure: String,
        outgrown: bool,
    },
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
            let errors = self
                .ledger
                .last_errors
                .clone()
                .unwrap_or_default()
                .join("; ");
            let hint = if self.ledger.bootstrap_tool_failures >= self.ledger.bootstrap_attempts {
                format!(
                    " (in every attempt all of the model's tool calls failed: the model does not \
                     work with {}'s tools; try another model or the other harness)",
                    self.ledger.config.harness
                )
            } else {
                String::new()
            };
            return self.block(&format!(
                "bootstrap failed {} times at step {step}{hint}: {errors}",
                self.ledger.bootstrap_attempts
            ));
        }
        self.ledger.bootstrap_attempts += 1;
        let mut extra = vec![(format!("Original brief ({BRIEF})"), self.project.brief()?)];
        if self.params.existing_project == Some(true) {
            extra.push((
                "Existing project".to_owned(),
                briefing::EXISTING_PROJECT.to_owned(),
            ));
        }
        if let Some(errors) = &self.ledger.last_errors
            && !errors.is_empty()
        {
            extra.push((
                "Problems the orchestrator found in the previous bootstrap session".to_owned(),
                errors
                    .iter()
                    .map(|e| format!("- {e}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
            if let Some(left) = self.bootstrap_left()? {
                extra.push((
                    "Files the previous bootstrap sessions left (kept)".to_owned(),
                    left,
                ));
            }
        }
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
        let prompt = self.briefing(Role::Bootstrap, fields, None, extra, &[])?;
        let s = self
            .session(Role::Bootstrap, &prompt, &SessionCtx::default())
            .await?;
        let (mut errors, backlog) = self.validate_bootstrap(step).await;
        // A file the session wrote where it may not was reverted after it. When the
        // bootstrap fails, say so: the next session otherwise sees only the failure
        // the revert caused (verify calling a file that is gone) and repeats it.
        if !errors.is_empty() && !s.record.reverted.is_empty() {
            errors.insert(
                0,
                crate::ownership::reverted_note(Role::Bootstrap, &s.record.reverted),
            );
        }
        // A reply cut at the output limit left no work: say why, so the next
        // session does not reason the same way again.
        if !errors.is_empty()
            && s.metrics.output_truncated()
            && let Some(reason) = &s.record.abort_reason
        {
            errors.insert(0, format!("the session was stopped: {reason}"));
        }
        // A session whose every tool call failed did not get to the task: say
        // so first, for the next session and for whoever reads the block.
        if !errors.is_empty() && s.metrics.every_tool_call_failed() {
            errors.insert(
                0,
                format!(
                    "every tool call in the session failed ({} of {}), so it could not read or \
                     write the project; the first: {}",
                    s.metrics.tool_errors,
                    s.metrics.tool_calls,
                    s.metrics.first_tool_error.as_deref().unwrap_or("unknown")
                ),
            );
        }
        self.journal(JournalRecord::BootstrapValidation {
            step,
            attempt: self.ledger.bootstrap_attempts,
            errors: errors.clone(),
            session_ok: s.ok(),
        });
        if !errors.is_empty() {
            if s.metrics.every_tool_call_failed() {
                self.ledger.bootstrap_tool_failures += 1;
            }
            // An attempt that left fewer problems than the one before is
            // progress: it is not counted, up to as many again as allowed.
            if let Some(before) = self.ledger.last_errors.as_ref().map(Vec::len)
                && errors.len() < before
                && self.ledger.bootstrap_uncounted < self.params.max_bootstrap_attempts
            {
                self.ledger.bootstrap_attempts -= 1;
                self.ledger.bootstrap_uncounted += 1;
                self.log(&format!(
                    "Bootstrap made progress ({before} -> {} problems); this attempt is not counted ({} of {} such)",
                    errors.len(),
                    self.ledger.bootstrap_uncounted,
                    self.params.max_bootstrap_attempts
                ));
            }
            self.log(&format!(
                "Bootstrap step {step} not accepted: {} problem(s){}",
                errors.len(),
                if s.metrics.every_tool_call_failed() {
                    format!(
                        "; all {} tool calls failed, the first: {}",
                        s.metrics.tool_calls,
                        s.metrics.first_tool_error.as_deref().unwrap_or("unknown")
                    )
                } else {
                    String::new()
                }
            ));
            self.ledger.last_errors = Some(errors);
            return self.save();
        }
        self.bootstrap_step_done(step, &backlog)
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
            self.project.set_card_fields(
                id,
                &[("status", "todo".to_owned()), ("attempts", "0".to_owned())],
            )?;
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
                None if t.status != TaskStatus::Todo => self.project.set_card_fields(
                    &t.id,
                    &[("status", "todo".to_owned()), ("attempts", "0".to_owned())],
                )?,
                Some(old)
                    if old.status != t.status
                        && !(t.status == TaskStatus::Dropped
                            && matches!(old.status, TaskStatus::Todo | TaskStatus::Blocked)) =>
                {
                    self.project
                        .set_card_fields(&t.id, &[("status", old.status.to_string())])?;
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
                self.project
                    .set_card_fields(&task.id, &[("status", "split".to_owned())])?;
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
        self.project
            .set_card_fields(&task.id, &[("status", "blocked".to_owned())])?;
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

    /// Builds one task: attempts, verify, review, done; or escalate.
    async fn run_task(&mut self, backlog: &Backlog, task: &TaskCard) -> Result<(), RunError> {
        if self
            .ledger
            .current
            .as_ref()
            .is_none_or(|c| c.task != task.id)
        {
            self.project
                .set_card_fields(&task.id, &[("status", "active".to_owned())])?;
            let base = self
                .deps
                .vcs
                .commit_all(&format!("tenaz: start {}", task.id))?;
            self.ledger.current = Some(CurrentTask {
                task: task.id.clone(),
                base,
                history: Vec::new(),
                failure: None,
                stopped: Vec::new(),
            });
            self.save()?;
        }
        loop {
            let attempts = self
                .project
                .load_backlog()
                .backlog
                .get(&task.id)
                .map_or(0, |t| t.attempts);
            if attempts >= self.params.max_attempts {
                let history = self
                    .ledger
                    .current
                    .as_ref()
                    .map(|c| c.history.clone())
                    .unwrap_or_default();
                let why = format!("{} failed {} attempts.", task.id, self.params.max_attempts);
                self.escalate(task, history, &why).await?;
                self.ledger.current = None;
                return self.save();
            }
            let attempt = attempts + 1;
            match self.build_attempt(backlog, task, attempt).await? {
                Attempt::Failed { failure, outgrown } => {
                    if let Some(current) = &mut self.ledger.current {
                        current
                            .history
                            .push(format!("Attempt {attempt}: {}", clip(&failure, 1500)));
                        current.failure = Some(failure);
                    }
                    self.save()?;
                    // Another attempt would meet the same budget: the task is too big for it.
                    if outgrown
                        && self.params.context_split
                        && task.id.split_depth() < self.params.max_split_depth
                    {
                        let why = format!(
                            "{} outgrew its context budget: attempt {attempt} used all {} context continuations and still fails verify.",
                            task.id, self.params.context_continuations
                        );
                        self.log(&format!("  {why} Splitting it instead of retrying."));
                        let history = self
                            .ledger
                            .current
                            .as_ref()
                            .map(|c| c.history.clone())
                            .unwrap_or_default();
                        self.escalate(task, history, &why).await?;
                        self.ledger.current = None;
                        return self.save();
                    }
                }
                Attempt::Passed(built) => {
                    self.deps.vcs.commit_all(&format!(
                        "task {}: {} (attempt {attempt})",
                        task.id, built.card.title
                    ))?;
                    let mut reviewed = false;
                    if self.params.review {
                        let base = self
                            .ledger
                            .current
                            .as_ref()
                            .map(|c| c.base.clone())
                            .unwrap_or_default();
                        let review = ReviewInput {
                            card: &built.card,
                            card_text: &built.card_text,
                            base: &base,
                            attempt,
                            all: &built.all,
                            own: &built.own,
                            stopped: built.stopped.as_deref(),
                        };
                        match self.review_task(&review).await? {
                            Review::Changes(text) => {
                                if let Some(current) = &mut self.ledger.current {
                                    current.failure = Some(format!(
                                        "The reviewer requested changes:\n{}",
                                        clip(&text, 3000)
                                    ));
                                    current.history.push(format!(
                                        "Attempt {attempt}: review requested changes:\n{}",
                                        clip(&text, 1500)
                                    ));
                                }
                                self.deps.vcs.commit_all(&format!(
                                    "tenaz: review {} attempt {attempt}",
                                    task.id
                                ))?;
                                self.save()?;
                                continue;
                            }
                            Review::NoVerdict => self.log(&format!(
                                "  reviewer left no verdict for {}; accepting on verify alone",
                                task.id
                            )),
                            Review::Accept => reviewed = true,
                        }
                    }
                    // The floor only goes up.
                    self.ledger.passed_checks = Some(
                        built
                            .all
                            .passed()
                            .max(self.ledger.passed_checks.unwrap_or(0)),
                    );
                    return self.complete_task(task, attempt, reviewed);
                }
            }
        }
    }

    /// One builder session and its judgement by `verify`. A paused attempt,
    /// or one cut short by the engine, is given back: it is not a failed one.
    async fn build_attempt(
        &mut self,
        backlog: &Backlog,
        task: &TaskCard,
        attempt: u32,
    ) -> Result<Attempt, RunError> {
        self.project
            .set_card_fields(&task.id, &[("attempts", attempt.to_string())])?;
        let card = self
            .project
            .load_backlog()
            .backlog
            .get(&task.id)
            .cloned()
            .ok_or_else(|| RunError::Precondition(format!("card {} disappeared", task.id)))?;
        let card_text = self.project.card_text(&task.id)?;
        let prompt = self
            .builder_prompt(backlog, &card, &card_text, attempt)
            .await?;
        let ctx = SessionCtx {
            task: Some(task.id.clone()),
            attempt: Some(attempt),
            turns: card.budget.turns,
            context_tokens: card.budget.tokens,
        };
        let s = match self.session(Role::Builder, &prompt, &ctx).await {
            Ok(s) => s,
            Err(error) => {
                if matches!(error, RunError::Paused(_) | RunError::EngineUnavailable(_)) {
                    self.project
                        .set_card_fields(&task.id, &[("attempts", (attempt - 1).to_string())])?;
                }
                return Err(error);
            }
        };
        let all = self.run_verify(&["--all"]).await;
        let own = self.run_verify(&["--task", task.id.as_str()]).await;
        let own_checks = own.passed();
        let mut failure = if !all.ok {
            Some(format!(
                "./verify --all fails after the attempt:\n{}",
                all.summary
            ))
        } else if let Some(why) = crate::verify::fewer_checks(
            task.id.as_str(),
            all.passed(),
            own_checks,
            self.ledger.passed_checks,
        ) {
            Some(why)
        } else if !own.ok || own_checks < 1 {
            Some(format!(
                "./verify --task {} must run at least one passing check with ID prefix \"{}.\" (got {own_checks}).\n{}",
                task.id, task.id, own.summary
            ))
        } else {
            None
        };
        let stopped = (!s.ok()).then(|| {
            s.record
                .abort_reason
                .clone()
                .or_else(|| {
                    s.record
                        .last_stop_reason
                        .as_ref()
                        .map(|r| r.as_str().to_owned())
                })
                .unwrap_or_default()
        });
        if let (Some(stopped), Some(f)) = (&stopped, &failure) {
            failure = Some(format!("The session was stopped: {stopped}.\n{f}"));
        }
        if let (Some(stopped), None) = (&stopped, &failure)
            && let Some(current) = &mut self.ledger.current
        {
            // A stopped session whose work still passes goes on to review, but is recorded as such.
            current.stopped.push(StoppedAttempt {
                attempt,
                reason: stopped.clone(),
            });
        }
        self.journal(JournalRecord::Verify {
            task: task.id.clone(),
            attempt,
            all_ok: all.ok,
            task_ok: own.ok,
            task_checks: own_checks,
            all: all.json.clone(),
            task_json: own.json.clone(),
        });
        Ok(match failure {
            Some(failure) => Attempt::Failed {
                failure,
                outgrown: s.metrics.context_stopped(),
            },
            None => Attempt::Passed(Box::new(Built {
                card,
                card_text,
                all,
                own,
                stopped,
            })),
        })
    }

    /// Marks a task done, commits the checkpoint and journals it.
    fn complete_task(
        &mut self,
        task: &TaskCard,
        attempt: u32,
        reviewed: bool,
    ) -> Result<(), RunError> {
        self.project
            .set_card_fields(&task.id, &[("status", "done".to_owned())])?;
        self.ledger.checkpoint = Some(
            self.deps
                .vcs
                .commit_all(&format!("tenaz: {} done", task.id))?,
        );
        let stopped_sessions = self
            .ledger
            .current
            .as_ref()
            .map(|c| c.stopped.clone())
            .unwrap_or_default();
        self.journal(JournalRecord::Task {
            task: task.id.clone(),
            outcome: TaskOutcome::Done,
            children: None,
            history: None,
            attempts: Some(attempt),
            reviewed: Some(reviewed),
            stopped_sessions: Some(stopped_sessions.clone()),
        });
        self.ledger.current = None;
        self.ledger.done_since_replan += 1;
        self.ledger.stuck_replanned = false;
        self.save()?;
        let plural = |n: usize, word: &str| {
            if n == 1 {
                word.to_owned()
            } else {
                format!("{word}s")
            }
        };
        let stopped_note = if stopped_sessions.is_empty() {
            String::new()
        } else {
            format!(
                "; {} stopped builder {} passed verify",
                stopped_sessions.len(),
                plural(stopped_sessions.len(), "session")
            )
        };
        self.log(&format!(
            "{} done ({attempt} {}{stopped_note})",
            task.id,
            plural(attempt as usize, "attempt")
        ));
        Ok(())
    }

    /// The builder's briefing: notes from the dependencies and from the task
    /// itself, the previous failure and what it left, and the latest
    /// `verify` result.
    async fn builder_prompt(
        &self,
        backlog: &Backlog,
        card: &TaskCard,
        card_text: &str,
        attempt: u32,
    ) -> Result<String, RunError> {
        let mut extra = Vec::new();
        let mut note_ids: Vec<String> = vec!["BOOTSTRAP".to_owned()];
        for dep in &card.depends {
            note_ids.push(dep.to_string());
            note_ids.extend(
                backlog
                    .children(dep)
                    .iter()
                    .map(|child| child.id.to_string()),
            );
        }
        let notes = briefing::dependency_notes(
            &note_ids,
            |id| self.project.note(id),
            self.params.note_clip_bytes,
        );
        if !notes.is_empty() && self.params.briefing {
            extra.push(("Notes from earlier tasks".to_owned(), notes));
        }
        if let Some(own) = self.project.note(card.id.as_str()) {
            extra.push((
                "Your notes from earlier attempts".to_owned(),
                crate::util::clip(own.trim(), self.params.note_clip_bytes),
            ));
        }
        let (base, failure) = self
            .ledger
            .current
            .as_ref()
            .map(|c| (c.base.clone(), c.failure.clone()))
            .unwrap_or_default();
        if let Some(failure) = failure {
            extra.push(("Previous attempt failed".to_owned(), failure));
            let diff = self.deps.vcs.diff_stat(&base, None, true)?;
            if !diff.trim().is_empty() {
                extra.push((
                    "Changes already made since the task started (kept)".to_owned(),
                    diff.trim().to_owned(),
                ));
            }
        }
        extra.push((
            "Latest ./verify --all".to_owned(),
            self.run_verify(&["--all"]).await.summary,
        ));
        let fields = vec![
            ("Task".to_owned(), card.id.to_string()),
            (
                "Attempt".to_owned(),
                format!("{attempt} of {}", self.params.max_attempts),
            ),
        ];
        let cites: &[String] = if self.params.cited_spec {
            &card.requires
        } else {
            &[]
        };
        self.briefing(
            Role::Builder,
            fields,
            Some((&card.file, card_text)),
            extra,
            cites,
        )
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
            self.project.set_card_fields(
                id,
                &[("status", "todo".to_owned()), ("attempts", "0".to_owned())],
            )?;
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
