//! Runs where some roles play another model than the run's: each session
//! is opened with its role's model and budgeted by that model's window
//! (ADR 0011).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::briefing;
use crate::lease::{Gate, LeaseManager};
use crate::ledger::{JournalRecord, SessionRecord};
use crate::orchestrator::{RunError, RunOptions, Runner};
use crate::params::RunParams;
use crate::ports::{
    EngineAdapter, EngineError, Harness, HarnessError, HarnessInfo, ModelState, Session,
    SessionModel, SessionSpec,
};
use crate::project::Project;
use crate::role::Role;
use crate::role_models::RoleModel;
use crate::testing::{ScriptedHarness, VirtualClock};

/// The scripted harness, keeping the model each session was opened with.
struct Recording {
    scripted: ScriptedHarness,
    opened: Mutex<Vec<(String, SessionModel)>>,
}

#[async_trait]
impl Harness for Recording {
    fn info(&self) -> HarnessInfo {
        self.scripted.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        self.opened
            .lock()
            .unwrap()
            .push((spec.label.clone(), spec.model.clone()));
        self.scripted.open(spec).await
    }
}

/// A run played to its end with some roles pinned to their own models.
struct Played {
    _work: tempfile::TempDir,
    project: Project,
    records: Vec<JournalRecord>,
    opened: Vec<(String, SessionModel)>,
}

impl Played {
    async fn with(scripted: ScriptedHarness, roles: &[(Role, RoleModel)]) -> Self {
        let work = tempfile::tempdir().unwrap();
        // A replan every two tasks, so every role plays.
        let (project, vcs) = fake_project(work.path(), &["maxAttempts=2", "replanEvery=2"]);
        let mut ledger = project.load_ledger().unwrap();
        ledger.config.role_models.extend(roles.iter().cloned());
        project.save_ledger(&ledger).unwrap();
        let harness = Arc::new(Recording {
            scripted,
            opened: Mutex::default(),
        });
        let deps = deps(
            &project,
            vcs,
            harness.clone(),
            Arc::new(VirtualClock::new()),
        );
        run(&project, deps, CancellationToken::new()).await;
        let records = project.journal().into_iter().map(|e| e.record).collect();
        let opened = harness.opened.lock().unwrap().clone();
        Self {
            _work: work,
            project,
            records,
            opened,
        }
    }

    fn sessions(&self) -> impl Iterator<Item = &SessionRecord> {
        self.records.iter().filter_map(session)
    }

    /// The index of the first record that passes `test`.
    fn first(&self, test: impl Fn(&JournalRecord) -> bool) -> usize {
        let found = self.records.iter().position(test);
        found.unwrap_or_else(|| panic!("no such record: {:#?}", self.records))
    }

    fn run_model(&self) -> SessionModel {
        self.project.load_ledger().unwrap().config.run_model()
    }
}

fn session(record: &JournalRecord) -> Option<&SessionRecord> {
    match record {
        JournalRecord::Session(s) => Some(s),
        _ => None,
    }
}

fn is_raise(record: &JournalRecord) -> bool {
    matches!(record, JournalRecord::ContextCapRaised { .. })
}

fn role_model(id: &str, context_window: u64) -> RoleModel {
    RoleModel {
        model: format!("fake/{id}"),
        id: id.into(),
        context_window,
        input: vec!["text".into()],
    }
}

#[tokio::test(start_paused = true)]
async fn each_session_runs_its_role_s_model_within_that_model_s_budget() {
    let critic = role_model("critic", 65536);
    // A task that cannot pass is split, so the split plays too.
    let scripted = ScriptedHarness {
        failing_task: Some("T003".into()),
        ..Default::default()
    };
    let run = Played::with(scripted, &[(Role::Reviewer, critic.clone())]).await;
    let run_model = run.run_model();
    let roles: Vec<Role> = run.sessions().map(|s| s.role).collect();
    for role in Role::ALL {
        assert!(roles.contains(&role), "no {role} session: {roles:?}");
    }
    // The preflight probes the run's model, and every other spec opened
    // belongs to a journaled session.
    assert_eq!(run.opened[0], ("preflight".to_owned(), run_model.clone()));
    assert_eq!(run.opened.len(), roles.len() + 1);
    for session in run.sessions() {
        // The default share of each window: 0.65 of 65536 and of 131072.
        let (expected, budget) = match session.role {
            Role::Reviewer => (critic.session_model(), 42598),
            _ => (run_model.clone(), 85196),
        };
        let label = &session.label;
        assert_eq!(session.model.as_ref(), Some(&expected.name), "{label}");
        assert_eq!(session.context_budget, budget, "{label}");
        let opened = run.opened.iter().find(|(l, _)| l == label);
        assert_eq!(opened, Some(&(label.clone(), expected)));
    }
}

