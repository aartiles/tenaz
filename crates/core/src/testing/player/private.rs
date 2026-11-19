//! The scripted harness's builder that removes `.tenaz/private`, or part
//! of it, with a shell command, as one did in the Raycaster Shooter run of
//! 2026-11-19.

use std::path::Path;
use std::sync::atomic::Ordering;

use super::{Player, ToolCall};
use crate::project::{Project, RUN_LOCK};
use crate::testing::{Removed, ScriptedRemoval};

impl Player {
    /// When the script's removal names `task` and has not run yet, removes
    /// what it says in a shell call; the session then goes on. The next
    /// builder session records whether a run still holds the lock.
    pub(super) async fn remove_private(&mut self, task: &str) {
        let Some(removal) = self.script.remove_private.clone() else {
            return;
        };
        if removal.done.load(Ordering::SeqCst) {
            let held = Project::new(&self.cwd).active_run().is_some();
            let _ = removal.lock_held.set(held);
            return;
        }
        if removal.task != task {
            return;
        }
        removal.done.store(true, Ordering::SeqCst);
        let command = match removal.what {
            Removed::Folder => "rm -rf .tenaz/private",
            Removed::Prompts => "rm .tenaz/private/*.prompt.md",
            Removed::Lock | Removed::LockForAnotherRun => "rm .tenaz/private/run.lock",
        };
        let call = ToolCall::new("shell", serde_json::json!({ "command": command }));
        let cwd = self.cwd.clone();
        self.turn(call, move |_| remove(&cwd, &removal)).await;
    }
}

/// Removes what `removal` says from the project at `cwd`.
fn remove(cwd: &Path, removal: &ScriptedRemoval) {
    let project = Project::new(cwd);
    let private = project.private_dir();
    match removal.what {
        Removed::Folder => {
            let _ = std::fs::remove_dir_all(&private);
        }
        Removed::Prompts => {
            for entry in std::fs::read_dir(&private).into_iter().flatten().flatten() {
                if entry.file_name().to_string_lossy().ends_with(".prompt.md") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Removed::Lock => {
            let _ = std::fs::remove_file(project.path(RUN_LOCK));
        }
        Removed::LockForAnotherRun => {
            let _ = std::fs::remove_file(project.path(RUN_LOCK));
            let lock = project.lock_run().ok().flatten();
            if let Ok(mut another) = removal.another_run.lock() {
                *another = lock;
            }
        }
    }
}
