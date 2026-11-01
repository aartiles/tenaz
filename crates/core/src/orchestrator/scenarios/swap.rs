//! Runs that swap their models (ADR 0015): the reviewer plays `critic`,
//! every other role the run's model `fake`, on an engine that holds one
//! of them at a time.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{StopOnEngineWait, deps, fake_project};
use crate::lease::{Gate, LeaseManager};
use crate::ledger::{JournalRecord, Phase};
use crate::orchestrator::{RunError, RunOptions, Runner};
use crate::ports::{
    Clock, EngineAdapter, EngineError, Harness, HarnessError, HarnessInfo, ModelState, Session,
    SessionSpec,
};
use crate::project::Project;
use crate::role::Role;
use crate::role_models::RoleModel;
use crate::testing::{ScriptedHarness, SnapshotVcs, VirtualClock};

/// An engine that lists `fake` and `critic` and refuses the critic's
/// loads once `critic_loads` have worked.
struct OneAtATime {
    loaded: Mutex<BTreeSet<String>>,
    log: Mutex<Vec<String>>,
    critic_loads: Mutex<u32>,
    /// The engine does not answer.
    down: bool,
}

impl OneAtATime {
    /// An engine that loads the critic every time.
    fn loading() -> Arc<Self> {
        Self::new(u32::MAX)
    }

    /// The critic loads for the check at the start of the run, and never
    /// again.
    fn critic_once() -> Arc<Self> {
        Self::new(1)
    }

    /// The critic never loads.
    fn critic_never() -> Arc<Self> {
        Self::new(0)
    }

    /// The engine does not answer.
    fn down() -> Arc<Self> {
        let mut engine = Self::raw(u32::MAX);
        engine.down = true;
        Arc::new(engine)
    }

    fn new(critic_loads: u32) -> Arc<Self> {
        Arc::new(Self::raw(critic_loads))
    }

    /// With `fake` loaded, as the operator left it.
    fn raw(critic_loads: u32) -> Self {
        Self {
            loaded: Mutex::new(BTreeSet::from(["fake".to_owned()])),
            log: Mutex::default(),
            critic_loads: Mutex::new(critic_loads),
            down: false,
        }
    }

    /// From now on the critic loads every time.
    fn let_critic_load(&self) {
        *self.critic_loads.lock().unwrap() = u32::MAX;
    }

    /// The engine drops every model by itself.
    fn drop_all(&self) {
        self.loaded.lock().unwrap().clear();
    }

    fn loaded(&self) -> Vec<String> {
        self.loaded.lock().unwrap().iter().cloned().collect()
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Takes one of the critic's loads; false when none is left.
    fn critic_may_load(&self) -> bool {
        let mut left = self.critic_loads.lock().unwrap();
        let may = *left > 0;
        *left = left.saturating_sub(1);
        may
    }
}

#[async_trait]
impl EngineAdapter for OneAtATime {
    fn name(&self) -> &str {
        "one-at-a-time"
    }

    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        if self.down {
            return Err(EngineError::Unreachable("connection refused".into()));
        }
        let loaded = self.loaded.lock().unwrap();
        Ok(["fake", "critic"]
            .into_iter()
            .map(|id| ModelState {
                id: id.into(),
                loaded: loaded.contains(id),
                ready: loaded.contains(id),
                context_length: None,
            })
            .collect())
    }

    async fn load(&self, model: &str) -> Result<(), EngineError> {
        if model == "critic" && !self.critic_may_load() {
            return Err(EngineError::Refused("no memory".into()));
        }
        self.log.lock().unwrap().push(format!("load {model}"));
        self.loaded.lock().unwrap().insert(model.to_owned());
        Ok(())
    }

    async fn unload(&self, model: &str) -> Result<(), EngineError> {
        self.log.lock().unwrap().push(format!("unload {model}"));
        self.loaded.lock().unwrap().remove(model);
        Ok(())
    }

    fn unloads_each_model(&self) -> bool {
        true
    }
}

/// The scripted harness, noting which models the engine held when each
/// session opened, and with which model.
struct Watching {
    scripted: ScriptedHarness,
    engine: Arc<OneAtATime>,
    opened: Mutex<Vec<Opened>>,
    /// The engine drops its models as the session with this index opens
    /// (0 is the preflight).
    drop_at: Option<usize>,
}

#[derive(Clone, Debug)]
struct Opened {
    label: String,
    model: String,
    loaded: Vec<String>,
}

#[async_trait]
impl Harness for Watching {
    fn info(&self) -> HarnessInfo {
        self.scripted.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        {
            let mut opened = self.opened.lock().unwrap();
            if self.drop_at == Some(opened.len()) {
                self.engine.drop_all();
            }
            opened.push(Opened {
                label: spec.label.clone(),
                model: spec.model.id.clone(),
                loaded: self.engine.loaded(),
            });
        }
        self.scripted.open(spec).await
    }
}

