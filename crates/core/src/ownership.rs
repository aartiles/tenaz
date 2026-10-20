//! Which paths a session of each role may change. Everything else it wrote
//! is reverted after the session, and the next prompt says so.

use crate::project::{BRIEF, GITIGNORE, PROTOCOL};
use crate::role::Role;
use crate::task::{TaskCard, TaskId};

/// Facts about the repository the rules need.
pub trait RepoFacts {
    /// The path exists in HEAD.
    fn in_head(&self, path: &str) -> bool;
    /// The content of the path in HEAD, if any.
    fn head_content(&self, path: &str) -> Option<String>;
}

/// The brief, the protocol, the role prompts, `.tenaz/.gitignore` and the
/// project's own agent instructions are never edited by a session.
pub fn is_protected(path: &str) -> bool {
    [BRIEF, PROTOCOL, GITIGNORE, "AGENTS.md", "CLAUDE.md"].contains(&path)
        || path.starts_with(".tenaz/prompts/")
}

/// Whether `role` may leave a change at `path`.
pub fn allowed(role: Role, path: &str, task: Option<&TaskId>, repo: &dyn RepoFacts) -> bool {
    if is_protected(path) {
        return false;
    }
    let allowed = if let Some(agent_path) = path.strip_prefix(".tenaz/") {
        agent_rule(role, agent_path, task, repo)
    } else {
        matches!(role, Role::Bootstrap | Role::Builder)
    };
    // A done card is history: nobody may rewrite it. Before the bootstrap is
    // accepted no task has been built, so a card a bootstrap session marked
    // done is a mistake it must be able to correct.
    if allowed
        && role != Role::Bootstrap
        && path.starts_with(".tenaz/tasks/")
        && repo
            .head_content(path)
            .is_some_and(|text| TaskCard::text_is_done(&text))
    {
        return false;
    }
    allowed
}

fn agent_rule(role: Role, agent_path: &str, task: Option<&TaskId>, repo: &dyn RepoFacts) -> bool {
    let task_id = task.map_or("", TaskId::as_str);
    match role {
        Role::Bootstrap => {
            agent_path == "verify"
                || agent_path == "spec.md"
                || agent_path == "map.md"
                || agent_path.starts_with("tasks/")
                || agent_path.starts_with("notes/")
        }
        Role::Builder => {
            agent_path == "verify"
                || agent_path == "map.md"
                || agent_path == format!("notes/{task_id}.md")
        }
        Role::Reviewer => agent_path == format!("reviews/{task_id}.md"),
        Role::Split | Role::Replan => {
            agent_path.starts_with("tasks/") || agent_path == "notes/PLANNER.md"
        }
        Role::FinalReviewer => {
            agent_path == "reviews/FINAL.md"
                || (agent_path.starts_with("tasks/")
                    && !repo.in_head(&format!(".tenaz/{agent_path}")))
        }
    }
}

/// Splits changed paths into kept and reverted.
pub fn partition<'a>(
    role: Role,
    paths: impl IntoIterator<Item = &'a str>,
    task: Option<&TaskId>,
    repo: &dyn RepoFacts,
) -> Vec<String> {
    paths
        .into_iter()
        .filter(|p| !allowed(role, p, task, repo))
        .map(str::to_owned)
        .collect()
}

