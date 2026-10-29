//! A builder's declaration that its card cannot pass without changing
//! checks of done tasks, and the cards a replan writes to amend them
//! (ADR 0009).

use crate::backlog::Backlog;
use crate::task::frontmatter::{Value, parse_frontmatter};
use crate::task::{TaskId, TaskStatus};

/// `.tenaz/conflicts/<TASK>.md`: the checks in the way and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub task: TaskId,
    /// Check IDs such as `T016a.look-cues`.
    pub checks: Vec<String>,
    /// The whole file, as its builder wrote it.
    pub text: String,
}

impl Conflict {
    /// Reads the conflict `task`'s builder wrote: front matter with a
    /// non-empty `checks` list of check IDs.
    pub fn read(task: &TaskId, text: &str) -> Result<Self, String> {
        let fm = parse_frontmatter(text).map_err(|e| e.to_string())?;
        let checks = fm.get("checks").map(Value::to_list).unwrap_or_default();
        if checks.is_empty() {
            return Err("its front matter names no `checks`".to_owned());
        }
        if let Some(bad) = checks.iter().find(|c| check_task(c).is_none()) {
            return Err(format!("`{bad}` is not a check ID such as T007.name"));
        }
        Ok(Self {
            task: task.clone(),
            checks,
            text: text.to_owned(),
        })
    }

    /// Why the conflict cannot stand against `backlog`, if it cannot: each
    /// check must belong to a done task other than its own. A conflict with
    /// its own task's checks is a card to fix, not a done check to amend.
    pub fn validate(&self, backlog: &Backlog) -> Result<(), String> {
        self.checks
            .iter()
            .find_map(|check| self.why_not(check, backlog))
            .map_or(Ok(()), Err)
    }

    /// Why `check` cannot be in this task's way, if it cannot.
    fn why_not(&self, check: &str, backlog: &Backlog) -> Option<String> {
        let owner = check_task(check)?;
        if owner == self.task {
            return Some(format!("`{check}` is {}'s own check", self.task));
        }
        match backlog.get(&owner) {
            Some(card) if card.status == TaskStatus::Done => None,
            Some(card) => Some(format!(
                "`{check}` belongs to {owner}, which is {}",
                card.status
            )),
            None => Some(format!(
                "`{check}` belongs to {owner}, which does not exist"
            )),
        }
    }
}

/// The task a check ID belongs to: `T016a` for `T016a.look-cues`.
pub fn check_task(check: &str) -> Option<TaskId> {
    let (task, name) = check.split_once('.')?;
    (!name.is_empty()).then_some(())?;
    TaskId::parse(task).ok()
}

/// What is wrong with the `amends` of the cards in `backlog` that are not
/// done: each check they name must be one a conflict names.
pub fn amend_errors(backlog: &Backlog, conflicts: &[Conflict]) -> Vec<String> {
    let declared = |check: &String| conflicts.iter().any(|c| c.checks.contains(check));
    backlog
        .cards()
        .filter(|card| card.status != TaskStatus::Done)
        .flat_map(|card| {
            card.amends
                .iter()
                .filter(|check| !declared(check))
                .map(move |check| {
                    format!(
                        "{}: amends `{check}`, which no file in .tenaz/conflicts/ names",
                        card.id
                    )
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskCard;

    fn id(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn card(id: &str, status: &str, extra: &str) -> TaskCard {
        let text = format!("---\nid: {id}\ntitle: t\nstatus: {status}\n{extra}---\n");
        TaskCard::parse(&format!("{id}.md"), &text).unwrap()
    }

    fn backlog() -> Backlog {
        Backlog::from_cards([
            card("T016a", "done", ""),
            card("T016b", "active", ""),
            card("T015", "todo", ""),
        ])
    }

    const LOTION: &str =
        "---\nchecks: [T016a.look-cues]\n---\nThe white floor and edge_grad cannot both hold.\n";

    #[test]
    fn a_conflict_names_the_checks_in_the_way() {
        let conflict = Conflict::read(&id("T016b"), LOTION).unwrap();
        assert_eq!(conflict.checks, ["T016a.look-cues"]);
        assert_eq!(conflict.text, LOTION);
        assert_eq!(conflict.validate(&backlog()), Ok(()));
    }

    #[test]
    fn a_conflict_without_check_ids_is_not_read() {
        for text in [
            "no front matter\n",
            "---\nchecks: []\n---\n",
            "---\nchecks: [look-cues]\n---\n",
            "---\nchecks: [T016a.]\n---\n",
        ] {
            assert!(Conflict::read(&id("T016b"), text).is_err(), "{text}");
        }
    }

    /// Only a done task's check can be in the way: its own is the card's
    /// to fix, and one not done yet can still change.
    #[test]
    fn a_conflict_stands_only_against_checks_of_done_tasks() {
        for (check, why) in [
            ("T016b.backdrop", "own check"),
            ("T015.separation", "which is todo"),
            ("T099.x", "does not exist"),
        ] {
            let text = format!("---\nchecks: [{check}]\n---\n");
            let conflict = Conflict::read(&id("T016b"), &text).unwrap();
            let err = conflict.validate(&backlog()).unwrap_err();
            assert!(err.contains(why), "{check}: {err}");
        }
    }

    #[test]
    fn a_card_may_amend_only_checks_a_conflict_names() {
        let conflict = Conflict::read(&id("T016b"), LOTION).unwrap();
        let backlog = Backlog::from_cards([
            card("T016g", "todo", "amends: [T016a.look-cues]\n"),
            card("T016h", "todo", "amends: [T009.lights]\n"),
            card("T001", "done", "amends: [T000.old]\n"),
        ]);
        assert_eq!(
            amend_errors(&backlog, &[conflict]),
            ["T016h: amends `T009.lights`, which no file in .tenaz/conflicts/ names"]
        );
    }
}
