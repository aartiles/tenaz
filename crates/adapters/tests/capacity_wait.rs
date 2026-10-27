//! The pause before a session is rerun at a lowered context budget: it lasts
//! `capacityRetryWaitSec`, and a stop during it pauses the run before
//! another session starts.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tenaz_adapters::git::GitRepo;
use tenaz_adapters::notify::SilentNotifier;
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::ledger::{JournalRecord, ModelConfig};
use tenaz_core::orchestrator::{Deps, InitOptions, RunOptions, Runner, Templates, init};
use tenaz_core::params::RunParams;
use tenaz_core::ports::Clock;
use tenaz_core::project::Project;
use tenaz_core::testing::{FastClock, ScriptedHarness};
use tokio_util::sync::CancellationToken;

/// The capacity wait the run is given; no other wait of the run is as long.
const WAIT: Duration = Duration::from_secs(4321);

/// The fast clock, except that being asked to wait [`WAIT`] stops the run.
struct StopOnWait {
    clock: FastClock,
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

/// A fake project, set up, whose run waits [`WAIT`] after a capacity error.
fn fake_project(work: &Path) -> (Project, FastClock) {
    let brief = work.join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild one trivial file.\n").unwrap();
    let project = Project::new(work.join("project"));
    let mut params = RunParams::default();
    let wait = format!("capacityRetryWaitSec={}", WAIT.as_secs());
    for pair in ["notify=false", "engineRetryBaseSec=0", &wait] {
        params.apply_override(pair).unwrap();
    }
    let config = ModelConfig {
        harness: "fake".into(),
        harness_version: "fake-pi 0.0.1".into(),
        model: "fake/fake".into(),
        provider: Some("fake".into()),
        id: "fake".into(),
        base_url: None,
        context_window: 131_072,
        input: vec!["text".into()],
        thinking: Some("medium".into()),
    };
    let templates =
        Templates::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates")).unwrap();
    let options = InitOptions {
        launch: tenaz_core::ledger::Launch::default(),
        brief: Some(brief),
        config,
        params,
        skills: vec![],
        runner_version: "test".into(),
    };
    let clock = FastClock::new(100);
    init(
        &project,
        &GitRepo::new(project.root()),
        &clock,
        &templates,
        options,
    )
    .unwrap();
    (project, clock)
}

#[tokio::test]
async fn a_stop_during_the_capacity_wait_pauses_before_the_rerun() {
    let work = tempfile::tempdir().unwrap();
    let (project, clock) = fake_project(work.path());

    // The scripted bootstrap's second session overflows the engine.
    let stop = CancellationToken::new();
    let deps = Deps {
        harness: Arc::new(ScriptedHarness::default()),
        vcs: Arc::new(GitRepo::new(project.root())),
        verify: Arc::new(VerifyProcess::new(project.root())),
        clock: Arc::new(StopOnWait {
            clock,
            stop: stop.clone(),
        }),
        notifier: Arc::new(SilentNotifier),
        engine: None,
        lease: None,
        engine_metrics: None,
        memory: None,
        effort: None,
    };
    let mut runner = Runner::open(project.clone(), deps, stop).unwrap();
    runner
        .run(RunOptions {
            runner_version: "test".into(),
            ..Default::default()
        })
        .await
        .unwrap();

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
