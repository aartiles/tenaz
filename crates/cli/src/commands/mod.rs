//! The `tenaz` subcommands, one module each, and what they share.

pub(crate) mod demo;
pub(crate) mod doctor;
pub(crate) mod finish;
pub(crate) mod harness_bins;
pub(crate) mod info;
pub(crate) mod init;
pub(crate) mod run;
pub(crate) mod shims;
pub(crate) mod status;
pub(crate) mod watch;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tenaz_core::ledger::EngineApi;
use tenaz_core::orchestrator::Templates;
use tenaz_core::project::{self, PrivatePlace, Project};

use crate::{VERSION, wiring, worktree};

/// The runner's version string, journaled per run.
fn runner_version() -> String {
    format!("tenaz {VERSION}")
}

/// The project directory as an absolute path: harnesses, `verify` and git
/// run inside it, where a path relative to the runner's directory is wrong.
pub(crate) fn absolute(project: PathBuf) -> PathBuf {
    std::path::absolute(&project).unwrap_or(project)
}

/// The role prompts and shared rules compiled into the binary.
fn templates() -> Templates {
    use tenaz_core::role::Role;
    let prompts = [
        (
            Role::Bootstrap,
            include_str!("../../templates/prompts/bootstrap.md"),
        ),
        (
            Role::Builder,
            include_str!("../../templates/prompts/builder.md"),
        ),
        (
            Role::Reviewer,
            include_str!("../../templates/prompts/reviewer.md"),
        ),
        (
            Role::Split,
            include_str!("../../templates/prompts/split.md"),
        ),
        (
            Role::Replan,
            include_str!("../../templates/prompts/replan.md"),
        ),
        (
            Role::FinalReviewer,
            include_str!("../../templates/prompts/final-reviewer.md"),
        ),
    ];
    Templates {
        protocol: include_str!("../../templates/protocol.md").to_owned(),
        prompts: prompts
            .into_iter()
            .map(|(r, t)| (r, t.to_owned()))
            .collect(),
    }
}

/// Which engine API loads and unloads the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum EngineKind {
    /// `/v1/load-model` and `/v1/unload-model` (Sushi, mlx-serve).
    #[value(name = "load-api", alias = "sushi")]
    LoadApi,
    /// Any OpenAI-compatible engine with start and stop commands.
    Generic,
}

impl EngineKind {
    pub(crate) fn api(self) -> EngineApi {
        match self {
            Self::LoadApi => EngineApi::LoadApi,
            Self::Generic => EngineApi::Generic,
        }
    }

    /// The value `--engine` takes for it.
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Self::LoadApi => "load-api",
            Self::Generic => "generic",
        }
    }

    pub(crate) fn from_api(api: EngineApi) -> Self {
        match api {
            EngineApi::LoadApi => Self::LoadApi,
            EngineApi::Generic => Self::Generic,
        }
    }
}

/// A word as a POSIX shell reads it back.
pub(crate) fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// A path to keep in `run.json`: a bare command name stays one (found on
/// `PATH` at run time), anything else is made absolute.
fn pinned_path(path: &Path) -> String {
    if path.components().count() == 1 && !path.as_os_str().is_empty() {
        path.display().to_string()
    } else {
        absolute(path.to_path_buf()).display().to_string()
    }
}

/// `name=path` pairs, checked to load, as absolute paths by name.
fn pinned_workflows(specs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    wiring::parse_workflows(specs)?;
    specs
        .iter()
        .map(|spec| {
            let (name, path) = spec
                .split_once('=')
                .with_context(|| format!("--workflow expects name=path, got {spec}"))?;
            Ok((name.to_owned(), pinned_path(Path::new(path))))
        })
        .collect()
}

/// Where the private folder of the project at `root` is (ADR 0040). The
/// git directory counts only when `root` is the top of its working tree:
/// a project without `.git` of its own inside another repository keeps it
/// in `.tenaz/private`, as runs of several such projects would share one.
fn private_place(root: &Path) -> PrivatePlace {
    let repo = tenaz_adapters::git::GitRepo::new(root);
    let top = |dir: &Path| std::fs::canonicalize(dir).ok();
    let git_dir = repo
        .git_dir()
        .filter(|_| repo.toplevel().and_then(|t| top(&t)) == top(root));
    let legacy = Project::new(root);
    let moved = git_dir
        .as_deref()
        .is_some_and(|dir| project::private_in(dir).exists());
    project::private_place(root, git_dir.as_deref(), legacy.legacy_private_run(), moved)
}

