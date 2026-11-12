//! How Tenaz judges the cards a split or replan session left: the
//! statuses it may not set are put right, then what is wrong with the cards
//! is named. The prompts before the session ends and the end itself read
//! the same `Judgement` (ADR 0018 and 0033).

use crate::backlog::{Backlog, LoadedBacklog};
use crate::conflict::{Conflict, amend_errors};
use crate::role::Role;
use crate::spec::{Requirement, coverage};
use crate::task::{TaskCard, TaskId, TaskStatus};

/// A split or replan session whose cards Tenaz judges: its role and the
/// backlog as it was before it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlannerSession<'a> {
    pub role: Role,
    pub before: &'a Backlog,
}

/// The cards a planner session left, as Tenaz judges them.
pub(crate) struct Judgement {
    /// The cards, with the statuses Tenaz puts right.
    pub after: Backlog,
    /// What Tenaz writes on the cards when it keeps them.
    pub fixes: Vec<CardFix>,
    /// Why Tenaz throws the cards away; none when it keeps them.
    pub errors: Vec<String>,
}

impl Judgement {
    /// The blocked tasks a replan set back to `todo` that Tenaz blocks
    /// again (ADR 0018).
    pub(crate) fn refused_reopens(&self) -> impl Iterator<Item = &TaskId> {
        self.fixes
            .iter()
            .filter(|f| f.refused_reopen())
            .map(|f| &f.id)
    }
}

/// The cards a planner `session` left, judged: the statuses it may not set
/// are put right first (`status_fixes`), then each `amends` is checked
/// against the conflicts that stand with those statuses (`conflicts_of`),
/// and the requirements left uncovered and the cards deleted are named.
/// `cards` are the cards as read, before their `amends` are checked
/// (`Project::load_cards`).
pub(crate) fn judge(
    session: PlannerSession<'_>,
    cards: LoadedBacklog,
    requirements: &[Requirement],
    conflicts_of: impl Fn(&Backlog) -> Vec<Conflict>,
) -> Judgement {
    let PlannerSession { role, before } = session;
    let fixes = status_fixes(role, before, &cards.backlog, &conflicts_of);
    let after = with_fixes(&cards.backlog, &fixes);
    let mut errors = cards.errors;
    errors.extend(amend_errors(&after, &conflicts_of(&after)));
    let cov = coverage(requirements, after.cards(), false);
    if !cov.missing.is_empty() {
        errors.push(format!(
            "requirements left uncovered: {}",
            cov.missing.join(", ")
        ));
    }
    errors.extend(
        before
            .ids()
            .filter(|id| !after.contains(id))
            .map(|id| format!("card {id} was deleted")),
    );
    Judgement {
        after,
        fixes,
        errors,
    }
}

/// What Tenaz does to a card a planner left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusFix {
    /// `todo` with no attempts, as a new card starts.
    Reset,
    /// Back to the status it had before the planner.
    Restore(TaskStatus),
}

/// What Tenaz writes on one card a planner left.
pub(crate) struct CardFix {
    pub id: TaskId,
    pub fix: StatusFix,
    /// A replan set it from `blocked` back to `todo` (`reopens`).
    reopen: bool,
}

impl CardFix {
    /// A reopen Tenaz undoes.
    pub(crate) fn refused_reopen(&self) -> bool {
        self.reopen && self.fix == StatusFix::Restore(TaskStatus::Blocked)
    }
}

/// What Tenaz writes on the cards a `role` session left, `after`, against
/// the backlog as it was `before`: the statuses it may not set are put
/// right (`status_fix`), then a replan's reopens are judged against the
/// conflicts that stand with those statuses (`conflicts_of`, `reopen_fix`,
/// ADR 0018).
fn status_fixes(
    role: Role,
    before: &Backlog,
    after: &Backlog,
    conflicts_of: impl Fn(&Backlog) -> Vec<Conflict>,
) -> Vec<CardFix> {
    let reopen = |t: &TaskCard| reopens(role, before.get(&t.id), t);
    let mut fixes: Vec<CardFix> = after
        .cards()
        .filter(|t| !reopen(t))
        .filter_map(|t| {
            let fix = status_fix(before.get(&t.id), t)?;
            Some(CardFix {
                id: t.id.clone(),
                fix,
                reopen: false,
            })
        })
        .collect();
    let conflicts = conflicts_of(&with_fixes(after, &fixes));
    fixes.extend(after.cards().filter(|t| reopen(t)).map(|t| CardFix {
        id: t.id.clone(),
        fix: reopen_fix(&t.id, &conflicts),
        reopen: true,
    }));
    fixes
}