/// Cards are sized to the builder's budget, from the builder's model.
#[tokio::test(start_paused = true)]
async fn the_planners_size_cards_for_the_builder_s_model() {
    let builder = role_model("builder", 65536);
    let run = Played::with(ScriptedHarness::default(), &[(Role::Builder, builder)]).await;
    let prompts: Vec<String> = std::fs::read_dir(run.project.private_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".prompt.md"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    let sized = |budget| {
        let text = briefing::card_sizing(budget);
        prompts.iter().any(|p| p.contains(&text))
    };
    assert!(sized(42598), "no prompt sizes cards for 42598 tokens");
    assert!(!sized(85196), "a prompt sizes cards for the run's model");
}

/// After the engine runs out of memory, the lowered budget is raised
/// toward the largest model's budget, not only the run's.
#[tokio::test(start_paused = true)]
async fn a_raise_aims_at_the_largest_model_s_budget() {
    let scripted = ScriptedHarness {
        engine_failures: true,
        ..Default::default()
    };
    let critic = role_model("critic", 262_144);
    let run = Played::with(scripted, &[(Role::Reviewer, critic)]).await;
    // Halfway from 49190 to 0.65 of 262144 (170393), not to 85196.
    let raised = run.first(is_raise);
    assert!(
        matches!(
            run.records[raised],
            JournalRecord::ContextCapRaised {
                context_cap: Some(109_791),
                ..
            }
        ),
        "{:#?}",
        run.records[raised]
    );
    // Above the run's budget now, the cap holds back only the critic.
    let after: Vec<&SessionRecord> = run.records[raised..].iter().filter_map(session).collect();
    let budgets = |role| -> Vec<u64> {
        after
            .iter()
            .filter(|s| s.role == role)
            .map(|s| s.context_budget)
            .collect()
    };
    let builders = budgets(Role::Builder);
    assert!(
        !builders.is_empty() && builders.iter().all(|b| *b == 85196),
        "{builders:?}"
    );
    let reviewers = budgets(Role::Reviewer);
    assert!(reviewers.contains(&109_791), "{reviewers:?}");
}

/// A session whose budget is under the lowered cap does not count toward
/// raising it: it did not test what the engine can hold.
#[tokio::test(start_paused = true)]
async fn sessions_under_the_cap_do_not_count_toward_a_raise() {
    let scripted = ScriptedHarness {
        engine_failures: true,
        ..Default::default()
    };
    // 0.65 of 32768 is 21299, under the 49190 cap.
    let critic = role_model("critic", 32768);
    let run = Played::with(scripted, &[(Role::Reviewer, critic)]).await;
    let raised = run.first(is_raise);
    let lowered = run.records[..raised]
        .iter()
        .rposition(|r| {
            matches!(
                r,
                JournalRecord::ContextCap {
                    context_cap: 49190,
                    ..
                }
            )
        })
        .expect("the cap was lowered before it was raised");
    let held: Vec<u64> = run.records[lowered..raised]
        .iter()
        .filter_map(session)
        .filter(|s| s.ok)
        .map(|s| s.context_budget)
        .collect();
    // `capacityRecoverSessions` sessions at the cap raise it; the critic's
    // sessions at 21299 ran meanwhile and did not count.
    let needed = RunParams::default().capacity_recover_sessions;
    let at_cap = held.iter().filter(|b| **b == 49190).count();
    assert_eq!(u32::try_from(at_cap).unwrap(), needed, "{held:?}");
    assert!(
        matches!(
            run.records[raised],
            JournalRecord::ContextCapRaised { clean_sessions, .. } if clean_sessions == needed
        ),
        "{:#?}",
        run.records[raised]
    );
    assert!(held.contains(&21299), "{held:?}");
}

/// An engine that lists the run's model `fake` and the critic, each loaded
/// or not, or does not answer; it loads nothing.
struct Listing {
    run_loaded: bool,
    critic_loaded: bool,
    down: bool,
}

#[async_trait]
impl EngineAdapter for Listing {
    fn name(&self) -> &str {
        "listing"
    }

    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        if self.down {
            return Err(EngineError::Unreachable("connection refused".into()));
        }
        let model = |id: &str, loaded| ModelState {
            id: id.into(),
            loaded,
            ready: loaded,
            context_length: None,
        };
        Ok(vec![
            model("fake", self.run_loaded),
            model("critic", self.critic_loaded),
        ])
    }

    async fn load(&self, _model: &str) -> Result<(), EngineError> {
        Err(EngineError::Refused("no load API".into()))
    }

    async fn unload(&self, _model: &str) -> Result<(), EngineError> {
        Err(EngineError::Refused("no load API".into()))
    }
}

