//! Where a run's private folder is, and moving the folder of a run of the
//! old layout out of the project into the git directory (ADR 0040).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{ORCHESTRATOR_LOG, Project, RUN_LOCK, RunLock, SCRATCH};

/// The private folder of a run whose git directory is `git_dir`.
pub fn private_in(git_dir: &Path) -> PathBuf {
    git_dir.join("tenaz")
}

/// Where a project's private folder is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivatePlace {
    /// In use at this path.
    At(PathBuf),
    /// A run of the old layout keeps it in `from`, inside the project; a
    /// run moves it to `to` before it starts.
    Move { from: PathBuf, to: PathBuf },
}

impl PrivatePlace {
    /// Where the folder is now, for reading it.
    pub fn current(&self) -> &Path {
        match self {
            Self::At(path) | Self::Move { from: path, .. } => path,
        }
    }
}

/// Where the private folder of the project at `root` is (ADR 0040): in
/// `git_dir`, out of reach of the commands that clean the project, unless
/// `.tenaz/private` already holds a run's files (`legacy_run`) and the git
/// directory has none yet (`moved`); without a git directory, in the
/// project as before.
pub fn private_place(
    root: &Path,
    git_dir: Option<&Path>,
    legacy_run: bool,
    moved: bool,
) -> PrivatePlace {
    let legacy = root.join(SCRATCH);
    let Some(git_dir) = git_dir else {
        return PrivatePlace::At(legacy);
    };
    let place = private_in(git_dir);
    if legacy_run && !moved {
        PrivatePlace::Move {
            from: legacy,
            to: place,
        }
    } else {
        PrivatePlace::At(place)
    }
}

/// What [`Project::move_legacy_private`] did.
#[derive(Debug)]
pub enum LegacyMove {
    /// Nothing was to be moved.
    NotNeeded,
    /// The folder is in the git directory now.
    Moved { from: PathBuf },
    /// Another process holds the old folder's run lock.
    Busy,
    /// The move failed; the run keeps the folder where it was.
    Failed { from: PathBuf, error: io::Error },
}

impl Project {
    /// Whether `.tenaz/private` holds a run's own files, as runs before
    /// ADR 0040 left it.
    pub fn legacy_private_run(&self) -> bool {
        let legacy = self.root.join(SCRATCH);
        legacy.join(RUN_LOCK).exists() || legacy.join(ORCHESTRATOR_LOG).exists()
    }

    /// Whether `.tenaz/private` holds a run's files and this project's
    /// private folder is elsewhere and not made yet.
    fn has_legacy_to_move(&self) -> bool {
        let elsewhere = self.private != self.root.join(SCRATCH);
        elsewhere && !self.private.exists() && self.legacy_private_run()
    }

    /// Moves the private folder of a run of the old layout, `.tenaz/private`,
    /// to this project's private folder, holding the old folder's run lock
    /// while it does; the lock moves with the folder and is released when
    /// this returns. Another process that moved it meanwhile leaves
    /// nothing to move. A new `.tenaz/private` is left for the agent, with
    /// the folders of its own Tenaz wrote there (`jobs`, `media`).
    pub fn move_legacy_private(&self) -> LegacyMove {
        let from = self.root.join(SCRATCH);
        if !self.has_legacy_to_move() {
            return LegacyMove::NotNeeded;
        }
        let (lock, made) = match self.lock_legacy() {
            Ok((Some(lock), made)) => (lock, made),
            Ok((None, _)) => return LegacyMove::Busy,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return LegacyMove::NotNeeded,
            Err(error) => return LegacyMove::Failed { from, error },
        };
        if !self.still_to_move(&lock, made) {
            return LegacyMove::NotNeeded;
        }
        let result = self.move_from(from);
        drop(lock);
        result
    }

    /// Whether the old folder, whose lock this run now holds, is still to
    /// be moved. When another run moved it meanwhile, a lock file this run
    /// made in the scratch folder left there is removed.
    fn still_to_move(&self, lock: &RunLock, made: bool) -> bool {
        let lock_file = self.root.join(SCRATCH).join(RUN_LOCK);
        let held_there = lock.is_at(&lock_file);
        if held_there && self.has_legacy_to_move() {
            return true;
        }
        if held_there && made {
            let _ = fs::remove_file(&lock_file);
        }
        false
    }