/// `backlog` as it is once `fixes` are written (`Runner::apply`).
fn with_fixes(backlog: &Backlog, fixes: &[CardFix]) -> Backlog {
    Backlog::from_cards(backlog.cards().map(|t| {
        let mut card = t.clone();
        if let Some(f) = fixes.iter().find(|f| f.id == t.id) {
            match f.fix {
                StatusFix::Reset => {
                    card.status = TaskStatus::Todo;
                    card.attempts = 0;
                }
                StatusFix::Restore(old) => card.status = old,
            }
        }
        card
    }))
}

/// What to do with card `after`, which was `before` the planner; `None`
/// keeps it. A new card starts as `todo`; an old one keeps its status
/// unless the planner may change it (`may_change_status`).
fn status_fix(before: Option<&TaskCard>, after: &TaskCard) -> Option<StatusFix> {
    match before {
        None => (after.status != TaskStatus::Todo).then_some(StatusFix::Reset),
        Some(old) => {
            (!may_change_status(old.status, after.status)).then_some(StatusFix::Restore(old.status))
        }
    }
}

/// Whether a `role` session set card `after`, which was `before` it, from
/// `blocked` back to `todo`, which only a replan may (ADR 0018).
fn reopens(role: Role, before: Option<&TaskCard>, after: &TaskCard) -> bool {
    role == Role::Replan
        && before.is_some_and(|old| old.status == TaskStatus::Blocked)
        && after.status == TaskStatus::Todo
}

/// What to do with blocked task `id` that a replan set back to `todo`,
/// given the `conflicts` that stand once the other statuses are put
/// right. While its own conflict stands (a card still to build amends one
/// of its checks), it starts again with no attempts and
/// `LoadedBacklog::ready` holds it until that card is built (ADR 0018).
/// Otherwise it is blocked again.
fn reopen_fix(id: &TaskId, conflicts: &[Conflict]) -> StatusFix {
    if conflicts.iter().any(|c| &c.task == id) {
        StatusFix::Reset
    } else {
        StatusFix::Restore(TaskStatus::Blocked)
    }
}