/// The explanation the next session gets when files were reverted.
pub fn reverted_note(role: Role, paths: &[String]) -> String {
    format!(
        "The orchestrator reverted your changes to {}, because a {role} session may not write there. Under .tenaz/ a bootstrap session writes only verify (in an existing project), spec.md, map.md, tasks/ and notes/; .tenaz/brief.md, .tenaz/protocol.md, .tenaz/prompts/ and the project's AGENTS.md and CLAUDE.md are never edited. Put verify and any code it runs in the project itself (for example a checks/ or scripts/ folder at the root), not under .tenaz/.",
        paths.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo {
        head: Vec<(&'static str, &'static str)>,
    }
    impl RepoFacts for Repo {
        fn in_head(&self, path: &str) -> bool {
            self.head.iter().any(|(p, _)| *p == path)
        }
        fn head_content(&self, path: &str) -> Option<String> {
            self.head
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, c)| (*c).to_owned())
        }
    }

    fn repo() -> Repo {
        Repo {
            head: vec![
                (".tenaz/tasks/T001.md", "---\nid: T001\nstatus: done\n---\n"),
                (".tenaz/tasks/T002.md", "---\nid: T002\nstatus: todo\n---\n"),
            ],
        }
    }

    #[test]
    fn protected_files_are_never_allowed() {
        let r = repo();
        for role in Role::ALL {
            for path in [
                ".tenaz/brief.md",
                ".tenaz/protocol.md",
                ".tenaz/.gitignore",
                "AGENTS.md",
                "CLAUDE.md",
            ] {
                assert!(!allowed(role, path, None, &r), "{role} {path}");
            }
            assert!(
                !allowed(role, ".tenaz/prompts/builder.md", None, &r),
                "{role}"
            );
            assert!(
                role == Role::Bootstrap || !allowed(role, ".tenaz/tasks/T001.md", None, &r),
                "{role}: done card"
            );
        }
    }

    #[test]
    fn roles_own_their_files() {
        let r = repo();
        let t = TaskId::parse("T007").unwrap();
        assert!(allowed(Role::Bootstrap, "verify", None, &r));
        assert!(allowed(Role::Bootstrap, ".tenaz/tasks/T002.md", None, &r));
        let marked_done = Repo {
            head: vec![(".tenaz/tasks/T007.md", "---\nid: T007\nstatus: done\n---\n")],
        };
        assert!(
            allowed(Role::Bootstrap, ".tenaz/tasks/T007.md", None, &marked_done),
            "a bootstrap may fix a card an earlier bootstrap marked done"
        );
        assert!(!allowed(
            Role::Replan,
            ".tenaz/tasks/T007.md",
            None,
            &marked_done
        ));
        assert!(!allowed(Role::Bootstrap, ".tenaz/checks/x.sh", None, &r));
        assert!(allowed(Role::Builder, "src/a.js", Some(&t), &r));
        assert!(allowed(Role::Builder, ".tenaz/notes/T007.md", Some(&t), &r));
        assert!(allowed(Role::Builder, ".tenaz/map.md", Some(&t), &r));
        assert!(!allowed(
            Role::Builder,
            ".tenaz/notes/T008.md",
            Some(&t),
            &r
        ));
        assert!(!allowed(
            Role::Builder,
            ".tenaz/tasks/T002.md",
            Some(&t),
            &r
        ));
        assert!(!allowed(Role::Reviewer, "src/a.js", Some(&t), &r));
        assert!(allowed(
            Role::Reviewer,
            ".tenaz/reviews/T007.md",
            Some(&t),
            &r
        ));
        assert!(allowed(Role::Split, ".tenaz/tasks/T007a.md", Some(&t), &r));
        assert!(allowed(Role::Replan, ".tenaz/notes/PLANNER.md", None, &r));
        assert!(!allowed(Role::Replan, ".tenaz/notes/T001.md", None, &r));
        assert!(allowed(
            Role::FinalReviewer,
            ".tenaz/reviews/FINAL.md",
            None,
            &r
        ));
        assert!(
            allowed(Role::FinalReviewer, ".tenaz/tasks/T009.md", None, &r),
            "new card"
        );
        assert!(
            !allowed(Role::FinalReviewer, ".tenaz/tasks/T002.md", None, &r),
            "existing card"
        );
    }

    #[test]
    fn partition_returns_what_to_revert_and_the_note_names_it() {
        let r = repo();
        let reverted = partition(
            Role::Bootstrap,
            ["verify", ".tenaz/brief.md", ".tenaz/checks/a.sh"],
            None,
            &r,
        );
        assert_eq!(reverted, vec![".tenaz/brief.md", ".tenaz/checks/a.sh"]);
        let note = reverted_note(Role::Bootstrap, &reverted);
        assert!(note.starts_with("The orchestrator reverted your changes to .tenaz/brief.md, .tenaz/checks/a.sh, because a bootstrap session may not write there."));
    }
}
