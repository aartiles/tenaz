//! The `tenaz` subcommands, one module each, and what they share.

pub(crate) mod demo;
pub(crate) mod doctor;
pub(crate) mod finish;
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
use tenaz_core::project::Project;

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

/// The project at `dir` or, when `dir` has none, in the only one of its
/// subfolders that has one; else an error that points at the runs in
/// worktrees of the repository there.
fn initialized(dir: PathBuf) -> anyhow::Result<Project> {
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