/// Sets aside the private folder an earlier run left in the git directory
/// of the checkout a new run is initialized in (`--in-place` after
/// removing `.tenaz/`, or on another branch), so that the new run's log
/// and records start empty. Returns where it went; a folder a run still
/// holds the lock of is left alone, with an error that says so.
fn set_aside_private(root: &Path) -> std::io::Result<Option<PathBuf>> {
    let PrivatePlace::At(dir) = private_place(root) else {
        return Ok(None);
    };
    if dir == root.join(project::SCRATCH) || !dir.exists() {
        return Ok(None);
    }
    if let Some(pid) = Project::new(root).with_private_dir(&dir).active_run() {
        return Err(std::io::Error::other(format!(
            "a run (pid {pid}) still works with {}",
            dir.display()
        )));
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = dir.with_file_name(format!("tenaz-before-{secs}-{}", std::process::id()));
    std::fs::rename(&dir, &aside)?;
    Ok(Some(aside))
}

/// The project at `root`, with its private folder where it is now.
fn with_private(project: Project) -> Project {
    let place = private_place(project.root());
    project.with_private_dir(place.current())
}

/// The project at `dir` or, when `dir` has none, in the only one of its
/// subfolders that has one; else an error that points at the runs in
/// worktrees of the repository there. Its private folder is where it is
/// now, which for a run of the old layout is still `.tenaz/private`.
fn initialized(dir: PathBuf) -> anyhow::Result<Project> {
    initialized_here(dir).map(with_private)
}

fn initialized_here(dir: PathBuf) -> anyhow::Result<Project> {
    let project = Project::new(absolute(dir));
    if let Some(note) = project.legacy_layout_note() {
        anyhow::bail!(note);
    }
    let project = if project.exists(".tenaz/run.json") {
        project
    } else {
        only_run_below(project)?
    };
    if !project.exists(".tenaz/run.json") {
        anyhow::bail!(not_initialized(&project));
    }
    Ok(project)
}

/// The run in the only subfolder of `project` that has one; `project`
/// itself when none has; an error that lists them when several have.
fn only_run_below(project: Project) -> anyhow::Result<Project> {
    let runs = runs_below(project.root());
    match runs.as_slice() {
        [] => Ok(project),
        [only] => {
            eprintln!("Using the project in {}", only.display());
            Ok(Project::new(only.clone()))
        }
        _ => {
            let mut message = format!(
                "{} has no .tenaz/run.json, but these subfolders do; pick one with --project:",
                project.root().display()
            );
            for run in &runs {
                let _ = write!(message, "\n  {}", run.display());
            }
            anyhow::bail!(message)
        }
    }
}

/// Why `project` has no run, and where the repository's runs are.
fn not_initialized(project: &Project) -> String {
    let mut message = format!(
        "{} has no .tenaz/run.json; run tenaz init first",
        project.root().display()
    );
    for line in worktree::runs_elsewhere(project.root()) {
        let _ = write!(message, "\n{line}");
    }
    message
}

/// The direct subfolders of `dir` that hold a Tenaz run, sorted.
fn runs_below(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut runs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join(".tenaz/run.json").is_file())
        .collect();
    runs.sort();
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A git repository at `<tmp>/game` with a Tenaz layout, and its git
    /// directory.
    fn repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("game");
        std::fs::create_dir_all(&root).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(init.success());
        Project::new(&root).create_layout().unwrap();
        let git_dir = root.join(".git");
        (dir, root, git_dir)
    }

    #[test]
    fn a_new_run_keeps_its_private_folder_in_the_git_directory() {
        let (_dir, root, git_dir) = repo();
        let project = with_private(Project::new(&root));
        assert_eq!(project.private_dir(), git_dir.join("tenaz"));
        assert_eq!(project.scratch_dir(), root.join(".tenaz/private"));
    }

    #[test]
    fn a_run_in_a_worktree_keeps_it_in_the_worktree_s_git_directory() {
        let (_dir, root, git_dir) = repo();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap();
            assert!(ok.success(), "git {args:?}");
        };
        git(&["commit", "-q", "--allow-empty", "-m", "first"]);
        let tree = root.with_file_name("game-tenaz-run");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "tenaz/run",
            &tree.display().to_string(),
        ]);
        let project = with_private(Project::new(&tree));
        assert_eq!(
            project.private_dir(),
            git_dir.join("worktrees/game-tenaz-run/tenaz")
        );
    }

    #[test]
    fn init_sets_aside_the_private_folder_an_earlier_run_left_in_the_git_directory() {
        let (_dir, root, git_dir) = repo();
        let earlier = with_private(Project::new(&root));
        earlier
            .append_private(project::ORCHESTRATOR_LOG, "[t] the earlier run\n")
            .unwrap();
        let aside = set_aside_private(&root).unwrap().unwrap();
        assert_eq!(aside.parent(), Some(git_dir.as_path()));
        let name = aside.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("tenaz-before-"), "{name}");
        assert!(aside.join(project::ORCHESTRATOR_LOG).exists());
        let new = with_private(Project::new(&root));
        assert!(new.read_private(project::ORCHESTRATOR_LOG).is_err());
        assert_eq!(set_aside_private(&root).unwrap(), None);
    }

    #[test]
    fn init_leaves_the_private_folder_of_a_run_that_still_works() {
        let (_dir, root, _git_dir) = repo();
        let running = with_private(Project::new(&root));
        let _lock = running.lock_run().unwrap().unwrap();
        assert!(set_aside_private(&root).is_err());
        assert!(running.run_lock_path().exists());
    }

    #[test]
    fn a_project_inside_another_repository_keeps_it_in_the_project() {
        let (_dir, root, _git_dir) = repo();
        let inner = root.join("games/digger");
        Project::new(&inner).create_layout().unwrap();
        let project = with_private(Project::new(&inner));
        assert_eq!(project.private_dir(), inner.join(".tenaz/private"));
        project
            .append_private(project::ORCHESTRATOR_LOG, "[t] Session 001 started\n")
            .unwrap();
        assert_eq!(set_aside_private(&inner).unwrap(), None);
        assert!(project.read_private(project::ORCHESTRATOR_LOG).is_ok());
    }

    #[test]
    fn a_run_of_the_old_layout_is_read_in_place() {
        let (_dir, root, _git_dir) = repo();
        let old = Project::new(&root);
        old.append_private(project::ORCHESTRATOR_LOG, "[t] Session 001 started\n")
            .unwrap();
        drop(old.lock_run().unwrap().unwrap());
        let read = with_private(Project::new(&root));
        assert_eq!(read.private_dir(), root.join(".tenaz/private"));
    }
}
