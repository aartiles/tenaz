//! Running on an existing repository without touching the user's checkout.
//!
//! `tenaz init` on a repository with commits does not work on the current
//! branch: it creates a branch `tenaz/<slug>` from HEAD, checks it out in a
//! worktree next to the repository (`../<repo>-tenaz-<slug>`) and starts the
//! run there. A sibling directory, rather than one inside `.git/`, so it is
//! easy to find, open in an editor and run `verify` in, and so the coding
//! agent sees an ordinary checkout. `--in-place` keeps the old behaviour.
//!
//! `tenaz finish` turns the run into a deliverable: one commit on top of
//! the commit the run started from, with the project's changes and none of
//! Tenaz's own files, on a branch of its own. The run branch keeps the full
//! history.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use tenaz_adapters::git::{GitRepo, TreeEdits};
use tenaz_core::ledger::{Ledger, Origin};
use tenaz_core::ports::Vcs as _;
use tenaz_core::project::{BRIEF, Project};

/// Where Tenaz writes the run's state; never part of a deliverable.
const STATE_DIR: &str = ".tenaz";
/// Files older layouts wrote at the project root (now under `.tenaz/`).
const OWNED_FILES: [&str; 2] = ["BRIEF.md", "AGENTS.md"];
/// The message of the commit `tenaz init` makes.
const INIT_MESSAGE: &str = "tenaz: init";
/// The tree with nothing in it.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// ---------- pure parts ----------

/// The line that names the goal: the first heading when it says more than
/// one word (`# Goal` alone does not), else the first line of text.
pub fn goal_line(brief: &str) -> Option<String> {
    let mut first_text = None;
    for line in brief.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(heading) = line.strip_prefix('#') {
            let heading = heading.trim_start_matches('#').trim();
            if heading.split_whitespace().count() > 1 {
                return Some(heading.to_owned());
            }
        } else if first_text.is_none() {
            first_text = Some(line.to_owned());
        }
    }
    first_text
}

/// A branch-safe name from the first words of `text`: lowercase ASCII
/// letters and digits joined by `-`, at most 40 characters; `run` when
/// nothing is left.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(5)
    {
        let next = usize::from(!slug.is_empty()) + word.len();
        if slug.len() + next > 40 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.to_ascii_lowercase());
    }
    if slug.is_empty() {
        "run".to_owned()
    } else {
        slug
    }
}

