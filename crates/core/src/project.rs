//! The project on disk: `verify` and everything Tenaz keeps under `.tenaz/`
//! (the brief, the protocol, the role prompts and the run's state). Outside
//! `.tenaz/` Tenaz writes only what the sessions build.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::backlog::{Backlog, LoadedBacklog};
use crate::ledger::{JournalEntry, JournalRecord, Ledger};
use crate::role::Role;
use crate::spec::{Requirement, parse_requirements};
use crate::task::{TaskId, set_card_fields};

/// The original request, copied at `init`.
pub const BRIEF: &str = ".tenaz/brief.md";
/// The working protocol every briefing starts with.
pub const PROTOCOL: &str = ".tenaz/protocol.md";
/// Keeps the run's private files out of git without touching the project's
/// own `.gitignore`.
pub const GITIGNORE: &str = ".tenaz/.gitignore";

/// Held by a run while it works; dropping it releases the lock.
#[derive(Debug)]
pub struct RunLock {
    _file: fs::File,
}

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

    pub fn tenaz_dir(&self) -> PathBuf {
        self.root.join(".tenaz")
    }

    /// What to do when the project still keeps its state in `.agent/`,
    /// where builds before the rename to `.tenaz/` put it; `None` otherwise.
    pub fn legacy_layout_note(&self) -> Option<String> {
        if self.exists(".tenaz/run.json") {
            return self.root_layout_note();
        }
        if !self.exists(".agent/run.json") {
            return None;
        }
        let root = self.root.display();
        Some(format!(
            "{root} keeps its Tenaz state in .agent/, the layout of earlier builds; this \
             version reads .tenaz/. To move it:\n  \
             mv '{root}/.agent' '{root}/.tenaz'\n  \
             change .agent/ to .tenaz/ in '{root}/.gitignore'\n  \
             tenaz run --project '{root}' --refresh-templates\n\
             The last step rewrites the protocol and the prompts with the new paths. \
             Pass the launch flags (--engine, --comfyui, ...) to that run once; they \
             are saved in run.json."
        ))
    }

    /// What to do when a run started before the brief and the protocol moved
    /// into `.tenaz/` still has `BRIEF.md` at the root; `None` otherwise.
    fn root_layout_note(&self) -> Option<String> {
        if self.exists(BRIEF) || !self.exists("BRIEF.md") {
            return None;
        }
        let root = self.root.display();
        Some(format!(
            "{root} keeps BRIEF.md and AGENTS.md at the root, the layout of earlier builds; \
             this version keeps them under .tenaz/ and leaves the project's own AGENTS.md \
             alone. To move it:\n  \
             git -C '{root}' mv BRIEF.md .tenaz/brief.md\n  \
             git -C '{root}' rm -q AGENTS.md   (or put your own AGENTS.md back there)\n  \
             git -C '{root}' commit -qm 'tenaz: move the brief into .tenaz/'\n  \
             tenaz run --project '{root}' --refresh-templates\n\
             The last step writes .tenaz/protocol.md, the prompts and .tenaz/.gitignore. \
             The .tenaz/ lines in the project's .gitignore can then go."
        ))
    }

    pub fn private_dir(&self) -> PathBuf {
        self.tenaz_dir().join("private")
    }

    fn run_lock_path(&self) -> PathBuf {
        self.private_dir().join("run.lock")
    }

    /// Takes the lock a run holds while it works on the project, or `None`
    /// when another process holds it. The OS releases it when the process
    /// ends, however it ends.
    pub fn lock_run(&self) -> io::Result<Option<RunLock>> {
        fs::create_dir_all(self.private_dir())?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.run_lock_path())?;
        // `active_run` holds a shared lock for an instant: retry past it.
        for attempt in 0..10 {
            match file.try_lock() {
                Ok(()) => {
                    use std::io::Write as _;
                    file.set_len(0)?;
                    write!(file, "{}", std::process::id())?;
                    return Ok(Some(RunLock { _file: file }));
                }
                Err(fs::TryLockError::WouldBlock) if attempt < 9 => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(fs::TryLockError::WouldBlock) => return Ok(None),
                Err(fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        Ok(None)
    }

    /// The process id of the run working on the project, or `None` when no
    /// run holds the lock.
    pub fn active_run(&self) -> Option<u32> {
        let file = fs::File::open(self.run_lock_path()).ok()?;
        match file.try_lock_shared() {
            Err(fs::TryLockError::WouldBlock) => Some(
                fs::read_to_string(self.run_lock_path())
                    .ok()
                    .and_then(|t| t.trim().parse().ok())
                    .unwrap_or(0),
            ),
            _ => None,
        }
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.tenaz_dir().join("run.json")
    }

    pub fn journal_path(&self) -> PathBuf {
        self.tenaz_dir().join("journal.jsonl")
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.tenaz_dir().join("tasks")
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Creates the `.tenaz/` layout.
    pub fn create_layout(&self) -> io::Result<()> {
        for dir in [
            "prompts",
            "tasks",
            "notes",
            "reviews",
            "private",
            "private/sessions",
        ] {
            fs::create_dir_all(self.tenaz_dir().join(dir))?;
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
        self.write_atomic(".tenaz/run.json", &format!("{text}\n"))
    }

    pub fn append_journal(&self, at: String, record: JournalRecord) -> io::Result<()> {
        let entry = JournalEntry { at, record };
        let line = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.append(".tenaz/journal.jsonl", &format!("{line}\n"))
    }

    /// Every journal line that parses.
    pub fn journal(&self) -> Vec<JournalEntry> {
        self.read_if(".tenaz/journal.jsonl")
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    // ---------- spec, map, cards, notes, prompts ----------

    pub fn requirements(&self) -> Vec<Requirement> {
        parse_requirements(&self.read_if(".tenaz/spec.md").unwrap_or_default())
    }

    /// Reads every `.md` card under `.tenaz/tasks/`.
    /// Rewrites the task cards a model wrapped in a code fence without it,
    /// and names them.
    pub fn unfence_cards(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.tasks_dir()) else {
            return Vec::new();
        };
        let mut fixed = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "md") {
                continue;
            }
            if let Some(text) = fs::read_to_string(&path)
                .ok()
                .and_then(|t| crate::task::frontmatter::unfence(&t))
                && fs::write(&path, text).is_ok()
            {
                fixed.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        fixed.sort();
        fixed
    }

    pub fn load_backlog(&self) -> LoadedBacklog {
        let dir = self.tasks_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return LoadedBacklog {
                backlog: Backlog::default(),
                errors: vec![".tenaz/tasks/ does not exist".to_owned()],
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
        self.read_if(&format!(".tenaz/notes/{id}.md"))
    }

    pub fn role_prompt(&self, role: Role) -> io::Result<String> {
        self.read(&role.prompt_path())
    }

    pub fn brief(&self) -> io::Result<String> {
        self.read(BRIEF)
    }

    pub fn protocol(&self) -> io::Result<String> {
        self.read(PROTOCOL)
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

    /// Adds the orchestrator's private paths to `.tenaz/.gitignore` when
    /// missing. The project's own `.gitignore` is not touched.
    pub fn ensure_gitignore(&self) -> io::Result<()> {
        let wanted = ["/private/", "/run.json", "/journal.jsonl", "/skills/"];
        let current = self.read_if(GITIGNORE).unwrap_or_default();
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
        self.write(GITIGNORE, &text)
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
    fn a_project_in_the_old_agent_layout_gets_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let p = Project::new(dir.path());
        assert_eq!(p.legacy_layout_note(), None);
        p.write(".agent/run.json", "{}").unwrap();
        let note = p.legacy_layout_note().unwrap();
        assert!(note.contains("mv '"), "{note}");
        assert!(note.contains("--refresh-templates"), "{note}");
        p.write(".tenaz/run.json", "{}").unwrap();
        assert_eq!(p.legacy_layout_note(), None);
    }

    #[test]
    fn cards_and_fields_round_trip_through_disk() {
        let (_dir, p) = project();
        p.write(".tenaz/spec.md", "- R1: a\n").unwrap();
        p.write(
            ".tenaz/tasks/T001.md",
            "---\nid: T001\nstatus: todo\nrequires: [R1]\n---\n## Goal\n",
        )
        .unwrap();
        p.write(".tenaz/tasks/notes.txt", "ignored").unwrap();
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
        p.write(".tenaz/notes/T001.md", "did it").unwrap();
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
            "/.tenaz/journal.jsonl".trim_start_matches('/'),
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
            ".tenaz/reviews/T001.md",
            "notes\nVerdict: CHANGES\n- [MAJOR] x\n",
        )
        .unwrap();
        assert_eq!(
            p.verdict(".tenaz/reviews/T001.md").0.as_deref(),
            Some("CHANGES")
        );
        p.write(".tenaz/reviews/T001.md", "Verdict: ACCEPTED?\n")
            .unwrap();
        assert_eq!(p.verdict(".tenaz/reviews/T001.md").0, None);
        assert_eq!(p.verdict(".tenaz/reviews/none.md"), (None, None));
    }

    #[test]
    fn gitignore_gets_only_the_missing_lines() {
        let (_dir, p) = project();
        p.write(".gitignore", "node_modules\n").unwrap();
        p.write(GITIGNORE, "# mine\n/run.json").unwrap();
        p.ensure_gitignore().unwrap();
        assert_eq!(
            p.read(GITIGNORE).unwrap(),
            "# mine\n/run.json\n/private/\n/journal.jsonl\n/skills/\n"
        );
        p.ensure_gitignore().unwrap();
        assert_eq!(p.read(GITIGNORE).unwrap().lines().count(), 5);
        assert_eq!(p.read(".gitignore").unwrap(), "node_modules\n");
    }

    #[test]
    fn a_run_with_the_brief_at_the_root_gets_a_note() {
        let (_dir, p) = project();
        p.write(".tenaz/run.json", "{}").unwrap();
        assert_eq!(p.legacy_layout_note(), None);
        p.write("BRIEF.md", "# Goal\n").unwrap();
        let note = p.legacy_layout_note().unwrap();
        assert!(note.contains("mv BRIEF.md .tenaz/brief.md"), "{note}");
        assert!(note.contains("--refresh-templates"), "{note}");
        p.write(BRIEF, "# Goal\n").unwrap();
        assert_eq!(p.legacy_layout_note(), None);
    }

    #[test]
    fn atomic_write_replaces_content() {
        let (_dir, p) = project();
        p.write_atomic(".tenaz/run.json", "{}\n").unwrap();
        p.write_atomic(".tenaz/run.json", "{\"a\":1}\n").unwrap();
        assert_eq!(p.read(".tenaz/run.json").unwrap(), "{\"a\":1}\n");
        assert!(!p.exists(&format!(".tenaz/run.{}.tmp", std::process::id())));
    }

    #[test]
    fn the_run_lock_is_held_once_and_names_its_process() {
        let (_dir, p) = project();
        assert_eq!(p.active_run(), None);
        let lock = p.lock_run().unwrap().expect("the first run takes the lock");
        assert_eq!(p.active_run(), Some(std::process::id()));
        assert!(p.lock_run().unwrap().is_none(), "a second run got the lock");
        drop(lock);
        assert_eq!(p.active_run(), None);
        assert!(p.lock_run().unwrap().is_some());
    }
}