/// A swapping run of a project whose `role` plays `critic`, played to its
/// end or until it stops.
struct Swapped {
    _work: tempfile::TempDir,
    project: Project,
    vcs: Arc<SnapshotVcs>,
    engine: Arc<OneAtATime>,
    harness: Arc<Watching>,
    result: Result<(), RunError>,
}

/// How a swapping run goes, besides its engine.
struct Plan {
    /// The role that plays `critic`.
    role: Role,
    scripted: ScriptedHarness,
    /// The engine drops its models as the session with this index opens
    /// (0 is the preflight).
    drop_at: Option<usize>,
}

impl Plan {
    fn critic_as(role: Role) -> Self {
        Self {
            role,
            scripted: ScriptedHarness::default(),
            drop_at: None,
        }
    }
}

impl Swapped {
    async fn run(engine: Arc<OneAtATime>, role: Role) -> Self {
        let clock = Arc::new(VirtualClock::new());
        let plan = Plan::critic_as(role);
        Self::run_with(engine, plan, clock, CancellationToken::new()).await
    }

    async fn run_with(
        engine: Arc<OneAtATime>,
        plan: Plan,
        clock: Arc<dyn Clock>,
        stop: CancellationToken,
    ) -> Self {
        let work = tempfile::tempdir().unwrap();
        let (project, vcs) = fake_project(work.path(), &["maxAttempts=2"]);
        let mut ledger = project.load_ledger().unwrap();
        let critic = RoleModel {
            model: "fake/critic".into(),
            id: "critic".into(),
            context_window: 65536,
            input: vec!["text".into()],
        };
        ledger.config.role_models.insert(plan.role, critic);
        project.save_ledger(&ledger).unwrap();
        let harness = Arc::new(Watching {
            scripted: plan.scripted,
            engine: engine.clone(),
            opened: Mutex::default(),
            drop_at: plan.drop_at,
        });
        let mut run = Self {
            _work: work,
            project,
            vcs,
            engine,
            harness,
            result: Ok(()),
        };
        run.again(clock, stop).await;
        run
    }

    /// Runs the project again, as `tenaz run` after a pause.
    async fn again(&mut self, clock: Arc<dyn Clock>, stop: CancellationToken) {
        let mut deps = deps(&self.project, self.vcs.clone(), self.harness.clone(), clock);
        let ids = self.project.load_ledger().unwrap().config.model_ids();
        let lease = LeaseManager::new(self.engine.clone(), ids, Gate::new(), Arc::new(|_| {}));
        deps.lease = Some(Arc::new(lease.with_swap()));
        deps.engine = Some(self.engine.clone());
        let mut runner = Runner::open(self.project.clone(), deps, stop).unwrap();
        let options = RunOptions {
            runner_version: "test".into(),
            ..Default::default()
        };
        self.result = runner.run(options).await;
    }

    fn opened(&self) -> Vec<Opened> {
        self.harness.opened.lock().unwrap().clone()
    }

    /// Whether a session of `model` opened.
    fn opened_on(&self, model: &str) -> bool {
        self.opened().iter().any(|o| o.model == model)
    }

    fn log(&self) -> String {
        self.project
            .read(".tenaz/private/orchestrator.log")
            .unwrap_or_default()
    }
}

/// Each session opens with its role's model alone on the engine, after a
/// check that loaded each model in turn and left the run's resident; the
/// sessions after a review swap back to the run's model.
#[tokio::test(start_paused = true)]
async fn each_session_opens_with_only_its_model_loaded() {
    let run = Swapped::run(OneAtATime::loading(), Role::Reviewer).await;
    let log = run.log();
    assert!(run.result.is_ok(), "{:?}\n{log}", run.result);
    let ledger = run.project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Complete, "{log}");
    let check = ["unload fake", "load critic", "unload critic", "load fake"];
    assert_eq!(
        run.engine.log().get(..4),
        Some(&check.map(str::to_owned)[..])
    );
    let opened = run.opened();
    // The preflight probes the run's model, which the check left resident.
    let first = &opened[0];
    assert_eq!(
        (first.label.as_str(), first.model.as_str()),
        ("preflight", "fake")
    );
    let models: Vec<&str> = opened.iter().map(|o| o.model.as_str()).collect();
    assert!(
        models.windows(2).any(|w| w == ["critic", "fake"]),
        "{models:?}"
    );
    for opened in &opened {
        assert_eq!(
            opened.loaded,
            std::slice::from_ref(&opened.model),
            "{opened:?}"
        );
    }
}

/// A model that cannot load stops the run before any session, naming it.
#[tokio::test(start_paused = true)]
async fn a_model_that_does_not_load_stops_the_run_before_any_session() {
    let run = Swapped::run(OneAtATime::critic_never(), Role::Reviewer).await;
    let refusal = match &run.result {
        Err(RunError::Precondition(message)) => message.clone(),
        other => panic!("not refused: {other:?}"),
    };
    assert!(
        refusal.starts_with("a model of the run did not load alone")
            && refusal.contains("critic: no memory"),
        "{refusal}"
    );
    assert!(run.opened().is_empty(), "{:?}", run.opened());
}

