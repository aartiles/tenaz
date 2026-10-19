//! The backlog: every card under `.tenaz/tasks/`, validated as a whole
//! (references, cycles), with the scheduling rules of the task loop.

use std::collections::BTreeMap;

use crate::spec::Requirement;
use crate::task::{TaskCard, TaskId, TaskStatus};

/// The cards of a project, keyed by ID (the ID order is the build order).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Backlog {
    tasks: BTreeMap<TaskId, TaskCard>,
}

/// A backlog read from disk and the problems found in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadedBacklog {
    pub backlog: Backlog,
    pub errors: Vec<String>,
}

impl Backlog {
    /// Parses `(file name, text)` pairs from `.tenaz/tasks/` and validates
    /// references against the spec and each other. Unreadable cards are
    /// reported and skipped.
    pub fn load<'a>(
        cards: impl IntoIterator<Item = (&'a str, &'a str)>,
        reqs: &[Requirement],
    ) -> LoadedBacklog {
        let mut tasks = BTreeMap::new();
        let mut errors = Vec::new();
        for (file, text) in cards {
            match TaskCard::parse(file, text) {
                Ok(card) => {
                    tasks.insert(card.id.clone(), card);
                }
                Err(error) => errors.push(format!(".tenaz/tasks/{file}: {error}")),
            }
        }
        for task in tasks.values() {
            for dep in &task.depends {
                if !tasks.contains_key(dep) {
                    errors.push(format!("{}: depends on missing {dep}", task.id));
                }
            }
            for r in &task.requires {
                if !reqs.iter().any(|req| &req.id == r) {
                    errors.push(format!("{}: requires unknown {r}", task.id));
                }
            }
            if let Some(parent) = &task.parent
                && !tasks.contains_key(parent)
            {
                errors.push(format!("{}: parent {parent} does not exist", task.id));
            }
        }
        let backlog = Backlog { tasks };
        errors.extend(backlog.cycles());
        LoadedBacklog { backlog, errors }
    }

    pub fn from_cards(cards: impl IntoIterator<Item = TaskCard>) -> Self {
        Self {
            tasks: cards.into_iter().map(|c| (c.id.clone(), c)).collect(),
        }
    }

    pub fn get(&self, id: &TaskId) -> Option<&TaskCard> {
        self.tasks.get(id)
    }

    pub fn contains(&self, id: &TaskId) -> bool {
        self.tasks.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn ids(&self) -> impl Iterator<Item = &TaskId> {
        self.tasks.keys()
    }

    pub fn cards(&self) -> impl Iterator<Item = &TaskCard> + Clone {
        self.tasks.values()
    }

    pub fn children(&self, parent: &TaskId) -> Vec<&TaskCard> {
        self.tasks
            .values()
            .filter(|t| t.parent.as_ref() == Some(parent))
            .collect()
    }

    /// Dependency cycles, as `dependency cycle: A -> B -> A` messages.
    fn cycles(&self) -> Vec<String> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Open,
            Done,
        }
        fn visit(
            backlog: &Backlog,
            id: &TaskId,
            trail: &mut Vec<TaskId>,
            marks: &mut BTreeMap<TaskId, Mark>,
            errors: &mut Vec<String>,
        ) {
            let Some(task) = backlog.tasks.get(id) else {
                return;
            };
            match marks.get(id) {
                Some(Mark::Done) => return,
                Some(Mark::Open) => {
                    let mut cycle: Vec<&str> = trail.iter().map(TaskId::as_str).collect();
                    cycle.push(id.as_str());
                    errors.push(format!("dependency cycle: {}", cycle.join(" -> ")));
                    return;
                }
                None => {}
            }
            marks.insert(id.clone(), Mark::Open);
            trail.push(id.clone());
            for dep in &task.depends {
                visit(backlog, dep, trail, marks, errors);
            }
            trail.pop();
            marks.insert(id.clone(), Mark::Done);
        }
        let mut errors = Vec::new();
        let mut marks = BTreeMap::new();
        for id in self.tasks.keys() {
            visit(self, id, &mut Vec::new(), &mut marks, &mut errors);
        }
        errors
    }

    /// A task is resolved when it is done or dropped, or when it was split and
    /// every child is resolved.
    pub fn resolved(&self, id: &TaskId) -> bool {
        self.resolved_seen(id, &mut Vec::new())
    }

    fn resolved_seen(&self, id: &TaskId, seen: &mut Vec<TaskId>) -> bool {
        let Some(task) = self.tasks.get(id) else {
            return false;
        };
        if seen.contains(id) {
            return false;
        }
        seen.push(id.clone());
        match task.status {
            TaskStatus::Done | TaskStatus::Dropped => true,
            TaskStatus::Split => {
                let children = self.children(id);
                !children.is_empty() && children.iter().all(|c| self.resolved_seen(&c.id, seen))
            }
            _ => false,
        }
    }

    /// Tasks that can be built now: `todo` or `active` with every dependency
    /// resolved. The active task comes first, then the lowest ID.
    pub fn ready(&self) -> Vec<&TaskCard> {
        let mut ready: Vec<&TaskCard> = self
            .tasks
            .values()
            .filter(|t| matches!(t.status, TaskStatus::Todo | TaskStatus::Active))
            .filter(|t| t.depends.iter().all(|dep| self.resolved(dep)))
            .collect();
        ready.sort_by_key(|t| (t.status != TaskStatus::Active, t.id.clone()));
        ready
    }

    /// Tasks that are neither resolved nor split: still open, even if blocked.
    pub fn open(&self) -> Vec<&TaskCard> {
        self.tasks
            .values()
            .filter(|t| !self.resolved(&t.id) && t.status != TaskStatus::Split)
            .collect()
    }

    /// `T007 [todo, 2 attempts] title (depends T003) (parent T006)` per line.
    pub fn table(&self) -> String {
        self.tasks
            .values()
            .map(|t| {
                let attempts = if t.attempts > 0 {
                    format!(", {} attempts", t.attempts)
                } else {
                    String::new()
                };
                let depends = if t.depends.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (depends {})",
                        t.depends
                            .iter()
                            .map(TaskId::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let parent = t
                    .parent
                    .as_ref()
                    .map(|p| format!(" (parent {p})"))
                    .unwrap_or_default();
                format!(
                    "{} [{}{}] {}{}{}",
                    t.id, t.status, attempts, t.title, depends, parent
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::parse_requirements;

    fn reqs() -> Vec<Requirement> {
        parse_requirements("- R1: a\n- R2: b\n- R3: c\n")
    }

    fn text(id: &str, status: &str, depends: &[&str], parent: Option<&str>) -> String {
        format!(
            "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\nrequires: [R1]\ndepends: [{}]\n{}---\n",
            depends.join(", "),
            parent.map(|p| format!("parent: {p}\n")).unwrap_or_default()
        )
    }

    #[test]
    fn loads_and_reports_reference_errors() {
        let a = text("T001", "todo", &["T009"], None);
        let b = "---\nid: T002\nrequires: [R7]\nparent: T050\n---\n".to_owned();
        let bad = "nope".to_owned();
        let loaded = Backlog::load(
            [
                ("T001.md", a.as_str()),
                ("T002.md", b.as_str()),
                ("T003.md", bad.as_str()),
            ],
            &reqs(),
        );
        assert_eq!(loaded.backlog.len(), 2);
        assert_eq!(
            loaded.errors,
            vec![
                ".tenaz/tasks/T003.md: missing --- frontmatter ---",
                "T001: depends on missing T009",
                "T002: requires unknown R7",
                "T002: parent T050 does not exist",
            ]
        );
    }

    #[test]
    fn detects_dependency_cycles() {
        let a = text("T001", "todo", &["T002"], None);
        let b = text("T002", "todo", &["T001"], None);
        let loaded = Backlog::load([("T001.md", a.as_str()), ("T002.md", b.as_str())], &reqs());
        assert_eq!(
            loaded.errors,
            vec!["dependency cycle: T001 -> T002 -> T001"]
        );
    }

    #[test]
    fn ready_orders_active_first_then_by_id_and_waits_for_dependencies() {
        let cards = [
            text("T001", "done", &[], None),
            text("T002", "todo", &["T001"], None),
            text("T003", "active", &["T001"], None),
            text("T004", "todo", &["T002"], None),
            text("T005", "blocked", &[], None),
        ];
        let files: Vec<String> = (1..=5).map(|n| format!("T00{n}.md")).collect();
        let loaded = Backlog::load(
            files
                .iter()
                .map(String::as_str)
                .zip(cards.iter().map(String::as_str)),
            &reqs(),
        );
        assert_eq!(loaded.errors, Vec::<String>::new());
        let ready: Vec<&str> = loaded
            .backlog
            .ready()
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(ready, vec!["T003", "T002"]);
        let open: Vec<&str> = loaded
            .backlog
            .open()
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(open, vec!["T002", "T003", "T004", "T005"]);
    }

    #[test]
    fn split_task_resolves_through_its_children() {
        let cards = [
            text("T001", "split", &[], None),
            text("T001a", "done", &[], Some("T001")),
            text("T001b", "todo", &["T001a"], Some("T001")),
            text("T002", "todo", &["T001"], None),
        ];
        let files = ["T001.md", "T001a.md", "T001b.md", "T002.md"];
        let loaded = Backlog::load(
            files.into_iter().zip(cards.iter().map(String::as_str)),
            &reqs(),
        );
        let b = &loaded.backlog;
        let id = |s: &str| TaskId::parse(s).unwrap();
        assert!(!b.resolved(&id("T001")));
        assert_eq!(
            b.ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["T001b"]
        );
        let done = Backlog::from_cards(b.cards().cloned().map(|mut c| {
            if c.id.as_str() == "T001b" {
                c.status = TaskStatus::Done;
            }
            c
        }));
        assert!(done.resolved(&id("T001")));
        assert_eq!(
            done.ready()
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            vec!["T002"]
        );
        assert!(!done.resolved(&id("T999")));
    }

    #[test]
    fn table_lists_status_attempts_and_links() {
        let mut card =
            TaskCard::parse("T002.md", &text("T002", "todo", &["T001"], Some("T000"))).unwrap();
        card.attempts = 2;
        let b = Backlog::from_cards([card]);
        assert_eq!(
            b.table(),
            "T002 [todo, 2 attempts] Task T002 (depends T001) (parent T000)"
        );
    }
}
