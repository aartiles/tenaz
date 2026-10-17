//! The project on disk: `BRIEF.md`, `AGENTS.md`, `verify`, and the
//! orchestration state under `.agent/`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::backlog::{Backlog, LoadedBacklog};
use crate::ledger::{JournalEntry, JournalRecord, Ledger};
use crate::role::Role;
use crate::spec::{Requirement, parse_requirements};
use crate::task::{TaskId, set_card_fields};

/// Paths and file access for one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    root: PathBuf,
}

impl Project {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn agent_dir(&self) -> PathBuf {
        self.root.join(".agent")
    }

    pub fn private_dir(&self) -> PathBuf {
        self.agent_dir().join("private")
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.agent_dir().join("run.json")
    }

    pub fn journal_path(&self) -> PathBuf {
        self.agent_dir().join("journal.jsonl")
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.agent_dir().join("tasks")
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Creates the `.agent/` layout.
    pub fn create_layout(&self) -> io::Result<()> {
        for dir in [
            "prompts",
            "tasks",
            "notes",
            "reviews",
            "private",
            "private/sessions",
        ] {
            fs::create_dir_all(self.agent_dir().join(dir))?;
        }
        Ok(())
    }

    pub fn read(&self, relative: &str) -> io::Result<String> {
        fs::read_to_string(self.path(relative))
    }

    pub fn read_if(&self, relative: &str) -> Option<String> {
        self.read(relative).ok()
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.path(relative).exists()
    }

    pub fn write(&self, relative: &str, text: &str) -> io::Result<()> {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)
    }

