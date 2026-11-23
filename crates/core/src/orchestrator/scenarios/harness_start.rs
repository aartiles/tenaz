//! A harness that ends before it sends the engine anything is not an
//! engine failure. In the Raycaster Shooter run of 2026-11-19 a shell
//! command removed `.tenaz/private`, and every later Pi session exited at
//! once with `Model "<provider>/<id>" not found`: the run waited through
//! its six engine retries, 45 minutes, and blocked as "engine
//! unavailable" while the engine served. Now one rerun is allowed, and a
//! second such session, with the engine serving, blocks the run as a
//! harness failure that quotes the harness's stderr.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, resume};
use crate::ledger::{JournalRecord, Phase, SessionRecord};
use crate::ports::{EngineAdapter, EngineError, ModelState};
use crate::project::Project;
use crate::testing::{
    PI_MODEL_NOT_FOUND, ScriptedHarness, ScriptedUnstartable, SnapshotVcs, VirtualClock,
};

/// An engine that serves the fake run's model until `down` is set; one
/// that `comes_back` fails one probe, then serves again.
struct Downable {
    down: Arc<AtomicBool>,
    comes_back: bool,
}

#[async_trait]
impl EngineAdapter for Downable {
    fn name(&self) -> &str {
        "downable"
    }

    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        if self.down.load(Ordering::SeqCst) {
            if self.comes_back {
                self.down.store(false, Ordering::SeqCst);
            }
            return Err(EngineError::Unreachable("connection refused".into()));
        }
        Ok(vec![ModelState {
            id: "fake".into(),
            loaded: true,
            ready: true,
            context_length: None,
        }])
    }

    async fn load(&self, _model: &str) -> Result<(), EngineError> {
        Err(EngineError::Refused("no load API".into()))
    }

    async fn unload(&self, _model: &str) -> Result<(), EngineError> {
        Err(EngineError::Refused("no load API".into()))
    }
}

/// For a session count that has no end.
const ALWAYS: u32 = u32::MAX;

/// How the engine fails with the harness: it goes down at the harness's
/// `at`th exit, and comes back after one probe when `comes_back`.
#[derive(Clone, Copy)]
struct Outage {
    at: u32,
    comes_back: bool,
}

/// Runs the fake goal with `overrides`, with T001's first `times` builder
/// sessions ending before their first turn, on an engine that serves
/// except for `outage`; the project, kept with its directory, and its
/// repository.
async fn run_unstartable(
    times: u32,
    outage: Option<Outage>,
    overrides: &[&str],
) -> (tempfile::TempDir, Project, Arc<SnapshotVcs>) {
    let work = tempfile::tempdir().unwrap();
    let mut set = vec!["maxAttempts=1"];
    set.extend_from_slice(overrides);
    let (project, vcs) = fake_project(work.path(), &set);
    let down = Arc::new(AtomicBool::new(false));
    let harness = ScriptedHarness {
        unstartable: Some(ScriptedUnstartable {
            task: "T001".into(),
            times,
            engine_down: outage.map(|o| (o.at, down.clone())),
        }),
        ..Default::default()
    };
    let mut deps = deps(
        &project,
        vcs.clone(),
        Arc::new(harness),
        Arc::new(VirtualClock::new()),
    );
    let comes_back = outage.is_some_and(|o| o.comes_back);
    deps.engine = Some(Arc::new(Downable { down, comes_back }));
    super::run(&project, deps, CancellationToken::new()).await;
    (work, project, vcs)
}

/// What `pick` takes from each record of the journal, in order.
fn journaled<T>(project: &Project, pick: impl Fn(JournalRecord) -> Option<T>) -> Vec<T> {
    project
        .journal()
        .into_iter()
        .filter_map(|e| pick(e.record))
        .collect()
}

/// T001's builder sessions, in order.
fn t001_builders(project: &Project) -> Vec<SessionRecord> {
    journaled(project, |record| match record {
        JournalRecord::Session(s) if s.label.ends_with("-builder-T001") => Some(s),
        _ => None,
    })
}

/// The causes of the run's engine retries, in order.
fn retry_causes(project: &Project) -> Vec<String> {
    journaled(project, |record| match record {
        JournalRecord::EngineRetry { cause, .. } => cause,
        _ => None,
    })
}

