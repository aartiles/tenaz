//! A builder's declaration that its card cannot pass without changing
//! checks of done tasks, and the cards a replan writes to amend them
//! (ADR 0009).

use crate::backlog::Backlog;
use crate::task::frontmatter::{Value, parse_frontmatter};
use crate::task::{TaskCard, TaskId, TaskStatus};

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

    /// Whether the conflict still stands in `backlog`: its task is blocked,
    /// dropped once a replan routed around it, or `todo` while another card
    /// still to build amends one of its checks, as when a replan reopened it
    /// behind that card (ADR 0018); such a task waits for those cards
    /// (`LoadedBacklog::ready`). A conflict file of a task in any other
    /// state is left from an earlier attempt, or its amend has been made;
    /// one whose checks do not belong to done tasks (`validate`) never
    /// stands.
    pub fn stands_in(&self, backlog: &Backlog) -> bool {
        let Some(card) = backlog.get(&self.task) else {
            return false;
        };
        if self.validate(backlog).is_err() {
            return false;
        }
        match card.status {
            TaskStatus::Blocked | TaskStatus::Dropped => true,
            // An active task's attempt removed the file it had; one there
            // now is that attempt's, not yet read.
            TaskStatus::Done | TaskStatus::Split | TaskStatus::Active => false,
            TaskStatus::Todo => self.amenders(backlog).next().is_some(),
        }
    }

    /// The cards of `backlog` still to build, other than its own task, that
    /// amend a check this conflict names.
    fn amenders<'a>(&'a self, backlog: &'a Backlog) -> impl Iterator<Item = &'a TaskCard> {
        backlog.cards().filter(|card| {
            card.id != self.task
                && to_build(card.status)
                && card.amends.iter().any(|c| self.checks.contains(c))
        })
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

/// Whether a card of `status` may still be built: one done, split or
/// dropped never is, so its `amends` licenses nothing more.
fn to_build(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Todo | TaskStatus::Active | TaskStatus::Blocked
    )
}

/// What is wrong with the `amends` of the cards in `backlog` that may
/// still be built: each check they name must be one a conflict names (see
/// `not_named`).
pub fn amend_errors(backlog: &Backlog, conflicts: &[Conflict]) -> Vec<String> {
    let declared = |check: &String| conflicts.iter().any(|c| c.checks.contains(check));
    backlog
        .cards()
        .filter(|card| to_build(card.status))
        .flat_map(|card| {
            card.amends
                .iter()
                .filter(|check| !declared(check))
                .map(move |check| not_named(&card.id, check))
        })
        .collect()
}

/// The error for card `id` amending `check`, which no conflict that stands
/// names, with the way out: drop the `amends`, since only a builder
/// declares a conflict. Shown to the bootstrap and to planners alike.
pub(crate) fn not_named(id: &TaskId, check: &str) -> String {
    format!(
        "{id}: amends `{check}`, which no conflict that stands names. Remove `{check}` from this card's `amends`. Do not write .tenaz/conflicts/: only a builder declares a conflict, and a later replan may then amend the check."
    )
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

    const AMENDS: &str = "amends: [T016a.look-cues]\n";

    /// A replan may reopen the blocked task behind the amending card
    /// instead of dropping it; its conflict stands while a card still to
    /// build amends one of its checks, wherever that card is, so the split
    /// children of the amending card keep it standing.
    #[test]
    fn a_conflict_stands_while_its_task_is_blocked_dropped_or_waits_for_an_amend() {
        let conflict = Conflict::read(&id("T016b"), LOTION).unwrap();
        for (task, amender, amender_amends, stands) in [
            ("blocked", "done", AMENDS, true),
            ("dropped", "done", AMENDS, true),
            ("todo", "todo", AMENDS, true),
            ("todo", "active", AMENDS, true),
            ("todo", "blocked", AMENDS, true),
            ("todo", "todo", "amends: [T009.lights]\n", false),
            (
                "todo",
                "todo",
                "amends: [T009.lights, T016a.look-cues]\n",
                true,
            ),
            ("todo", "done", AMENDS, false),
            ("todo", "dropped", AMENDS, false),
            ("todo", "split", AMENDS, false),
            ("active", "todo", AMENDS, false),
            ("done", "todo", AMENDS, false),
            ("split", "todo", AMENDS, false),
        ] {
            let backlog = Backlog::from_cards([
                card("T016a", "done", ""),
                card("T016b", task, ""),
                card("T016g", amender, amender_amends),
            ]);
            let row = format!("{task} with T016g {amender} {}", amender_amends.trim_end());
            assert_eq!(conflict.stands_in(&backlog), stands, "{row}");
        }
        assert!(!conflict.stands_in(&Backlog::default()), "no card");
        let not_done =
            Backlog::from_cards([card("T016a", "todo", ""), card("T016b", "blocked", "")]);
        assert!(!conflict.stands_in(&not_done), "T016a is not done");
    }

    /// A conflict naming two checks stands while one of them is amended.
    #[test]
    fn a_conflict_stands_while_any_of_its_checks_is_amended() {
        let text = "---\nchecks: [T016a.look-cues, T009.lights]\n---\n";
        let conflict = Conflict::read(&id("T016b"), text).unwrap();
        let backlog = Backlog::from_cards([
            card("T016a", "done", ""),
            card("T009", "done", ""),
            card("T016b", "todo", ""),
            card("T016g", "todo", AMENDS),
        ]);
        assert!(conflict.stands_in(&backlog));
    }

    /// A task does not amend its own conflict: its attempt would remove
    /// the conflict that licenses the amend.
    #[test]
    fn a_conflict_does_not_stand_behind_its_own_task() {
        let conflict = Conflict::read(&id("T016b"), LOTION).unwrap();
        let backlog =
            Backlog::from_cards([card("T016a", "done", ""), card("T016b", "todo", AMENDS)]);
        assert!(!conflict.stands_in(&backlog));
    }

    #[test]
    fn a_card_may_amend_only_checks_a_conflict_names() {
        let conflict = Conflict::read(&id("T016b"), LOTION).unwrap();
        let backlog = Backlog::from_cards([
            card("T016g", "todo", "amends: [T016a.look-cues]\n"),
            card("T016h", "todo", "amends: [T009.lights]\n"),
            card("T001", "done", "amends: [T000.old]\n"),
            card("T002", "dropped", "amends: [T000.old]\n"),
            card("T003", "split", "amends: [T000.old]\n"),
            card("T004", "active", "amends: [T000.old]\n"),
            card("T005", "blocked", "amends: [T000.old]\n"),
        ]);
        let not_named = |card: &str, check: &str| not_named(&TaskId::parse(card).unwrap(), check);
        assert_eq!(
            amend_errors(&backlog, &[conflict]),
            [
                not_named("T004", "T000.old"),
                not_named("T005", "T000.old"),
                not_named("T016h", "T009.lights"),
            ]
        );
    }

    /// The error names the card and the check, and says what to do.
    #[test]
    fn an_amend_no_conflict_names_says_to_drop_it() {
        assert_eq!(
            not_named(&TaskId::parse("T024").unwrap(), "T002.state"),
            "T024: amends `T002.state`, which no conflict that stands names. Remove `T002.state` from this card's `amends`. Do not write .tenaz/conflicts/: only a builder declares a conflict, and a later replan may then amend the check."
        );
    }
}