/// `../<repo>-tenaz-<slug>` next to the repository at `toplevel`.
pub fn worktree_dir(toplevel: &Path, slug: &str) -> PathBuf {
    let name = toplevel.file_name().map_or_else(
        || "project".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    toplevel
        .parent()
        .unwrap_or(toplevel)
        .join(format!("{name}-tenaz-{slug}"))
}

/// The `git status --porcelain` lines that stop a worktree run: everything
/// except an untracked brief (`.tenaz/brief.md`) when it is to be the brief.
pub fn blocking_changes(status: &[String], brief_is_untracked_ok: bool) -> Vec<String> {
    let untracked_brief = format!("?? {BRIEF}");
    status
        .iter()
        .filter(|l| !(brief_is_untracked_ok && **l == untracked_brief))
        .cloned()
        .collect()
}

/// `current` without the lines `init` added: those in the init commit's
/// version that the base did not have. `None` when nothing is left and the
/// base had no such file.
pub fn strip_added_lines(current: &str, base: Option<&str>, init: &str) -> Option<String> {
    let base_lines: Vec<&str> = base.map(|b| b.lines().collect()).unwrap_or_default();
    let added: Vec<&str> = init
        .lines()
        .filter(|l| !l.trim().is_empty() && !base_lines.contains(l))
        .collect();
    let kept: Vec<&str> = current.lines().filter(|l| !added.contains(l)).collect();
    if base.is_none() && kept.iter().all(|l| l.trim().is_empty()) {
        return None;
    }
    let mut text = kept.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    Some(text)
}

/// The deliverable's commit message: the goal as the subject, the brief's
/// first paragraph when it says more, and where the history is.
pub fn finish_message(brief: &str, run_branch: Option<&str>, commits: u64) -> String {
    let goal = goal_line(brief).unwrap_or_else(|| "Result of a Tenaz run".to_owned());
    let mut subject: String = goal.trim_end_matches('.').chars().take(72).collect();
    if subject.chars().count() < goal.trim_end_matches('.').chars().count() {
        subject = format!("{}...", subject.chars().take(69).collect::<String>());
    }
    let paragraph: Vec<&str> = brief
        .lines()
        .map(str::trim)
        .skip_while(|l| l.is_empty() || l.starts_with('#'))
        .take_while(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let paragraph = paragraph.join("\n");
    let mut message = format!("{subject}\n");
    if !paragraph.is_empty() && paragraph != goal {
        let _ = write!(message, "\n{paragraph}\n");
    }
    let history = run_branch.map_or_else(String::new, |b| format!(" on branch {b}"));
    let _ = write!(
        message,
        "\nSquashed from {commits} commit{} of a Tenaz run{history}, \
         without the run's own files ({STATE_DIR}/).\n",
        if commits == 1 { "" } else { "s" }
    );
    message
}

/// A directory with nothing in it but, perhaps, the brief: a new project,
/// whatever repository it is in.
fn is_fresh_dir(dir: &Path) -> bool {
    let only = |dir: &Path, name: &str| match std::fs::read_dir(dir) {
        Err(_) => true,
        Ok(entries) => entries
            .filter_map(Result::ok)
            .all(|e| e.file_name() == name),
    };
    let (state, file) = BRIEF.split_once('/').unwrap_or((BRIEF, ""));
    only(dir, state) && only(&dir.join(state), file)
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

// ---------- init ----------

/// What the user asked for about the run's place.
#[derive(Clone, Debug, Default)]
pub struct PlaceRequest {
    pub in_place: bool,
    /// Overrides `../<repo>-tenaz-<slug>`.
    pub worktree: Option<PathBuf>,
    /// Overrides the slug taken from the goal.
    pub name: Option<String>,
}

/// Where `init` puts the run.
#[derive(Clone, Debug)]
pub enum Placement {
    /// A new project, or not a repository with commits: as before.
    Fresh,
    /// The user's checkout, on its current branch (`--in-place`).
    InPlace { origin: Origin },
    /// A new branch in a new worktree.
    Worktree(WorktreePlan),
}

#[derive(Clone, Debug)]
pub struct WorktreePlan {
    pub repo: PathBuf,
    pub base_branch: Option<String>,
    pub base_commit: String,
    pub branch: String,
    pub path: PathBuf,
    /// An untracked brief in the user's checkout, to copy in.
    pub untracked_brief: Option<PathBuf>,
}

impl Placement {
    /// The directory `init` writes the project into.
    pub fn project_dir<'a>(&'a self, given: &'a Path) -> &'a Path {
        match self {
            Placement::Worktree(plan) => &plan.path,
            _ => given,
        }
    }
}

/// The repository with commits whose top is `dir`, if any, and whether
/// `dir` is instead somewhere inside one.
fn repo_at(dir: &Path) -> (Option<GitRepo>, Option<PathBuf>) {
    if is_fresh_dir(dir) {
        return (None, None);
    }
    let repo = GitRepo::new(dir);
    let Some(top) = repo.toplevel() else {
        return (None, None);
    };
    if !repo.has_commits() {
        return (None, None);
    }
    if std::fs::canonicalize(&top).ok() == std::fs::canonicalize(dir).ok() {
        (Some(repo), None)
    } else {
        (None, Some(top))
    }
}

/// The repository `dir` is the top of, with commits; `None` for anything
/// `init` treats as a new project.
fn existing_repo(dir: &Path) -> anyhow::Result<Option<GitRepo>> {
    let (repo, outer) = repo_at(dir);
    if let Some(top) = outer {
        // A nested `git init` here would hide the project from the outer
        // repository, and a worktree of the outer one would hold far more
        // than the project: ask for one or the other.
        bail!(
            "{} is inside the repository {}, not at its top. Run tenaz init on the \
             repository (--project {}) to work on a branch of it, or pass --in-place \
             to make this directory a repository of its own as before.",
            dir.display(),
            top.display(),
            crate::shell_quote(&top.display().to_string())
        );
    }
    Ok(repo)
}

/// True when `dir` is a repository with commits that `init` would work on
/// in a worktree (for the wizard's question).
pub fn is_existing_repo(dir: &Path) -> bool {
    matches!(existing_repo(dir), Ok(Some(_)))
}

/// Decides where the run goes, checking what can be checked before
/// anything is created. `brief` is the brief's text when known and
/// `brief_file` the file it comes from, if any.
pub fn place(
    dir: &Path,
    request: &PlaceRequest,
    brief: Option<&str>,
    brief_file: Option<&Path>,
) -> anyhow::Result<Placement> {
    if dir.join(".tenaz/run.json").exists() {
        bail!(
            "{} already has a Tenaz run (.tenaz/run.json); tenaz run --project {} continues it",
            dir.display(),
            crate::shell_quote(&dir.display().to_string())
        );
    }
    let repo = if request.in_place {
        match repo_at(dir) {
            (Some(repo), _) => repo,
            _ => return Ok(Placement::Fresh),
        }
    } else {
        match existing_repo(dir)? {
            Some(repo) => repo,
            None => return Ok(Placement::Fresh),
        }
    };
    let base_commit = repo.head()?;
    let base_branch = repo.current_branch();
    if request.in_place {
        return Ok(Placement::InPlace {
            origin: Origin {
                repo: dir.display().to_string(),
                branch: base_branch.clone(),
                base_branch,
                base_commit,
                worktree: None,
                init_commit: None,
            },
        });
    }
    // An untracked brief here is the brief, when no other file is.
    let in_checkout = dir.join(BRIEF);
    let is_it =
        |f: &Path| std::fs::canonicalize(f).ok() == std::fs::canonicalize(&in_checkout).ok();
    let untracked_brief =
        brief_file.is_none_or(is_it) && !repo.in_head(BRIEF) && in_checkout.is_file();
    let blocking = blocking_changes(&repo.status_lines()?, untracked_brief);
    if !blocking.is_empty() {
        let mut shown: Vec<String> = blocking.iter().take(10).map(|l| format!("  {l}")).collect();
        if blocking.len() > shown.len() {
            shown.push(format!("  ... and {} more", blocking.len() - shown.len()));
        }
        bail!(
            "{} has uncommitted changes:\n{}\nTenaz works on a new branch in its own worktree, \
             which starts from the last commit and would not see them. Commit or stash them \
             first (untracked files too), or pass --in-place to run on the current branch \
             here, with these changes in Tenaz's first commit.",
            dir.display(),
            shown.join("\n")
        );
    }
    let brief_text = match brief {
        Some(text) => Some(text.to_owned()),
        None => std::fs::read_to_string(dir.join(BRIEF)).ok(),
    };
    let wanted = request.name.as_deref().map_or_else(
        || {
            slugify(
                &brief_text
                    .as_deref()
                    .and_then(goal_line)
                    .unwrap_or_default(),
            )
        },
        slugify,
    );
    let (branch, path) = free_names(&repo, dir, &wanted, request.worktree.as_deref());
    Ok(Placement::Worktree(WorktreePlan {
        repo: dir.to_path_buf(),
        base_branch,
        base_commit,
        branch,
        path,
        untracked_brief: (untracked_brief && brief_file.is_none()).then_some(in_checkout),
    }))
}

/// `tenaz/<slug>` and its worktree directory, with `-2`, `-3`... when a
/// branch or directory of that name exists.
fn free_names(repo: &GitRepo, dir: &Path, slug: &str, path: Option<&Path>) -> (String, PathBuf) {
    for n in 1.. {
        let slug = if n == 1 {
            slug.to_owned()
        } else {
            format!("{slug}-{n}")
        };
        let branch = format!("tenaz/{slug}");
        let target = path.map_or_else(|| worktree_dir(dir, &slug), Path::to_path_buf);
        let taken = repo.branch_exists(&branch) || (path.is_none() && target.exists());
        if !taken {
            return (branch, target);
        }
    }
    unreachable!("some suffix is free")
}

impl WorktreePlan {
    /// Creates the branch and the worktree.
    pub fn create(&self) -> anyhow::Result<()> {
        if self.path.exists() && !is_fresh_dir(&self.path) {
            bail!(
                "{} exists and is not empty; pass --worktree with another directory",
                self.path.display()
            );
        }
        GitRepo::new(&self.repo)
            .add_worktree(&self.path, &self.branch, &self.base_commit)
            .with_context(|| format!("cannot create the worktree {}", self.path.display()))
    }

    /// Undoes [`Self::create`] after a failed `init`.
    pub fn discard(&self) {
        let repo = GitRepo::new(&self.repo);
        let _ = repo.remove_worktree(&self.path, true);
        let _ = repo.delete_branch(&self.branch);
    }

    pub fn origin(&self) -> Origin {
        Origin {
            repo: self.repo.display().to_string(),
            base_branch: self.base_branch.clone(),
            base_commit: self.base_commit.clone(),
            branch: Some(self.branch.clone()),
            worktree: Some(self.path.display().to_string()),
            init_commit: None,
        }
    }
}

/// One line on where a run works, for `run` and `status`.
pub fn describe(origin: &Origin) -> String {
    let base = match &origin.base_branch {
        Some(b) => format!("{b} at {}", short(&origin.base_commit)),
        None => short(&origin.base_commit).to_owned(),
    };
    match (&origin.worktree, &origin.branch) {
        (Some(path), branch) => format!(
            "Worktree: {path} (branch {}, from {base}; your checkout {} is untouched)",
            branch.as_deref().unwrap_or("?"),
            origin.repo
        ),
        (None, Some(branch)) => format!(
            "In place: branch {branch} of {} (from {})",
            origin.repo,
            short(&origin.base_commit)
        ),
        (None, None) => format!(
            "In place: {} on a detached HEAD (from {})",
            origin.repo,
            short(&origin.base_commit)
        ),
    }
}

/// For a directory without a run: the Tenaz runs in worktrees of its
/// repository, as lines to print.
pub fn runs_elsewhere(dir: &Path) -> Vec<String> {
    let repo = GitRepo::new(dir);
    let Ok(worktrees) = repo.worktrees() else {
        return Vec::new();
    };
    worktrees
        .into_iter()
        .filter(|w| w.branch.as_deref().is_some_and(|b| b.starts_with("tenaz/")))
        .filter(|w| w.path.join(".tenaz/run.json").exists())
        .map(|w| {
            format!(
                "A Tenaz run works in the worktree {} (branch {}): --project {}",
                w.path.display(),
                w.branch.unwrap_or_default(),
                crate::shell_quote(&w.path.display().to_string())
            )
        })
        .collect()
}

// ---------- finish ----------

/// What `finish` was asked to do.
#[derive(Clone, Debug, Default)]
pub struct FinishRequest {
    /// The deliverable branch; default: the run's slug.
    pub branch: Option<String>,
    /// Move the deliverable branch when it exists.
    pub force: bool,
    pub remove_worktree: bool,
    pub delete_run_branch: bool,
}

/// What `finish` did.
#[derive(Clone, Debug)]
pub struct Finished {
    pub branch: String,
    pub commit: String,
    pub base: Option<String>,
    pub stat: String,
    pub notes: Vec<String>,
}

/// The base, the init commit and the run branch: from `origin` when
/// recorded, else from the history (the parent of the `tenaz: init`
/// commit; none for a project Tenaz created).
fn run_bounds(
    repo: &GitRepo,
    ledger: &Ledger,
    source: &str,
) -> anyhow::Result<(Option<String>, Option<String>, Option<String>)> {
    if let Some(origin) = &ledger.origin {
        let init = origin
            .init_commit
            .clone()
            .or_else(|| repo.find_commit(source, INIT_MESSAGE));
        return Ok((
            Some(origin.base_commit.clone()),
            init,
            origin.branch.clone(),
        ));
    }
    let init = repo
        .find_commit(source, INIT_MESSAGE)
        .context("no `tenaz: init` commit in the run's history; cannot tell where it started")?;
    Ok((repo.parent_of(&init), Some(init), repo.current_branch()))
}

/// The edits that leave out Tenaz's files: `.tenaz/`, and what the init
/// commit added at the root (`BRIEF.md`, `AGENTS.md`, `.gitignore` lines).
fn tenaz_edits(repo: &GitRepo, source: &str, base: Option<&str>, init: Option<&str>) -> TreeEdits {
    let mut edits = TreeEdits {
        remove: vec![STATE_DIR.to_owned()],
        replace: Vec::new(),
    };
    let Some(init) = init else {
        return edits;
    };
    let at_base = |path: &str| base.and_then(|b| repo.file_at(b, path));
    for file in OWNED_FILES {
        if at_base(file).is_none() && repo.file_at(init, file).is_some() {
            edits.remove.push(file.to_owned());
        }
    }
    if let (Some(current), Some(added)) = (
        repo.file_at(source, ".gitignore"),
        repo.file_at(init, ".gitignore"),
    ) {
        match strip_added_lines(&current, at_base(".gitignore").as_deref(), &added) {
            None => edits.remove.push(".gitignore".to_owned()),
            Some(text) if text != current => edits.replace.push((".gitignore".to_owned(), text)),
            Some(_) => {}
        }
    }
    edits
}

/// Makes the deliverable branch for the run in `project`.
pub fn finish(project: &Project, request: &FinishRequest) -> anyhow::Result<Finished> {
    let ledger = project.load_ledger()?;
    if request.remove_worktree {
        worktree_to_remove(&ledger)?;
    }
    let repo = GitRepo::new(project.root());
    let head = repo.head().context("the run's directory has no commits")?;
    let mut notes = Vec::new();
    // The last accepted state; a later commit belongs to a step the run
    // may still undo.
    let source = match ledger.checkpoint.as_deref() {
        Some(checkpoint) if repo.resolve(checkpoint).is_ok() => checkpoint.to_owned(),
        _ => head.clone(),
    };
    if source != head {
        notes.push(format!(
            "HEAD ({}) is past the run's last checkpoint; the deliverable stops at {}.",
            short(&head),
            short(&source)
        ));
    }
    if ledger.phase.is_running() {
        notes.push(format!(
            "The run is {} and not finished: the deliverable holds what it has accepted so far.",
            ledger.phase.as_str()
        ));
    }
    let (base, init, run_branch) = run_bounds(&repo, &ledger, &source)?;
    let edits = tenaz_edits(&repo, &source, base.as_deref(), init.as_deref());
    let tree = repo.edited_tree(&source, &edits)?;
    let unchanged = match &base {
        Some(base) => repo.tree_of(base)? == tree,
        None => tree == EMPTY_TREE,
    };
    if unchanged {
        bail!(
            "The run changed nothing in the project since {}; there is nothing to finish.",
            base.as_deref().map_or("its start", short)
        );
    }
    let brief = project
        .read_if(BRIEF)
        .or_else(|| project.read_if("BRIEF.md"))
        .unwrap_or_default();
    let commits = repo.count_commits(base.as_deref(), &source)?;
    let message = finish_message(&brief, run_branch.as_deref(), commits);
    let commit = repo.commit_tree(&tree, base.as_deref(), &message)?;
    let branch = match &request.branch {
        Some(name) => name.clone(),
        None => run_branch
            .as_deref()
            .and_then(|b| b.strip_prefix("tenaz/"))
            .map_or_else(
                || slugify(&goal_line(&brief).unwrap_or_default()),
                str::to_owned,
            ),
    };
    if Some(branch.as_str()) == run_branch.as_deref() {
        bail!("{branch} is the run's own branch; pass --branch with another name");
    }
    if repo.branch_exists(&branch) && !request.force {
        bail!("Branch {branch} exists; pass --branch with another name, or --force to move it");
    }
    repo.set_branch(&branch, &commit, request.force)?;
    let stat = match &base {
        Some(base) => repo.diff_stat(base, Some(&commit), false)?,
        None => String::new(),
    };
    if request.remove_worktree {
        notes.extend(remove_worktree(&ledger, request.delete_run_branch)?);
    }
    Ok(Finished {
        branch,
        commit,
        base,
        stat,
        notes,
    })
}

/// The run's origin and worktree, or why there is no worktree to remove.
fn worktree_to_remove(ledger: &Ledger) -> anyhow::Result<(&Origin, &str)> {
    let Some(origin) = &ledger.origin else {
        bail!("This run has no worktree of its own; there is nothing to remove");
    };
    let Some(path) = &origin.worktree else {
        bail!(
            "This run works in place in {}; there is no worktree to remove",
            origin.repo
        );
    };
    Ok((origin, path))
}

/// Removes the run's worktree (and, when asked, its branch) through the
/// user's checkout.
fn remove_worktree(ledger: &Ledger, delete_branch: bool) -> anyhow::Result<Vec<String>> {
    let (origin, path) = worktree_to_remove(ledger)?;
    let repo = GitRepo::new(&origin.repo);
    repo.remove_worktree(Path::new(path), false)
        .with_context(|| {
            format!(
                "cannot remove the worktree {path} (git keeps a worktree with uncommitted or \
             untracked files; look at it, then git worktree remove --force {path})"
            )
        })?;
    let mut notes = vec![format!("Removed the worktree {path}.")];
    if delete_branch && let Some(branch) = &origin.branch {
        repo.delete_branch(branch)?;
        notes.push(format!("Deleted the run branch {branch}."));
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goal_is_a_telling_heading_or_the_first_line_of_text() {
        assert_eq!(
            goal_line("# Fake brief\n\nBuild four files.\n").as_deref(),
            Some("Fake brief")
        );
        assert_eq!(
            goal_line("# Goal\n\nBuild four trivial files.\n").as_deref(),
            Some("Build four trivial files.")
        );
        assert_eq!(goal_line("\n\n").as_deref(), None);
    }

    #[test]
    fn slugs_are_short_ascii_and_never_empty() {
        assert_eq!(
            slugify("Build four trivial files."),
            "build-four-trivial-files"
        );
        assert_eq!(slugify("Añadir ÉXITO a la API v2, ya"), "a-adir-xito-a-la");
        assert_eq!(slugify("¿?"), "run");
        let long = slugify("internationalization localization containerization orchestration");
        assert!(long.len() <= 40, "{long}");
        assert_eq!(long, "internationalization-localization");
    }

    #[test]
    fn the_worktree_sits_next_to_the_repository() {
        assert_eq!(
            worktree_dir(Path::new("/src/app"), "add-login"),
            PathBuf::from("/src/app-tenaz-add-login")
        );
    }

    #[test]
    fn only_an_untracked_brief_is_let_through() {
        let status = vec!["?? .tenaz/brief.md".to_owned(), " M src/lib.rs".to_owned()];
        assert_eq!(blocking_changes(&status, true), vec![" M src/lib.rs"]);
        assert_eq!(blocking_changes(&status, false).len(), 2);
        assert!(blocking_changes(&["?? .tenaz/brief.md".to_owned()], true).is_empty());
        assert_eq!(blocking_changes(&["?? BRIEF.md".to_owned()], true).len(), 1);
    }

    #[test]
    fn the_lines_init_added_are_stripped() {
        let init = "target/\n.tenaz/private/\n.tenaz/run.json\n";
        let current = "target/\n.tenaz/private/\n.tenaz/run.json\ndist/\n";
        assert_eq!(
            strip_added_lines(current, Some("target/\n"), init).as_deref(),
            Some("target/\ndist/\n")
        );
        assert_eq!(
            strip_added_lines(".tenaz/run.json\n", None, ".tenaz/run.json\n"),
            None
        );
        assert_eq!(
            strip_added_lines("dist/\n.tenaz/run.json\n", None, ".tenaz/run.json\n").as_deref(),
            Some("dist/\n")
        );
        assert_eq!(
            strip_added_lines(".tenaz/run.json\n", Some(""), ".tenaz/run.json\n").as_deref(),
            Some("")
        );
    }

    #[test]
    fn the_message_names_the_goal_and_the_history() {
        let message = finish_message(
            "# Goal\n\nBuild four trivial files.\n\n## Details\n\nMore.\n",
            Some("tenaz/build-four"),
            7,
        );
        assert!(
            message.starts_with("Build four trivial files\n"),
            "{message}"
        );
        assert!(message.contains("7 commits of a Tenaz run on branch tenaz/build-four"));
        assert!(!message.contains("More."), "{message}");
        let message = finish_message("# Add a login page\n\nWith a form.\n", None, 1);
        assert!(
            message.starts_with("Add a login page\n\nWith a form.\n"),
            "{message}"
        );
        assert!(message.contains("1 commit of a Tenaz run,"), "{message}");
        let long = finish_message(&"word ".repeat(40), None, 2);
        assert!(long.lines().next().unwrap().chars().count() <= 72, "{long}");
    }
}
