//! `tenaz finish`: the run's result as one commit on a new branch of the
//! original repository.

use std::path::PathBuf;

use super::{initialized, shell_quote};
use crate::worktree;

/// `tenaz finish`.
#[derive(clap::Args, Debug)]
pub(crate) struct FinishArgs {
    /// The run's directory: the one init printed (a worktree for an existing repository).
    #[arg(long, default_value = ".")]
    project: PathBuf,
    /// The branch to create (default: the run branch's slug, without tenaz/).
    #[arg(long)]
    branch: Option<String>,
    /// Move the branch when it already exists.
    #[arg(long)]
    force: bool,
    /// Then remove the run's worktree. Its run.json and journal are not
    /// committed and go with it; the committed history stays on the run
    /// branch.
    #[arg(long)]
    remove_worktree: bool,
    /// With --remove-worktree, delete the run branch too.
    #[arg(long, requires = "remove_worktree")]
    delete_run_branch: bool,
}

pub(crate) fn execute(args: FinishArgs) -> anyhow::Result<()> {
    let project = initialized(args.project)?;
    let request = worktree::FinishRequest {
        branch: args.branch,
        force: args.force,
        remove_worktree: args.remove_worktree,
        delete_run_branch: args.delete_run_branch,
    };
    let ledger = project.load_ledger()?;
    let done = worktree::finish(&project, &request)?;
    println!(
        "Created branch {} ({}): one commit on {}.",
        done.branch,
        &done.commit[..done.commit.len().min(7)],
        done.base.as_deref().map_or_else(
            || "no parent".to_owned(),
            |b| b[..b.len().min(7)].to_owned()
        )
    );
    if !done.stat.trim().is_empty() {
        print!("{}", done.stat);
    }
    for note in &done.notes {
        println!("{note}");
    }
    if let Some(origin) = &ledger.origin {
        if let Some(branch) = &origin.branch {
            println!("The run's full history stays on {branch}.");
        }
        if let Some(path) = &origin.worktree
            && !request.remove_worktree
        {
            println!(
                "To remove the worktree: git -C {} worktree remove {}",
                shell_quote(&origin.repo),
                shell_quote(path)
            );
        }
    }
    println!("Nothing was pushed: git show {} to review it.", done.branch);
    Ok(())
}
