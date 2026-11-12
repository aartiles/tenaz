//! The scripted harness's conflict with a done check (ADR 0009): the
//! builder that declares it, and the replan that amends the check.

use super::{Player, card};
use crate::task::{TaskCard, set_card_fields};
use crate::testing::{Routing, ScriptedConflict, UNDECLARED};

impl Player {
    /// The first session of the script's conflict task declares its
    /// conflict and stops.
    pub(super) async fn conflicting(&mut self, task: &str) -> bool {
        let Some(ScriptedConflict {
            task: at, check, ..
        }) = self.script.conflict.clone()
        else {
            return false;
        };
        if !self.first(Some(&at), "conflict", task) {
            return false;
        }
        self.write_tool(
            &format!(".tenaz/conflicts/{task}.md"),
            &format!("---\nchecks: [{check}]\n---\n{task} and {check} cannot both pass.\n"),
            false,
        )
        .await;
        self.finish("Declared a conflict.");
        true
    }

    /// The first replan shown the conflict adds `T005`, which amends the
    /// check in the way and does the blocked task's work, and drops the
    /// blocked task, or reopens it to wait for `T005` (ADR 0018).
    pub(super) async fn amend(&mut self, prompt: &str) -> bool {
        let Some(ScriptedConflict { check, routing, .. }) = self.script.conflict.clone() else {
            return false;
        };
        let shown = format!("## {}", crate::orchestrator::CONFLICTS_TITLE);
        if !prompt.contains(&shown) || self.cwd.join(".tenaz/tasks/T005.md").exists() {
            return false;
        }
        let told = format!("## {}", crate::orchestrator::REJECTED_REPLAN_TITLE);
        let undeclared_first = matches!(
            routing,
            Routing::AmendUndeclaredFirst | Routing::AmendUndeclaredFixedWhenTold
        );
        if undeclared_first && !prompt.contains(&told) {
            self.write_amending(UNDECLARED, routing).await;
            self.finish("Amended a check no conflict names.");
            return true;
        }
        if !matches!(
            routing,
            Routing::ReopenWithoutAmend | Routing::ReopenWithoutAmendThenDrop
        ) {
            self.write_amending(&check, routing).await;
        }
        self.route(routing).await;
        self.finish("Amended.");
        true
    }

    /// Told that the task it reopened would be blocked again, a replan
    /// whose routing says so gives its work to a new `T005` and drops it;
    /// any other ignores the prompt.
    pub(super) async fn drop_refused_reopen(&mut self) {
        let Some(ScriptedConflict {
            task,
            routing: Routing::ReopenWithoutAmendThenDrop,
            ..
        }) = self.script.conflict.clone()
        else {
            return self.finish("Left as it is.");
        };
        let routed_to = card("T005", &["R3"], &["T001"], None);
        self.write_tool(".tenaz/tasks/T005.md", &routed_to, false)
            .await;
        let path = format!(".tenaz/tasks/{task}.md");
        let card = std::fs::read_to_string(self.cwd.join(&path)).unwrap_or_default();
        let dropped = set_card_fields(&card, &[("status", "dropped".to_owned())]);
        self.write_tool(&path, &dropped, false).await;
        self.finish("Dropped.");
    }

    /// Told before it ends that its cards would be thrown away for its
    /// amend of `UNDECLARED` (`names_undeclared`), a replan whose routing
    /// says so amends the declared check and routes the blocked task as
    /// `Reopen` does; any other leaves its cards as they are.
    pub(super) async fn fix_invalid_cards(&mut self, names_undeclared: bool) {
        let Some(ScriptedConflict {
            check,
            routing: routing @ Routing::AmendUndeclaredFixedWhenTold,
            ..
        }) = self.script.conflict.clone().filter(|_| names_undeclared)
        else {
            return self.finish("Left as it is.");
        };
        self.write_amending(&check, routing).await;
        self.route(routing).await;
        self.finish("Fixed the cards.");
    }

    /// Rewrites the script's blocked card as `routing` says (`routed`).
    async fn route(&mut self, routing: Routing) {
        let Some(ScriptedConflict { task, .. }) = self.script.conflict.clone() else {
            return;
        };
        let path = format!(".tenaz/tasks/{task}.md");
        let blocked = std::fs::read_to_string(self.cwd.join(&path)).unwrap_or_default();
        self.write_tool(&path, &routed(&task, &blocked, routing), false)
            .await;
    }

    /// Writes `T005`, which amends `check`, as `routing` says.
    async fn write_amending(&mut self, check: &str, routing: Routing) {
        let status = match routing {
            Routing::ReopenAmenderMarkedDone => "done",
            _ => "todo",
        };
        let amending = card("T005", &["R3"], &["T001"], None).replacen(
            "status: todo\n",
            &format!("status: {status}\namends: [{check}]\n"),
            1,
        );
        self.write_tool(".tenaz/tasks/T005.md", &amending, false)
            .await;
    }
}

/// The text of blocked card `task` once the replan routed its work to
/// `T005`.
fn routed(task: &str, blocked: &str, routing: Routing) -> String {
    match routing {
        Routing::Drop => set_card_fields(blocked, &[("status", "dropped".to_owned())]),
        Routing::ReopenUnlinked
        | Routing::ReopenWithoutAmend
        | Routing::ReopenWithoutAmendThenDrop => {
            set_card_fields(blocked, &[("status", "todo".to_owned())])
        }
        Routing::MarkDone => set_card_fields(blocked, &[("status", "done".to_owned())]),
        Routing::Reopen
        | Routing::ReopenAmenderMarkedDone
        | Routing::AmendUndeclaredFirst
        | Routing::AmendUndeclaredFixedWhenTold => {
            let card =
                TaskCard::parse(&format!("{task}.md"), blocked).expect("the blocked card parses");
            let depends: Vec<String> = card
                .depends
                .iter()
                .map(ToString::to_string)
                .chain(["T005".to_owned()])
                .collect();
            set_card_fields(
                blocked,
                &[
                    ("status", "todo".to_owned()),
                    ("depends", format!("[{}]", depends.join(", "))),
                ],
            )
        }
    }
}