/// Whether a split or replan session may change a card's status `from`
/// one `to` another: it may drop a task that is not started or is blocked,
/// and change nothing else. Blocked to todo is not allowed here: a replan's
/// reopen is judged apart (`reopen_fix`, ADR 0018).
fn may_change_status(from: TaskStatus, to: TaskStatus) -> bool {
    from == to
        || (to == TaskStatus::Dropped && matches!(from, TaskStatus::Todo | TaskStatus::Blocked))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status; a new one fails to compile here until it is listed.
    fn all() -> [TaskStatus; 6] {
        use TaskStatus::{Active, Blocked, Done, Dropped, Split, Todo};
        let all = [Todo, Active, Done, Blocked, Split, Dropped];
        for status in all {
            match status {
                Todo | Active | Done | Blocked | Split | Dropped => {}
            }
        }
        all
    }

    #[test]
    fn a_planner_may_drop_only_a_task_not_started_or_blocked() {
        let droppable: Vec<TaskStatus> = all()
            .into_iter()
            .filter(|&from| may_change_status(from, TaskStatus::Dropped))
            .collect();
        // A dropped task stays dropped: that is no change.
        assert_eq!(
            droppable,
            [TaskStatus::Todo, TaskStatus::Blocked, TaskStatus::Dropped]
        );
    }

    #[test]
    fn a_planner_may_change_no_other_status() {
        for from in all() {
            for to in all().into_iter().filter(|&to| to != TaskStatus::Dropped) {
                assert_eq!(may_change_status(from, to), from == to, "{from} -> {to}");
            }
        }
    }

    fn card(id: &str, status: &str) -> TaskCard {
        let text = format!("---\nid: {id}\ntitle: t\nstatus: {status}\n---\n");
        TaskCard::parse(&format!("{id}.md"), &text).unwrap()
    }

    fn conflict_of(task: &str) -> Conflict {
        let id = TaskId::parse(task).unwrap();
        Conflict::read(&id, "---\nchecks: [T001.runs]\n---\n").unwrap()
    }

    /// The cards a planner left, to judge against `before` = [T001 done,
    /// T002 blocked], with T002's conflict on `T001.runs` (when `conflict`)
    /// standing while T002 is blocked or reopened behind its amend.
    struct Left {
        role: Role,
        cards: Vec<TaskCard>,
        load_errors: Vec<String>,
        requirements: Vec<Requirement>,
        conflict: bool,
    }

    impl Left {
        /// `cards` a replan left, read with no errors, for a spec with no
        /// requirements.
        fn replan(cards: Vec<TaskCard>) -> Self {
            Self {
                role: Role::Replan,
                cards,
                load_errors: Vec::new(),
                requirements: Vec::new(),
                conflict: true,
            }
        }

        fn judged(self) -> Judgement {
            let before = Backlog::from_cards([card("T001", "done"), card("T002", "blocked")]);
            let cards = LoadedBacklog {
                backlog: Backlog::from_cards(self.cards),
                errors: self.load_errors,
                ..LoadedBacklog::default()
            };
            let conflict = self.conflict;
            let conflicts = |b: &Backlog| {
                [conflict_of("T002")]
                    .into_iter()
                    .filter(|c| conflict && c.stands_in(b))
                    .collect()
            };
            let session = PlannerSession {
                role: self.role,
                before: &before,
            };
            judge(session, cards, &self.requirements, conflicts)
        }
    }

    /// Card T005, with `status`, amending `check`.
    fn amends(status: &str, check: &str) -> TaskCard {
        let text = format!("---\nid: T005\ntitle: t\nstatus: {status}\namends: [{check}]\n---\n");
        TaskCard::parse("T005.md", &text).unwrap()
    }

    /// The statuses a planner may not set are put right before its cards
    /// are judged: a blocked task it marked done is blocked again, so its
    /// conflict stands and the card amending its check is fine (ADR 0018).
    #[test]
    fn an_amend_is_judged_with_the_statuses_put_right() {
        let after = vec![
            card("T001", "done"),
            card("T002", "done"),
            amends("todo", "T001.runs"),
        ];
        assert_eq!(Left::replan(after).judged().errors, Vec::<String>::new());
    }

    /// An `amends` naming a check no conflict names is an error.
    #[test]
    fn an_amend_no_conflict_names_is_an_error() {
        let after = vec![
            card("T001", "done"),
            card("T002", "blocked"),
            amends("todo", "T001.other"),
        ];
        assert_eq!(
            Left::replan(after).judged().errors,
            ["T005: amends `T001.other`, which no file in .tenaz/conflicts/ names"]
        );
    }

    /// A card of the backlog before a replan or split that is gone is an
    /// error.
    #[test]
    fn a_deleted_card_is_an_error() {
        for role in [Role::Replan, Role::Split] {
            let left = Left {
                role,
                ..Left::replan(vec![card("T001", "done")])
            };
            let errors = left.judged().errors;
            assert_eq!(errors, ["card T002 was deleted"], "{role}");
        }
    }

    /// A requirement no card covers is an error.
    #[test]
    fn an_uncovered_requirement_is_an_error() {
        let r1 = Requirement {
            id: "R1".to_owned(),
            manual: false,
            text: "r".to_owned(),
        };
        let after = vec![card("T001", "done"), card("T002", "blocked")];
        assert_eq!(
            Left {
                requirements: vec![r1],
                ..Left::replan(after)
            }
            .judged()
            .errors,
            ["requirements left uncovered: R1"]
        );
    }

    /// The errors found reading the cards come first, unchanged.
    #[test]
    fn an_error_reading_the_cards_is_kept() {
        let after = vec![card("T001", "done")];
        assert_eq!(
            Left {
                load_errors: vec!["T009.md: bad".to_owned()],
                ..Left::replan(after)
            }
            .judged()
            .errors,
            ["T009.md: bad", "card T002 was deleted"]
        );
    }

    /// A split's statuses are put right with its own rules: it may not
    /// reopen a blocked task, which stays blocked; a replan may, behind the
    /// card that amends its check (ADR 0018).
    #[test]
    fn a_split_may_not_reopen_a_blocked_task() {
        let reopened = || {
            vec![
                card("T001", "done"),
                card("T002", "todo"),
                amends("todo", "T001.runs"),
            ]
        };
        let judged = |role| {
            let judgement = Left {
                role,
                ..Left::replan(reopened())
            }
            .judged();
            assert_eq!(judgement.errors, Vec::<String>::new(), "{role}");
            judgement
        };
        let t002 = TaskId::parse("T002").unwrap();
        let split = judged(Role::Split);
        assert_eq!(split.after.get(&t002).unwrap().status, TaskStatus::Blocked);
        assert_eq!(
            split.refused_reopens().count(),
            0,
            "a split reopens nothing"
        );
        let replan = judged(Role::Replan);
        assert_eq!(replan.after.get(&t002).unwrap().status, TaskStatus::Todo);
    }

    /// A blocked task set back to `todo` is reopened, with no attempts,
    /// only while its own conflict stands; another task's does not count.
    #[test]
    fn a_replan_may_reopen_a_blocked_task_only_while_its_conflict_stands() {
        let id = TaskId::parse("T002").unwrap();
        assert_eq!(reopen_fix(&id, &[conflict_of("T002")]), StatusFix::Reset);
        let blocked_again = StatusFix::Restore(TaskStatus::Blocked);
        assert_eq!(reopen_fix(&id, &[conflict_of("T003")]), blocked_again);
        assert_eq!(reopen_fix(&id, &[]), blocked_again);
    }

    /// A reopen is refused unless a card still to build amends a check of
    /// its conflict, judged once the statuses a planner may not set are put
    /// right: an amending card written as `done` still counts. A task with
    /// no conflict, or one not set back to `todo`, is not a refused reopen.
    #[test]
    fn a_reopen_is_refused_without_an_amend_still_to_build() {
        let refused = |reopened: &str, amender: Option<TaskCard>, conflict: bool| {
            let cards = [card("T001", "done"), card("T002", reopened)]
                .into_iter()
                .chain(amender)
                .collect();
            Left {
                conflict,
                ..Left::replan(cards)
            }
            .judged()
            .refused_reopens()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
        };
        let other = Some(amends("todo", "T001.other"));
        assert_eq!(
            refused("todo", other, true),
            ["T002"],
            "amends another check"
        );
        assert_eq!(refused("todo", None, false), ["T002"], "no conflict at all");
        let runs = |status| Some(amends(status, "T001.runs"));
        assert!(
            refused("todo", runs("todo"), true).is_empty(),
            "behind its amend"
        );
        assert!(
            refused("todo", runs("done"), true).is_empty(),
            "amend marked done"
        );
        assert!(refused("blocked", None, true).is_empty(), "left blocked");
        assert!(refused("dropped", None, true).is_empty(), "dropped");
    }

    /// Only a replan reopens, and only from `blocked` to `todo`.
    #[test]
    fn only_a_replan_reopens_a_blocked_task() {
        let (blocked, todo) = (card("T002", "blocked"), card("T002", "todo"));
        assert!(reopens(Role::Replan, Some(&blocked), &todo));
        assert!(!reopens(Role::Split, Some(&blocked), &todo));
        assert!(!reopens(
            Role::Replan,
            Some(&card("T002", "dropped")),
            &todo
        ));
        assert!(!reopens(
            Role::Replan,
            Some(&blocked),
            &card("T002", "active")
        ));
        assert!(!reopens(Role::Replan, None, &todo), "a new card");
    }

    /// An old card's status is restored, to what it was, unless the planner
    /// may change it; a new card starts as `todo`.
    #[test]
    fn a_status_a_planner_may_not_set_is_restored() {
        let fix = |from: &str, to: &str| status_fix(Some(&card("T002", from)), &card("T002", to));
        assert_eq!(fix("todo", "dropped"), None);
        assert_eq!(
            fix("todo", "done"),
            Some(StatusFix::Restore(TaskStatus::Todo))
        );
        assert_eq!(
            fix("done", "todo"),
            Some(StatusFix::Restore(TaskStatus::Done))
        );
        assert_eq!(
            fix("blocked", "todo"),
            Some(StatusFix::Restore(TaskStatus::Blocked))
        );
        let new = |status| status_fix(None, &card("T009", status));
        assert_eq!(new("todo"), None);
        assert_eq!(new("done"), Some(StatusFix::Reset));
    }
}