    /// Renames `from` to the private folder, under the old folder's lock.
    fn move_from(&self, from: PathBuf) -> LegacyMove {
        let moved = self
            .private
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::rename(&from, &self.private));
        match moved {
            Ok(()) => {
                self.give_back_scratch(&from);
                LegacyMove::Moved { from }
            }
            Err(error) => LegacyMove::Failed { from, error },
        }
    }

    /// The old folder's run lock, taken without making a lock file in the
    /// scratch folder a run that moved it meanwhile left, and whether this
    /// made the file: only a run older than the lock, with a log and no
    /// lock file, gets one made.
    fn lock_legacy(&self) -> io::Result<(Option<RunLock>, bool)> {
        let old = Self::new(&self.root);
        match old.lock_existing_run() {
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.has_legacy_to_move() => {
                old.lock_run().map(|lock| (lock, true))
            }
            other => other.map(|lock| (lock, false)),
        }
    }

    /// Makes the scratch folder `from` again, with the job logs and media
    /// outputs the old layout kept beside the run's files: task notes name
    /// them by their path there.
    fn give_back_scratch(&self, from: &Path) {
        let _ = fs::create_dir_all(from);
        for name in ["jobs", "media"] {
            let _ = fs::rename(self.private.join(name), from.join(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_folder_is_in_the_git_directory() {
        let root = Path::new("/w/game");
        let git = Path::new("/w/repo/.git/worktrees/game");
        assert_eq!(
            private_place(root, Some(git), false, false),
            PrivatePlace::At(git.join("tenaz"))
        );
        assert_eq!(
            private_place(root, Some(git), true, true),
            PrivatePlace::At(git.join("tenaz")),
            "once moved, a lock left in .tenaz/private does not matter"
        );
    }

    #[test]
    fn either_the_log_or_the_lock_marks_a_run_of_the_old_layout() {
        let dir = tempfile::tempdir().unwrap();
        let p = Project::new(dir.path());
        assert!(!p.legacy_private_run());
        p.write(".tenaz/private/notes.txt", "scratch").unwrap();
        assert!(!p.legacy_private_run(), "the agent's scratch files are not");
        for name in [RUN_LOCK, ORCHESTRATOR_LOG] {
            p.write_private(name, "").unwrap();
            assert!(p.legacy_private_run(), "{name}");
            std::fs::remove_file(p.private_path(name)).unwrap();
        }
    }

    #[test]
    fn a_run_of_the_old_layout_keeps_its_folder_until_it_is_moved() {
        let root = Path::new("/w/game");
        let git = Path::new("/w/game/.git");
        let place = private_place(root, Some(git), true, false);
        assert_eq!(
            place,
            PrivatePlace::Move {
                from: root.join(".tenaz/private"),
                to: git.join("tenaz"),
            }
        );
        assert_eq!(place.current(), root.join(".tenaz/private"));
    }

    #[test]
    fn without_a_git_directory_the_private_folder_stays_in_the_project() {
        let root = Path::new("/w/game");
        assert_eq!(
            private_place(root, None, true, false),
            PrivatePlace::At(root.join(".tenaz/private"))
        );
    }

    /// A run of the old layout: `.tenaz/private` with its log and lock.
    fn legacy_run() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        let old = Project::new(dir.path().join("game"));
        old.create_layout().unwrap();
        old.append_private(ORCHESTRATOR_LOG, "[t] Session 001 started\n")
            .unwrap();
        drop(old.lock_run().unwrap().unwrap());
        let moved = old.with_private_dir(dir.path().join("git/tenaz"));
        (dir, moved)
    }

    #[test]
    fn the_old_folder_moves_with_its_files_and_leaves_a_scratch_folder() {
        let (_dir, project) = legacy_run();
        assert!(project.legacy_private_run());
        assert!(matches!(
            project.move_legacy_private(),
            LegacyMove::Moved { .. }
        ));
        assert_eq!(
            project.read_private(ORCHESTRATOR_LOG).unwrap(),
            "[t] Session 001 started\n"
        );
        assert!(project.scratch_dir().is_dir());
        assert!(!project.legacy_private_run());
        assert!(
            project.lock_run().unwrap().is_some(),
            "the lock was released"
        );
        assert!(matches!(
            project.move_legacy_private(),
            LegacyMove::NotNeeded
        ));
    }

    #[test]
    fn the_agent_s_job_logs_and_media_stay_in_the_scratch_folder() {
        let (_dir, project) = legacy_run();
        project
            .write(".tenaz/private/jobs/T002-1-1.log", "frame 1\n")
            .unwrap();
        project.write(".tenaz/private/media/a.png", "png").unwrap();
        assert!(matches!(
            project.move_legacy_private(),
            LegacyMove::Moved { .. }
        ));
        assert!(project.exists(".tenaz/private/jobs/T002-1-1.log"));
        assert!(project.exists(".tenaz/private/media/a.png"));
        assert!(!project.private_path("jobs").exists());
    }

    #[test]
    fn a_run_that_lost_the_race_leaves_no_lock_in_the_scratch_folder() {
        let (_dir, project) = legacy_run();
        // This run took the old folder's lock, making its file, and another
        // run then moved the folder and left a scratch folder in its place.
        let made = Project::new(project.root()).lock_run().unwrap().unwrap();
        std::fs::create_dir_all(project.private_dir().parent().unwrap()).unwrap();
        std::fs::rename(project.scratch_dir(), project.private_dir()).unwrap();
        let late = Project::new(project.root()).lock_run().unwrap().unwrap();
        drop(made);
        assert!(!project.still_to_move(&late, true));
        assert!(!project.legacy_private_run(), "{:?}", project.scratch_dir());
    }

    #[test]
    fn a_lock_taken_on_a_folder_moved_meanwhile_is_not_moved_again() {
        let (_dir, project) = legacy_run();
        let lock = Project::new(project.root()).lock_run().unwrap().unwrap();
        std::fs::create_dir_all(project.private_dir().parent().unwrap()).unwrap();
        std::fs::rename(project.scratch_dir(), project.private_dir()).unwrap();
        assert!(!project.still_to_move(&lock, false));
        assert!(
            project.private_path(RUN_LOCK).exists(),
            "the moved lock stays"
        );
    }

    #[test]
    fn a_run_from_before_the_lock_is_moved_too() {
        let dir = tempfile::tempdir().unwrap();
        let old = Project::new(dir.path().join("game"));
        old.append_private(ORCHESTRATOR_LOG, "[t] Session 001 started\n")
            .unwrap();
        let project = old.with_private_dir(dir.path().join("git/tenaz"));
        assert!(matches!(
            project.move_legacy_private(),
            LegacyMove::Moved { .. }
        ));
        assert!(project.read_private(ORCHESTRATOR_LOG).is_ok());
        assert!(!project.legacy_private_run());
    }

    #[test]
    fn a_folder_that_cannot_be_moved_stays_whole_and_unlocked() {
        let (dir, project) = legacy_run();
        std::fs::write(
            dir.path().join("git"),
            "a file where the git directory goes",
        )
        .unwrap();
        assert!(matches!(
            project.move_legacy_private(),
            LegacyMove::Failed { .. }
        ));
        let old = Project::new(project.root());
        assert!(old.read_private(ORCHESTRATOR_LOG).is_ok());
        assert!(old.lock_run().unwrap().is_some(), "the lock was released");
    }

    #[test]
    fn the_old_folder_is_not_moved_while_a_run_holds_its_lock() {
        let (_dir, project) = legacy_run();
        let running = Project::new(project.root()).lock_run().unwrap().unwrap();
        assert!(matches!(project.move_legacy_private(), LegacyMove::Busy));
        assert!(!project.private_dir().exists());
        drop(running);
    }
}
