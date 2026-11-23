//! The scripted harness's builder that removes the private folder, or part
//! of it, or the scratch folder `.tenaz/private`, with a shell command, as
//! one removed `.tenaz/private` (then the private folder) in the Raycaster
//! Shooter run of 2026-11-19.

use std::path::Path;
use std::sync::atomic::Ordering;

use super::{Player, ToolCall};
use crate::project::{Project, RUN_LOCK, SCRATCH};
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
            let held = Project::new(&self.cwd)
                .with_private_dir(&removal.private)
                .active_run()
                .is_some();
            let _ = removal.lock_held.set(held);
            return;
        }
        if removal.task != task {
            return;
        }
        assert!(
            !removal.private.as_os_str().is_empty(),
            "a ScriptedRemoval names the run's private folder"
        );
        removal.done.store(true, Ordering::SeqCst);
        let private = removal.private.display();
        let command = match removal.what {
            Removed::Folder => format!("rm -rf {private}"),
            Removed::Scratch => format!("rm -rf {SCRATCH}"),
            Removed::Prompts => format!("rm {private}/*.prompt.md"),
            Removed::Lock | Removed::LockForAnotherRun => format!("rm {private}/{RUN_LOCK}"),
        };
        let call = ToolCall::new("shell", serde_json::json!({ "command": command }));
        let cwd = self.cwd.clone();
        self.turn(call, move |_| remove(&cwd, &removal)).await;
    }
}

/// Removes what `removal` says from the project at `cwd`.
fn remove(cwd: &Path, removal: &ScriptedRemoval) {
    let project = Project::new(cwd).with_private_dir(&removal.private);
    let private = project.private_dir();
    match removal.what {
        Removed::Folder => {
            let _ = std::fs::remove_dir_all(&private);
        }
        Removed::Scratch => {
            let _ = std::fs::remove_dir_all(project.scratch_dir());
        }
        Removed::Prompts => {
            for entry in std::fs::read_dir(&private).into_iter().flatten().flatten() {
                if entry.file_name().to_string_lossy().ends_with(".prompt.md") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Removed::Lock => {
            let _ = std::fs::remove_file(project.run_lock_path());
        }
        Removed::LockForAnotherRun => {
            let _ = std::fs::remove_file(project.run_lock_path());
            let lock = project.lock_run().ok().flatten();
            if let Ok(mut another) = removal.another_run.lock() {
                *another = lock;
            }
        }
    }
}
