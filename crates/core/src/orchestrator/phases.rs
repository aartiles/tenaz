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
use crate::verify::{TreeState, VerifyOutcome, interpret, judge_attempt, retarget, writes_error};

use super::{RunError, Runner};

mod bootstrap;
mod final_review;
mod job;
mod planner;
mod review;
mod task;
mod task_loop;

pub(crate) use planner::PlannerSession;

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

/// The briefing section that tells a replan why the last replan's cards
/// were thrown away; the replan prompt refers to it by this title.
pub(crate) const REJECTED_REPLAN_TITLE: &str = "Why the last replan was rejected";

/// `verify` on an attempt's work (`Runner::verify_attempt`).
pub(crate) struct AttemptVerify {
    pub all: VerifyOutcome,
    pub own: VerifyOutcome,
    /// Why the attempt fails; `None` when it passes.
    pub failure: Option<String>,
}

impl Runner {
    /// The files changed from `from` to `to` (the working tree when
    /// `None`), as `git diff --stat` lists them, cut to `statClipBytes` at
    /// a whole line. Empty when none changed.
    fn changed_files(
        &self,
        from: &str,
        to: Option<&str>,
        exclude_agent: bool,
    ) -> Result<String, RunError> {
        let stat = self.deps.vcs.diff_stat(from, to, exclude_agent)?;
        Ok(clip_lines(stat.trim(), self.params.stat_clip_bytes))
    }

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

    /// `verify` on `task`'s work, judged as an attempt is: the whole suite,
    /// then the task's own checks, and why the attempt fails
    /// (`judge_attempt`), or, when it passes, that `verify` changed the
    /// working tree (`tree_writes`, ADR 0035). The one place both the end
    /// of an attempt and the builder's follow-up before it (ADR 0034)
    /// judge it.
    pub(crate) async fn verify_attempt(&self, task: &TaskId) -> AttemptVerify {
        let before = self.tree_state();
        let all = self.run_verify(&["--all"]).await;
        let own = self.run_verify(&["--task", task.as_str()]).await;
        let failure = judge_attempt(task.as_str(), &all, &own, self.ledger.passed_checks)
            .or_else(|| self.tree_writes(before.as_ref()));
        AttemptVerify { all, own, failure }
    }

    /// The working tree as `verify` must leave it (`TreeState`, ADR 0035);
    /// `None`, logged, when git cannot say what changed.
    pub(crate) fn tree_state(&self) -> Option<TreeState> {
        let vcs = &self.deps.vcs;
        let read = vcs
            .changed_paths()
            .and_then(|changed| Ok((vcs.ignored(&changed)?, changed)));
        let (ignored, changed) = match read {
            Ok(read) => read,
            Err(e) => {
                self.log(&format!(
                    "  cannot tell whether verify changed the working tree: {e}"
                ));
                return None;
            }
        };
        Some(
            changed
                .into_iter()
                .map(|path| {
                    let hash = (!ignored.contains(&path))
                        .then(|| self.project.content_hash(&path))
                        .flatten();
                    (path, hash)
                })
                .collect(),
        )
    }

    /// Why `verify`, run since the tree was `before`, may not leave it as
    /// it is now (`writes_error`); `None` when it left it as it was, or git
    /// cannot say on either side.
    pub(crate) fn tree_writes(&self, before: Option<&TreeState>) -> Option<String> {
        writes_error(before?, &self.tree_state()?)
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

/// Cuts `text` to the whole lines that fit in `limit` bytes and says how
/// many lines were left out, for a list such as a diff's file stat.
pub(crate) fn clip_lines(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let (mut kept, mut left_out) = (0, 0);
    for line in text.split_inclusive('\n') {
        if left_out == 0 && kept + line.len() <= limit {
            kept += line.len();
        } else {
            left_out += 1;
        }
    }
    let lines = if left_out == 1 { "line" } else { "lines" };
    format!("{}[... clipped {left_out} {lines}]", &text[..kept])
}

#[cfg(test)]
mod tests {
    use super::clip_lines;

    #[test]
    fn clip_lines_keeps_whole_lines_and_counts_the_rest() {
        assert_eq!(clip_lines("a\nb\n", 10), "a\nb\n");
        assert_eq!(
            clip_lines("ab\ncd\nef\n", 7),
            "ab\ncd\n[... clipped 1 line]"
        );
        assert_eq!(clip_lines("abcdef\ng", 3), "[... clipped 2 lines]");
        // Lines are cut whole, so a character is never split.
        assert_eq!(clip_lines("é\r\nü\r\n", 5), "é\r\n[... clipped 1 line]");
    }
}