/// One such session is rerun, uncounted, and the run goes on: a harness
/// that cannot start once may start the next time.
#[tokio::test(start_paused = true)]
async fn a_harness_that_once_ends_before_the_engine_is_rerun() {
    let (_work, project, _) = run_unstartable(1, None, &[]).await;
    let log = project.read_private("orchestrator.log").unwrap();
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    let builders = t001_builders(&project);
    let exit = builders[0].harness_exit.as_deref().unwrap_or_default();
    assert!(exit.contains(PI_MODEL_NOT_FOUND), "{exit}");
    assert_eq!(retry_causes(&project), ["harness"], "{log}");
}

/// Two in a row, with the engine serving, block the run at once as a
/// harness failure with the harness's stderr, not after the engine
/// retries as an engine failure. The attempt is not charged, and
/// `--resume` with a harness that starts finishes the run.
#[tokio::test(start_paused = true)]
async fn a_harness_that_twice_ends_before_the_engine_blocks_the_run() {
    let (_work, project, vcs) = run_unstartable(ALWAYS, None, &[]).await;
    let log = project.read_private("orchestrator.log").unwrap();
    let ledger = project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Blocked, "{log}");
    let reason = ledger.blocked_reason.unwrap_or_default();
    assert!(reason.starts_with("harness failed: "), "{reason}");
    assert!(reason.contains(PI_MODEL_NOT_FOUND), "{reason}");
    assert!(
        reason.ends_with("Fix the harness, then run with --resume"),
        "{reason}"
    );
    assert_eq!(t001_builders(&project).len(), 2, "{log}");
    assert_eq!(retry_causes(&project), ["harness"], "{log}");
    let card = project.read(".tenaz/tasks/T001.md").unwrap();
    assert!(card.contains("attempts: 0"), "{card}");

    let fixed = deps(
        &project,
        vcs,
        Arc::new(ScriptedHarness::default()),
        Arc::new(VirtualClock::new()),
    );
    resume(&project, fixed).await;
    let log = project.read_private("orchestrator.log").unwrap();
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
}

/// With no engine retry left at the second such session, the harness is
/// still blamed: it is asked about before the retries are counted.
#[tokio::test(start_paused = true)]
async fn a_harness_is_blamed_with_no_engine_retry_left() {
    let (_work, project, _) = run_unstartable(ALWAYS, None, &["engineRetries=1"]).await;
    let ledger = project.load_ledger().unwrap();
    let reason = ledger.blocked_reason.unwrap_or_default();
    assert!(reason.starts_with("harness failed: "), "{reason}");
}

/// Runs with every T001 builder session ending before its first turn,
/// with `outage` and `overrides`, until the run blocks; the project and
/// its block reason.
async fn blocked(outage: Outage, overrides: &[&str]) -> (tempfile::TempDir, Project, String) {
    let (work, project, _) = run_unstartable(ALWAYS, Some(outage), overrides).await;
    let log = project.read_private("orchestrator.log").unwrap();
    let ledger = project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Blocked, "{log}");
    assert_eq!(t001_builders(&project).len(), 3, "{log}");
    (work, project, ledger.blocked_reason.unwrap_or_default())
}

/// When the engine stops serving as the harness ends, the harness may
/// have ended because of it: the sessions are rerun as before, and the run
/// blocks as an engine failure once `engineRetries` are used.
#[tokio::test(start_paused = true)]
async fn a_harness_that_ends_with_the_engine_down_is_not_blamed() {
    let outage = Outage {
        at: 1,
        comes_back: false,
    };
    let (_work, project, reason) = blocked(outage, &["engineRetries=2"]).await;
    assert!(reason.starts_with("engine unavailable: "), "{reason}");
    assert_eq!(retry_causes(&project), ["harness", "harness"]);
}

/// The engine was down when the second such session ended, and serves
/// again by the third: the harness is blamed then.
#[tokio::test(start_paused = true)]
async fn a_harness_is_blamed_once_the_engine_serves_again() {
    let outage = Outage {
        at: 2,
        comes_back: true,
    };
    let (_work, _, reason) = blocked(outage, &[]).await;
    assert!(reason.starts_with("harness failed: "), "{reason}");
}
