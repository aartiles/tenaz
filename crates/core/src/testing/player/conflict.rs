//! The scripted harness's conflict with a done check (ADR 0009): the
//! builder that declares it, and the replan that amends the check.

use super::{Player, card};
use crate::task::{TaskCard, set_card_fields};
use crate::testing::{Routing, ScriptedConflict};

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
        let Some(ScriptedConflict {
            task,
            check,
            routing,
        }) = self.script.conflict.clone()
        else {
            return false;
        };
        let shown = format!("## {}", crate::orchestrator::CONFLICTS_TITLE);
        if !prompt.contains(&shown) || self.cwd.join(".tenaz/tasks/T005.md").exists() {
            return false;
        }
        if !matches!(routing, Routing::ReopenWithoutAmend) {
            self.write_amending(&check, routing).await;
        }
        let path = format!(".tenaz/tasks/{task}.md");
        let blocked = std::fs::read_to_string(self.cwd.join(&path)).unwrap_or_default();
        self.write_tool(&path, &routed(&task, &blocked, routing), false)
            .await;
        self.finish("Amended.");
        true
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
        Routing::ReopenUnlinked | Routing::ReopenWithoutAmend => {
            set_card_fields(blocked, &[("status", "todo".to_owned())])
        }
        Routing::MarkDone => set_card_fields(blocked, &[("status", "done".to_owned())]),
        Routing::Reopen | Routing::ReopenAmenderMarkedDone => {
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