    pub fn remove(&self, relative: &str) -> io::Result<()> {
        match fs::remove_file(self.path(relative)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    pub fn append(&self, relative: &str, text: &str) -> io::Result<()> {
        use io::Write as _;
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(text.as_bytes())
    }

    /// Writes through a temporary file so a crash leaves the old content.
    pub fn write_atomic(&self, relative: &str, text: &str) -> io::Result<()> {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temp, text)?;
        fs::rename(temp, path)
    }

    // ---------- ledger and journal ----------

    pub fn load_ledger(&self) -> io::Result<Ledger> {
        let text = fs::read_to_string(self.ledger_path())?;
        serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("run.json: {e}")))
    }

    pub fn save_ledger(&self, ledger: &Ledger) -> io::Result<()> {
        let text = serde_json::to_string_pretty(ledger)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.write_atomic(".agent/run.json", &format!("{text}\n"))
    }

    pub fn append_journal(&self, at: String, record: JournalRecord) -> io::Result<()> {
        let entry = JournalEntry { at, record };
        let line = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.append(".agent/journal.jsonl", &format!("{line}\n"))
    }

    /// Every journal line that parses.
    pub fn journal(&self) -> Vec<JournalEntry> {
        self.read_if(".agent/journal.jsonl")
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    // ---------- spec, map, cards, notes, prompts ----------

    pub fn requirements(&self) -> Vec<Requirement> {
        parse_requirements(&self.read_if(".agent/spec.md").unwrap_or_default())
    }

    /// Reads every `.md` card under `.agent/tasks/`.
    pub fn load_backlog(&self) -> LoadedBacklog {
        let dir = self.tasks_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return LoadedBacklog {
                backlog: Backlog::default(),
                errors: vec![".agent/tasks/ does not exist".to_owned()],
            };
        };
        let mut files: Vec<(String, String)> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                fs::read_to_string(e.path()).ok().map(|text| (name, text))
            })
            .collect();
        files.sort();
        Backlog::load(
            files.iter().map(|(n, t)| (n.as_str(), t.as_str())),
            &self.requirements(),
        )
    }

    pub fn card_text(&self, id: &TaskId) -> io::Result<String> {
        self.read(&id.card_path())
    }

    pub fn set_card_fields(&self, id: &TaskId, fields: &[(&str, String)]) -> io::Result<()> {
        let text = self.card_text(id)?;
        self.write(&id.card_path(), &set_card_fields(&text, fields))
    }

    pub fn note(&self, id: &str) -> Option<String> {
        self.read_if(&format!(".agent/notes/{id}.md"))
    }

    pub fn role_prompt(&self, role: Role) -> io::Result<String> {
        self.read(&role.prompt_path())
    }

    /// `(verdict, text)` of a review file: the first `Verdict: ACCEPT|CHANGES` line.
    pub fn verdict(&self, relative: &str) -> (Option<String>, Option<String>) {
        let text = self.read_if(relative);
        let verdict = text.as_deref().and_then(|t| {
            t.lines().find_map(|line| {
                let rest = line.trim_start().strip_prefix("Verdict:")?.trim_start();
                ["ACCEPT", "CHANGES"]
                    .into_iter()
                    .find(|v| {
                        rest.starts_with(v)
                            && !rest[v.len()..].starts_with(|c: char| c.is_alphanumeric())
                    })
                    .map(str::to_owned)
            })
        });
        (verdict, text)
    }

    /// Adds the orchestrator's private paths to `.gitignore` when missing.
    pub fn ensure_gitignore(&self) -> io::Result<()> {
        let wanted = [
            ".agent/private/",
            ".agent/run.json",
            ".agent/journal.jsonl",
            ".agent/skills/",
        ];
        let current = self.read_if(".gitignore").unwrap_or_default();
        let missing: Vec<&str> = wanted
            .into_iter()
            .filter(|line| !current.lines().any(|l| l == *line))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut text = current.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&missing.join("\n"));
        text.push('\n');
        self.write(".gitignore", &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        let p = Project::new(dir.path());
        p.create_layout().unwrap();
        (dir, p)
    }

    #[test]
    fn cards_and_fields_round_trip_through_disk() {
        let (_dir, p) = project();
        p.write(".agent/spec.md", "- R1: a\n").unwrap();
        p.write(
            ".agent/tasks/T001.md",
            "---\nid: T001\nstatus: todo\nrequires: [R1]\n---\n## Goal\n",
        )
        .unwrap();
        p.write(".agent/tasks/notes.txt", "ignored").unwrap();
        let loaded = p.load_backlog();
        assert_eq!(loaded.errors, Vec::<String>::new());
        let id = TaskId::parse("T001").unwrap();
        p.set_card_fields(&id, &[("status", "done".into())])
            .unwrap();
        assert_eq!(
            p.load_backlog().backlog.get(&id).unwrap().status,
            crate::task::TaskStatus::Done
        );
        assert_eq!(p.note("T001"), None);
        p.write(".agent/notes/T001.md", "did it").unwrap();
        assert_eq!(p.note("T001").as_deref(), Some("did it"));
    }

    #[test]
    fn journal_appends_and_reads_entries() {
        let (_dir, p) = project();
        p.append_journal(
            "t1".into(),
            JournalRecord::Run {
                outcome: "COMPLETE".into(),
                reason: None,
            },
        )
        .unwrap();
        p.append(
            "/.agent/journal.jsonl".trim_start_matches('/'),
            "not json\n",
        )
        .unwrap();
        p.append_journal(
            "t2".into(),
            JournalRecord::SkillChanged { skill: "s".into() },
        )
        .unwrap();
        let entries = p.journal();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].at, "t2");
    }

    #[test]
    fn verdict_reads_the_first_matching_line() {
        let (_dir, p) = project();
        p.write(
            ".agent/reviews/T001.md",
            "notes\nVerdict: CHANGES\n- [MAJOR] x\n",
        )
        .unwrap();
        assert_eq!(
            p.verdict(".agent/reviews/T001.md").0.as_deref(),
            Some("CHANGES")
        );
        p.write(".agent/reviews/T001.md", "Verdict: ACCEPTED?\n")
            .unwrap();
        assert_eq!(p.verdict(".agent/reviews/T001.md").0, None);
        assert_eq!(p.verdict(".agent/reviews/none.md"), (None, None));
    }

    #[test]
    fn gitignore_gets_only_the_missing_lines() {
        let (_dir, p) = project();
        p.write(".gitignore", "node_modules\n.agent/run.json")
            .unwrap();
        p.ensure_gitignore().unwrap();
        assert_eq!(
            p.read(".gitignore").unwrap(),
            "node_modules\n.agent/run.json\n.agent/private/\n.agent/journal.jsonl\n.agent/skills/\n"
        );
        p.ensure_gitignore().unwrap();
        assert_eq!(p.read(".gitignore").unwrap().lines().count(), 5);
    }

    #[test]
    fn atomic_write_replaces_content() {
        let (_dir, p) = project();
        p.write_atomic(".agent/run.json", "{}\n").unwrap();
        p.write_atomic(".agent/run.json", "{\"a\":1}\n").unwrap();
        assert_eq!(p.read(".agent/run.json").unwrap(), "{\"a\":1}\n");
        assert!(!p.exists(&format!(".agent/run.{}.tmp", std::process::id())));
    }
}