/// A model that stops loading during the run blocks it as engine
/// unavailable, naming the model, after the engine wait.
#[tokio::test(start_paused = true)]
async fn a_swap_the_engine_never_takes_blocks_the_run() {
    let run = Swapped::run(OneAtATime::critic_once(), Role::Reviewer).await;
    let log = run.log();
    let ledger = run.project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Blocked, "{log}");
    let reason = ledger.blocked_reason.unwrap_or_default();
    assert!(
        reason.contains("critic did not load for the reviewer session")
            && reason.contains("no memory"),
        "{reason}"
    );
    assert!(
        log.contains("waiting for the LLMs to serve again for the reviewer session's model critic"),
        "{log}"
    );
    assert!(!run.opened_on("critic"), "{:?}", run.opened());
}

/// A builder whose swap never works gets its attempt back: the session
/// did not run.
#[tokio::test(start_paused = true)]
async fn a_builder_whose_swap_fails_keeps_its_attempt() {
    let run = Swapped::run(OneAtATime::critic_once(), Role::Builder).await;
    let ledger = run.project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Blocked, "{}", run.log());
    let reason = ledger.blocked_reason.unwrap_or_default();
    assert!(
        reason.contains("critic did not load for the builder session"),
        "{reason}"
    );
    let backlog = run.project.load_backlog().backlog;
    let attempts: Vec<u32> = backlog.cards().map(|c| c.attempts).collect();
    assert!(
        !attempts.is_empty() && attempts.iter().all(|a| *a == 0),
        "{attempts:?}"
    );
    assert!(!run.opened_on("critic"), "{:?}", run.opened());
}

/// A stop while the run waits for a swap pauses it before the session,
/// with no continuation note; run again, the check at the start loads
/// each model once more and the run ends.
#[tokio::test(start_paused = true)]
async fn a_stop_during_a_swap_pauses_before_the_session() {
    let stop = CancellationToken::new();
    let clock: Arc<dyn Clock> = Arc::new(StopOnEngineWait {
        clock: VirtualClock::new(),
        stop: stop.clone(),
    });
    let plan = Plan::critic_as(Role::Reviewer);
    let mut run = Swapped::run_with(OneAtATime::critic_once(), plan, clock, stop).await;
    let journal = run.project.journal();
    let paused = journal.iter().find_map(|e| match &e.record {
        JournalRecord::Paused { label, role, .. } => Some((label.clone(), *role)),
        _ => None,
    });
    assert_eq!(
        paused,
        Some((Some("the swap to critic".to_owned()), Some(Role::Reviewer))),
        "{}",
        run.log()
    );
    assert!(!run.project.load_ledger().unwrap().pending_continuation);
    assert!(!run.opened_on("critic"), "{:?}", run.opened());

    run.engine.let_critic_load();
    run.again(Arc::new(VirtualClock::new()), CancellationToken::new())
        .await;
    let ledger = run.project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Complete, "{}", run.log());
    assert!(run.opened_on("critic"), "{:?}", run.opened());
}

/// A stop while the run waits for the engine between two tries of a
/// session pauses at the earlier try, with the continuation note, as a
/// stop after it would, and not at the swap before the next try. Here the
/// first bootstrap try fails in the engine, which dropped the critic
/// meanwhile and will not take it back.
#[tokio::test(start_paused = true)]
async fn a_stop_between_two_tries_pauses_at_the_earlier_try() {
    let stop = CancellationToken::new();
    let clock: Arc<dyn Clock> = Arc::new(StopOnEngineWait {
        clock: VirtualClock::new(),
        stop: stop.clone(),
    });
    let plan = Plan {
        scripted: ScriptedHarness {
            engine_failures: true,
            ..Default::default()
        },
        drop_at: Some(1),
        ..Plan::critic_as(Role::Bootstrap)
    };
    // The critic loads for the check at the start and the first try only.
    let run = Swapped::run_with(OneAtATime::new(2), plan, clock, stop).await;
    let first = run.opened().get(1).map(|o| o.label.clone());
    let paused = run
        .project
        .journal()
        .into_iter()
        .find_map(|e| match e.record {
            JournalRecord::Paused { label, .. } => Some(label),
            _ => None,
        });
    assert!(first.is_some(), "{:?}", run.opened());
    assert_eq!(paused, Some(first), "{}", run.log());
    assert!(run.project.load_ledger().unwrap().pending_continuation);
}

/// An engine that does not answer at the start is refused as without
/// swapping: start the inference server first.
#[tokio::test(start_paused = true)]
async fn an_engine_that_does_not_answer_is_told_to_start() {
    let run = Swapped::run(OneAtATime::down(), Role::Reviewer).await;
    let refusal = match &run.result {
        Err(RunError::Precondition(message)) => message.clone(),
        other => panic!("not refused: {other:?}"),
    };
    assert!(
        refusal.contains("Start the inference server first"),
        "{refusal}"
    );
    assert!(run.opened().is_empty(), "{:?}", run.opened());
}
