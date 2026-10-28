//! `tenaz status`: where a run is, read from its files.

use std::path::PathBuf;

use tenaz_core::orchestrator::{status_json, status_text};
use tenaz_core::project::Project;

use crate::worktree;

/// `tenaz status`: the run's state as text, or as one JSON object.
pub(crate) fn execute(project: PathBuf, json: bool) -> anyhow::Result<()> {
    let project = Project::new(project);
    if json {
        return print_json(&project);
    }
    print!("{}", status_text(&project));
    print_where(&project);
    Ok(())
}

/// The state as one JSON object; when there is no run here, where the
/// repository's runs are.
fn print_json(project: &Project) -> anyhow::Result<()> {
    let mut value = status_json(project);
    if value["initialized"] == false {
        value["runs_elsewhere"] = worktree::runs_elsewhere(project.root()).into();
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Where the run came from, or, when there is none here, where the
/// repository's runs are.
fn print_where(project: &Project) {
    let Ok(ledger) = project.load_ledger() else {
        for line in worktree::runs_elsewhere(project.root()) {
            println!("{line}");
        }
        return;
    };
    if let Some(origin) = &ledger.origin {
        println!("{}", worktree::describe(origin));
    }
}