/// How a run meets the `Listing` engine.
struct OnEngine {
    /// The reviewer plays the critic.
    critic: bool,
    engine: Listing,
    /// Through a lease, as every run on a real engine does.
    lease: bool,
}

impl OnEngine {
    /// The reviewer on the critic, with the critic loaded or not.
    fn critic(critic_loaded: bool, lease: bool) -> Self {
        Self {
            critic: true,
            engine: Listing {
                run_loaded: true,
                critic_loaded,
                down: false,
            },
            lease,
        }
    }
}

/// Runs a project on the `Listing` engine as `on` says.
async fn run_on(on: OnEngine) -> (tempfile::TempDir, Project, Result<(), RunError>) {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let mut ledger = project.load_ledger().unwrap();
    if on.critic {
        ledger
            .config
            .role_models
            .insert(Role::Reviewer, role_model("critic", 65536));
    }
    project.save_ledger(&ledger).unwrap();
    let mut deps = deps(
        &project,
        vcs,
        Arc::new(ScriptedHarness::default()),
        Arc::new(VirtualClock::new()),
    );
    let engine = Arc::new(on.engine);
    if on.lease {
        let ids = ledger.config.model_ids();
        let lease = LeaseManager::new(engine.clone(), ids, Gate::new(), Arc::new(|_| {}));
        deps.lease = Some(Arc::new(lease));
    }
    deps.engine = Some(engine);
    let mut runner = Runner::open(project.clone(), deps, CancellationToken::new()).unwrap();
    let result = runner
        .run(RunOptions {
            runner_version: "test".into(),
            ..Default::default()
        })
        .await;
    (work, project, result)
}

fn refusal(result: Result<(), RunError>) -> String {
    match result {
        Err(RunError::Precondition(message)) => message,
        other => panic!("not refused: {other:?}"),
    }
}

fn no_session(project: &Project) -> bool {
    project
        .journal()
        .iter()
        .all(|e| !matches!(e.record, JournalRecord::Session(_)))
}

/// A role's model the engine has not loaded, and cannot load, stops the
/// run before its first session, named.
#[tokio::test(start_paused = true)]
async fn a_run_does_not_start_until_every_role_s_model_is_loaded() {
    let (_work, project, result) = run_on(OnEngine::critic(false, true)).await;
    let message = refusal(result);
    assert!(
        message.starts_with("Model critic is not being served at "),
        "{message}"
    );
    assert!(
        message.contains(" (loading failed: engine refused: critic: no load API). "),
        "{message}"
    );
    assert!(no_session(&project));
}

/// Without a lease the engine's list is checked, and the message says
/// every model must be loaded at once.
#[tokio::test(start_paused = true)]
async fn the_missing_models_are_named_from_the_engine_s_list() {
    let (_work, project, result) = run_on(OnEngine::critic(false, false)).await;
    let message = refusal(result);
    assert!(
        message.starts_with("Model critic is not being served at "),
        "{message}"
    );
    assert!(
        message.ends_with(
            "Start the inference server first, with every model of the run loaded at once."
        ),
        "{message}"
    );
    assert!(no_session(&project));
}

#[tokio::test(start_paused = true)]
async fn a_run_starts_when_every_role_s_model_is_loaded() {
    let (_work, project, result) = run_on(OnEngine::critic(true, true)).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(!no_session(&project));
}

/// A run of one model gets no word about loading several at once, and an
/// engine that does not answer is named as such.
#[tokio::test(start_paused = true)]
async fn one_model_s_refusal_says_only_what_is_missing() {
    let alone = |run_loaded, down| OnEngine {
        critic: false,
        engine: Listing {
            run_loaded,
            critic_loaded: false,
            down,
        },
        lease: false,
    };
    let (_work, _project, result) = run_on(alone(false, false)).await;
    let message = refusal(result);
    assert!(
        message.starts_with("Model fake is not being served at "),
        "{message}"
    );
    assert!(
        message.ends_with(". Start the inference server first."),
        "{message}"
    );
    let (_work, _project, result) = run_on(alone(true, true)).await;
    assert_eq!(
        refusal(result),
        "Model fake is not being served (engine unreachable: connection refused). Start the inference server first."
    );
}
