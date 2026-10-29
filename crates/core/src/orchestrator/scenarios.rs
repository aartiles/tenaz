//! Whole runs against the scripted harness and the in-memory fakes: no
//! git, no shell (ADR 0005).

mod conflict;
mod harness_exit;
mod role_models;
mod scripted_run;
mod stop_grace;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{Deps, InitOptions, RunOptions, Runner, Templates, init};
use crate::ledger::{HarnessKind, JournalRecord, Launch, ModelConfig};
use crate::params::RunParams;
use crate::ports::{Clock, Harness};
use crate::project::Project;
use crate::testing::{ScriptedHarness, ScriptedVerify, SilentNotifier, SnapshotVcs, VirtualClock};
use crate::write_scope::WriteScope;

/// A project set up with `overrides`, and the repository it was set up in:
/// the snapshots live in memory, so the run must use the same one.
fn fake_project(work: &Path, overrides: &[&str]) -> (Project, Arc<SnapshotVcs>) {
    let brief = work.join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild four trivial files.\n").unwrap();
    let project = Project::new(work.join("project"));
    let mut params = RunParams::default();
    for pair in ["notify=false", "engineRetryBaseSec=0"]
        .iter()
        .chain(overrides)
    {
        params.apply_override(pair).unwrap();
    }
    let config = ModelConfig {
        harness: HarnessKind::Fake,
        harness_version: "fake-pi 0.0.1".into(),
        model: "fake/fake".into(),
        provider: Some("fake".into()),
        id: "fake".into(),
        base_url: None,
        context_window: 131_072,
        input: vec!["text".into()],
        thinking: Some("medium".into()),
        role_models: BTreeMap::new(),
    };
    let templates =
        Templates::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates")).unwrap();
    let options = InitOptions {
        launch: Launch::default(),
        brief: Some(brief),
        config,
        params,
        skills: vec![],
        runner_version: "test".into(),
    };
    let vcs = Arc::new(SnapshotVcs::new(project.root()));
    init(&project, &*vcs, &VirtualClock::new(), &templates, options).unwrap();
    (project, vcs)
}

/// The fakes for a run of `project`, with `harness` and `clock`.
fn deps(
    project: &Project,
    vcs: Arc<SnapshotVcs>,
    harness: Arc<dyn Harness>,
    clock: Arc<dyn Clock>,
) -> Deps {
    Deps {
        harness,
        vcs,
        verify: Arc::new(ScriptedVerify::new(project.root())),
        clock,
        notifier: Arc::new(SilentNotifier),
        engine: None,
        lease: None,
        engine_metrics: None,
        memory: None,
        effort: None,
        cache_report: None,
        write_scope: Some(WriteScope::new(project.root(), None, Vec::new())),
    }
}

/// Opens the run of `project` and runs it until it ends or pauses.
async fn run(project: &Project, deps: Deps, stop: CancellationToken) {
    let mut runner = Runner::open(project.clone(), deps, stop).unwrap();
    runner
        .run(RunOptions {
            runner_version: "test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
}

/// The capacity wait the run is given; no other wait of the run is as long.
const WAIT: Duration = Duration::from_secs(4321);

/// The virtual clock, except that being asked to wait [`WAIT`] stops the run.
struct StopOnWait {
    clock: VirtualClock,
    stop: CancellationToken,
}

#[async_trait]
impl Clock for StopOnWait {
    fn now_rfc3339(&self) -> String {
        self.clock.now_rfc3339()
    }

    fn monotonic_ms(&self) -> u64 {
        self.clock.monotonic_ms()
    }

    async fn sleep(&self, duration: Duration) {
        if duration == WAIT {
            self.stop.cancel();
        }
        self.clock.sleep(duration).await;
    }
}

/// The pause before a session is rerun at a lowered context budget lasts
/// `capacityRetryWaitSec`, and a stop during it pauses the run before
/// another session starts. The scripted bootstrap's second session
/// overflows the engine.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_capacity_wait_pauses_before_the_rerun() {
    let work = tempfile::tempdir().unwrap();
    let wait = format!("capacityRetryWaitSec={}", WAIT.as_secs());
    let (project, vcs) = fake_project(work.path(), &[&wait]);
    let stop = CancellationToken::new();
    let clock = Arc::new(StopOnWait {
        clock: VirtualClock::new(),
        stop: stop.clone(),
    });
    let harness = Arc::new(ScriptedHarness {
        engine_failures: true,
        ..Default::default()
    });
    run(&project, deps(&project, vcs, harness, clock), stop).await;

    let journal = project.journal();
    let capped = journal.iter().find_map(|e| match &e.record {
        JournalRecord::ContextCap { label, .. } => Some(label),
        _ => None,
    });
    let paused = journal.iter().find_map(|e| match &e.record {
        JournalRecord::Paused { label, .. } => label.as_ref(),
        _ => None,
    });
    assert!(capped.is_some(), "the engine never ran out of memory");
    assert_eq!(paused, capped, "the run did not pause in the capacity wait");
}
