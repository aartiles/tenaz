//! A session that removes `.tenaz/private`, as a shell command of the
//! agent's did in the Raycaster Shooter run of 2026-11-19. Nothing said so
//! then: the journal had only the harness's errors that followed. Now the
//! journal and the log name the session, Tenaz writes its prompt there
//! again and keeps the run locked, and the run goes on.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{RunOptions, deps, fake_project, try_run};
use crate::ledger::{JournalRecord, Phase};
use crate::orchestrator::RunError;
use crate::project::{Project, prompt_path};
use crate::testing::{Removed, ScriptedHarness, ScriptedRemoval, VirtualClock};

/// A run of the fake goal whose T001 builder removed `what`: the project,
/// kept with its directory, how the run ended, and the script.
struct Removal {
    _work: tempfile::TempDir,
    project: Project,
    ended: Result<(), RunError>,
    script: ScriptedRemoval,
}

impl Removal {
    async fn run(what: Removed) -> Self {
        let work = tempfile::tempdir().unwrap();
        let (project, vcs) = fake_project(work.path(), &[]);
        let script = ScriptedRemoval {
            task: "T001".into(),
            what,
            ..ScriptedRemoval::default()
        };
        let harness = ScriptedHarness {
            remove_private: Some(script.clone()),
            ..Default::default()
        };
        let deps = deps(
            &project,
            vcs,
            Arc::new(harness),
            Arc::new(VirtualClock::new()),
        );
        let stop = CancellationToken::new();
        let ended = try_run(&project, deps, stop, RunOptions::default()).await;
        Self {
            _work: work,
            project,
            ended,
            script,
        }
    }

    fn log(&self) -> String {
        self.project
            .read(".tenaz/private/orchestrator.log")
            .unwrap()
    }

    /// The run completed, and a run held the lock after the removal.
    fn assert_completed_locked(&self) {
        let log = self.log();
        assert!(self.ended.is_ok(), "{:?}\n{log}", self.ended);
        assert_eq!(
            self.project.load_ledger().unwrap().phase,
            Phase::Complete,
            "{log}"
        );
        assert_eq!(
            self.script.lock_held.get(),
            Some(&true),
            "the run lost its lock"
        );
    }

    /// The labels journaled as `private-removed`.
    fn removed(&self) -> Vec<String> {
        self.project
            .journal()
            .into_iter()
            .filter_map(|e| match e.record {
                JournalRecord::PrivateRemoved { label } => Some(label),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test(start_paused = true)]
async fn a_session_that_removes_the_private_folder_is_named_and_the_run_goes_on() {
    let run = Removal::run(Removed::Folder).await;
    run.assert_completed_locked();
    let removed = run.removed();
    let [label] = removed.as_slice() else {
        panic!("{removed:?}");
    };
    assert!(label.ends_with("-builder-T001"), "{label}");
    let log = run.log();
    assert!(
        log.contains(&format!("{label}: the prompt Tenaz wrote")),
        "{log}"
    );
    assert!(
        log.contains(&format!("{label}: the run lock's file")),
        "{log}"
    );
    assert!(run.project.exists(&prompt_path(label)));
}

/// Only the prompts removed, the lock file is still the run's: the session
/// is named, and the run does not take its own lock again, which would
/// stop it as another run.
#[tokio::test(start_paused = true)]
async fn a_session_that_removes_only_the_prompts_keeps_the_run_s_lock() {
    let run = Removal::run(Removed::Prompts).await;
    run.assert_completed_locked();
    assert_eq!(run.removed().len(), 1);
    assert!(!run.log().contains("the run lock's file"), "{}", run.log());
}

/// Only the lock's file removed: the run takes the lock again, and no
/// prompt is missing.
#[tokio::test(start_paused = true)]
async fn a_session_that_removes_only_the_lock_has_it_taken_again() {
    let run = Removal::run(Removed::Lock).await;
    run.assert_completed_locked();
    assert_eq!(run.removed(), Vec::<String>::new());
    assert!(run.log().contains("the run lock's file"), "{}", run.log());
}

/// Another run took the lock once its file was removed: this run stops,
/// naming it, rather than work beside it, and leaves the ledger to it.
#[tokio::test(start_paused = true)]
async fn a_run_whose_lock_another_run_took_stops() {
    let run = Removal::run(Removed::LockForAnotherRun).await;
    let Err(RunError::LockLost(error)) = &run.ended else {
        panic!("{:?}\n{}", run.ended, run.log());
    };
    assert!(error.starts_with("this run stopped: "), "{error}");
    assert!(error.contains("Another tenaz run"), "{error}");
    let ledger = run.project.load_ledger().unwrap();
    let saved = "this run saved its ledger after losing the lock";
    assert_eq!(ledger.last_error, None, "{saved}");
    assert!(ledger.inflight.is_some(), "{saved}");
    assert!(run.log().contains("  this run stopped: "), "{}", run.log());
}
