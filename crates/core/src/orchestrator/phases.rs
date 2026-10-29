//! The phases: bootstrap, the task loop (build, verify, review, split,
//! block, replan) and the final review, one module each. Here: what they
//! share, the `verify` call and the prompt.

use std::time::Duration;

use crate::briefing::{self, Briefing};
use crate::conflict::Conflict;
use crate::project::conflict_path;
use crate::role::Role;
use crate::spec;
use crate::task::{TaskCard, TaskId, TaskStatus};
use crate::util::clip;
use crate::verify::{VerifyOutcome, interpret, retarget};

use super::{RunError, Runner};

mod bootstrap;
mod final_review;
mod planner;
mod review;
mod task;
mod task_loop;

/// What a session's prompt adds to the role prompt, spec and map.
#[derive(Default)]
struct PromptParts<'a> {
    /// Header lines, `Key: value`.
    fields: Vec<(String, String)>,
    /// The task card's path and text.
    card: Option<(&'a str, &'a str)>,
    /// Titled sections after the card.
    extra: Vec<(String, String)>,
    /// The requirements to keep from the spec (`spec::cited`); the whole
    /// spec when empty.
    cites: &'a [String],
}

/// The briefing section that lists the checks a card may amend; the
/// builder and reviewer prompts refer to it by this title.
const AMENDS_TITLE: &str = "Checks this card may amend";

/// The briefing section that shows a replan the conflicts that stood.
pub(crate) const CONFLICTS_TITLE: &str = "Declared conflicts";

impl Runner {
    /// `./verify` with `args`, judged as its contract says.
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

    /// The commit the current task started from.
    fn current_base(&self) -> String {
        self.ledger
            .current
            .as_ref()
            .map(|c| c.base.clone())
            .unwrap_or_default()
    }

    /// Writes `status` on `task`'s card and commits the run's checkpoint
    /// with `message`.
    fn checkpoint_task(
        &mut self,
        task: &TaskId,
        status: TaskStatus,
        message: &str,
    ) -> Result<(), RunError> {
        self.project.set_status(task, status)?;
        self.ledger.checkpoint = Some(self.deps.vcs.commit_all(message)?);
        Ok(())
    }

    /// The section that sizes the cards a planner writes to the builders'
    /// configured context budget. A cap after a memory failure is lifted
    /// again; cards outlive it.
    fn card_sizing(&self) -> (String, String) {
        (
            briefing::CARD_SIZING_TITLE.to_owned(),
            briefing::card_sizing(self.ledger.builder_budget_tokens()),
        )
    }

    /// The notes of `id`, trimmed and clipped for a prompt.
    fn clipped_note(&self, id: &str) -> Option<String> {
        self.project
            .note(id)
            .map(|note| clip(note.trim(), self.params.note_clip_bytes))
    }

    /// The section that lists the checks of done tasks `card` may amend,
    /// with the conflicts that named them (ADR 0009); `None` when it amends
    /// none. The role prompts say what the builder and reviewer do with it.
    fn amends_section(&self, card: &TaskCard) -> Option<(String, String)> {
        if card.amends.is_empty() {
            return None;
        }
        let checks = card
            .amends
            .iter()
            .map(|check| format!("- `{check}`"))
            .collect::<Vec<_>>()
            .join("\n");
        let conflicts = self
            .project
            .load_backlog()
            .conflicts
            .iter()
            .filter(|conflict| {
                card.amends
                    .iter()
                    .any(|check| conflict.checks.contains(check))
            })
            .map(|conflict| self.shown_conflict(conflict))
            .collect::<Vec<_>>()
            .join("\n\n");
        Some((
            AMENDS_TITLE.to_owned(),
            format!("{checks}\n\nThe conflicts that named them:\n\n{conflicts}"),
        ))
    }

    /// A conflict as a briefing shows it: its path, then what its builder
    /// wrote, clipped like notes.
    fn shown_conflict(&self, conflict: &Conflict) -> String {
        format!(
            "{}:\n\n{}",
            conflict_path(conflict.task.as_str()),
            clip(conflict.text.trim(), self.params.note_clip_bytes)
        )
    }

    /// The section that shows a replan the conflicts that stood, so one
    /// that did not see it happen still does (ADR 0009); `None` without any.
    fn conflicts_section(&self) -> Option<(String, String)> {
        let conflicts = self.project.load_backlog().conflicts;
        (!conflicts.is_empty()).then(|| {
            (
                CONFLICTS_TITLE.to_owned(),
                conflicts
                    .iter()
                    .map(|conflict| self.shown_conflict(conflict))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        })
    }

    /// Composes a prompt from the project's role prompt, spec and map, and
    /// what `parts` adds for this session.
    fn briefing(&self, role: Role, parts: PromptParts<'_>) -> Result<String, RunError> {
        let PromptParts {
            mut fields,
            card,
            extra,
            cites,
        } = parts;
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
}
