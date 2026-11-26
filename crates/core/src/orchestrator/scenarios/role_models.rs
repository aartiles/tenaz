//! Runs where some roles play another model than the run's, or think at
//! another level: each session is opened with its role's model and level
//! and budgeted by that model's window (ADRs 0011, 0042).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, no_session, run};
use crate::briefing;
use crate::effort::EffortDial;
use crate::lease::{Gate, LeaseManager};
use crate::ledger::{JournalRecord, SessionRecord};
use crate::orchestrator::session::lowered_effort_note;
use crate::orchestrator::{Deps, RunError, RunOptions, Runner};
use crate::params::RunParams;
use crate::ports::{
    EngineAdapter, EngineError, Harness, HarnessError, HarnessInfo, ModelState, Session,
    SessionModel, SessionSpec,
};
use crate::project::Project;
use crate::role::Role;
use crate::role_models::RoleModel;
use crate::testing::{ScriptedHarness, VirtualClock};

/// The scripted harness, keeping the model each session was opened with
/// and the effort the engine proxy's dial asked for then.
struct Recording {
    scripted: ScriptedHarness,
    opened: Mutex<Vec<(String, SessionModel)>>,
    dial: EffortDial,
    efforts: Mutex<Vec<(String, Option<String>)>>,
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
        self.efforts
            .lock()
            .unwrap()
            .push((spec.label.clone(), self.dial.current().map(|e| e.effort)));
        self.scripted.open(spec).await
    }
}

/// A run played to its end with some roles pinned to their own models.
struct Played {
    _work: tempfile::TempDir,
    project: Project,
    records: Vec<JournalRecord>,
    opened: Vec<(String, SessionModel)>,
    efforts: Vec<(String, Option<String>)>,
}

impl Played {
    async fn with(scripted: ScriptedHarness, roles: &[(Role, RoleModel)]) -> Self {
        Box::pin(Self::with_levels(scripted, roles, &[])).await
    }

    /// [`Self::with`], some roles also at a thinking level of their own.
    async fn with_levels(
        scripted: ScriptedHarness,
        roles: &[(Role, RoleModel)],
        levels: &[(Role, &str)],
    ) -> Self {
        Box::pin(Self::at(scripted, roles, levels, None)).await
    }

    /// [`Self::with_levels`], the run itself at `run_level` when given.
    async fn at(
        scripted: ScriptedHarness,
        roles: &[(Role, RoleModel)],
        levels: &[(Role, &str)],
        run_level: Option<&str>,
    ) -> Self {
        let work = tempfile::tempdir().unwrap();
        // A replan every two tasks, so every role plays.
        let (project, vcs) = fake_project(work.path(), &["maxAttempts=2", "replanEvery=2"]);
        let mut ledger = project.load_ledger().unwrap();
        if let Some(level) = run_level {
            ledger.config.thinking = Some(level.to_owned());
        }
        ledger.config.role_models.extend(roles.iter().cloned());
        ledger.config.role_thinking.extend(
            levels
                .iter()
                .map(|(role, level)| (*role, (*level).to_owned())),
        );
        project.save_ledger(&ledger).unwrap();
        let dial = EffortDial::new();
        let harness = Arc::new(Recording {
            scripted,
            opened: Mutex::default(),
            dial: dial.clone(),
            efforts: Mutex::default(),
        });
        let deps = Deps {
            effort: Some(dial),
            ..deps(
                &project,
                vcs,
                harness.clone(),
                Arc::new(VirtualClock::new()),
            )
        };
        run(&project, deps, CancellationToken::new()).await;
        let records = project.journal().into_iter().map(|e| e.record).collect();
        let opened = harness.opened.lock().unwrap().clone();
        let efforts = harness.efforts.lock().unwrap().clone();
        Self {
            _work: work,
            project,
            records,
            opened,
            efforts,
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
            Role::Reviewer => (critic.session_model(run_model.thinking.as_deref()), 42598),
            _ => (run_model.clone(), 85196),
        };
        let label = &session.label;
        assert_eq!(session.model.as_ref(), Some(&expected.name), "{label}");
        assert_eq!(session.context_budget, budget, "{label}");
        let opened = run.opened.iter().find(|(l, _)| l == label);
        assert_eq!(opened, Some(&(label.clone(), expected)));
    }
}

/// The run's level is `medium`: the preflight and most roles think at it,
/// the reviewer at its own `high`, the builder not at all. Each session's agent
/// is given its role's level, and the proxy's dial asks for its effort.
#[tokio::test(start_paused = true)]
async fn each_session_asks_for_its_role_s_effort() {
    let levels = [(Role::Reviewer, "high"), (Role::Builder, "off")];
    let run = Box::pin(Played::with_levels(
        ScriptedHarness::default(),
        &[],
        &levels,
    ))
    .await;
    assert_eq!(run.run_model().thinking.as_deref(), Some("medium"));
    // The preflight runs the run's model at the run's level.
    assert_eq!(run.efforts[0], ("preflight".into(), Some("medium".into())));
    let roles: Vec<Role> = run.sessions().map(|s| s.role).collect();
    for role in [Role::Reviewer, Role::Builder, Role::Bootstrap] {
        assert!(roles.contains(&role), "no {role} session: {roles:?}");
    }
    for session in run.sessions() {
        let (level, effort) = match session.role {
            Role::Reviewer => ("high", Some("high")),
            Role::Builder => ("off", None),
            _ => ("medium", Some("medium")),
        };
        let label = &session.label;
        let opened = run.opened.iter().find(|(l, _)| l == label).unwrap();
        assert_eq!(opened.1.thinking.as_deref(), Some(level), "{label}");
        let asked = run.efforts.iter().find(|(l, _)| l == label).unwrap();
        assert_eq!(asked.1.as_deref(), effort, "{label}");
    }
}

/// The preflight asks for the run's level, not a role's: a run at `off`
/// with the reviewer and builder at `high` runs its preflight with no
/// effort, and the reviewer still asks for its own.
#[tokio::test(start_paused = true)]
async fn the_preflight_asks_for_no_effort_when_the_run_is_off() {
    let levels = [(Role::Reviewer, "high"), (Role::Builder, "high")];
    let run = Box::pin(Played::at(
        ScriptedHarness::default(),
        &[],
        &levels,
        Some("off"),
    ))
    .await;
    assert_eq!(run.efforts[0], ("preflight".into(), None));
    let reviewer = run
        .sessions()
        .find(|s| s.role == Role::Reviewer)
        .expect("a reviewer session");
    let asked = run.efforts.iter().find(|(l, _)| *l == reviewer.label);
    assert_eq!(asked.unwrap().1.as_deref(), Some("high"));
}

/// After a builder's reply is cut at the output limit, the next builder
/// asks for the effort below its own level (`medium` for `xhigh`), not
/// below the run's; the review between asks for the reviewer's own, not
/// lowered.
#[tokio::test(start_paused = true)]
async fn a_lowered_builder_steps_down_from_its_own_level() {
    let scripted = ScriptedHarness {
        truncate_after_work_task: Some("T001".into()),
        ..Default::default()
    };
    let levels = [(Role::Builder, "xhigh"), (Role::Reviewer, "high")];
    let run = Box::pin(Played::with_levels(scripted, &[], &levels)).await;
    let efforts: Vec<(&str, Option<&str>)> = run
        .efforts
        .iter()
        .filter(|(label, _)| label.contains("-builder-") || label.contains("-reviewer-"))
        .take(3)
        .map(|(label, effort)| (label.split_once('-').unwrap().1, effort.as_deref()))
        .collect();
    assert_eq!(
        efforts,
        [
            ("builder-T001", Some("xhigh")),
            ("reviewer-T001", Some("high")),
            ("builder-T002", Some("medium")),
        ]
    );
}

/// A role at `low` has no lower step: its next session is not said to
/// reason at a lower effort.
#[tokio::test(start_paused = true)]
async fn a_role_with_no_lower_step_is_not_said_to_be_lowered() {
    let scripted = ScriptedHarness {
        truncate_after_work_task: Some("T001".into()),
        ..Default::default()
    };
    let run = Box::pin(Played::with_levels(
        scripted,
        &[],
        &[(Role::Builder, "low")],
    ))
    .await;
    let builders: Vec<&SessionRecord> =
        run.sessions().filter(|s| s.role == Role::Builder).collect();
    let cut = builders[0].abort_reason.as_deref().unwrap_or_default();
    assert!(
        cut.contains("output limit"),
        "the first builder was cut: {cut}"
    );
    let next = &builders[1].label;
    let log = run.project.read_private("orchestrator.log").unwrap();
    assert!(!log.contains(&lowered_effort_note(next)), "{log}");
    let asked = run.efforts.iter().find(|(l, _)| l == next).unwrap();
    assert_eq!(asked.1.as_deref(), Some("low"));
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
    let params = RunParams::default();
    let turns = params.turn_budget;
    let sized = |budget| {
        let text = briefing::card_sizing(budget, turns, &params);
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

/// A session the engine holds past the lowered cap, with no error, lifts
/// the cap to its peak at once: waiting for a raise cut every session that
/// needed more, until the run blocked.
#[tokio::test(start_paused = true)]
async fn a_session_held_past_the_cap_raises_it_at_once() {
    let scripted = ScriptedHarness {
        engine_failures: true,
        context_stop_task: Some("T001".into()),
        ..Default::default()
    };
    // A reviewer on a 262144 window makes the run's largest budget 170393.
    let reviewer = (Role::Reviewer, role_model("r", 262_144));
    let run = Played::with(scripted, &[reviewer]).await;
    let held = run
        .records
        .iter()
        .find_map(|r| match r {
            JournalRecord::ContextCapOverrun {
                context_peak,
                context_cap,
                ..
            } => Some((*context_peak, *context_cap)),
            _ => None,
        })
        .expect("the overrun lifted the cap");
    // The scripted harness's figures: its bootstrap engine failures lower
    // the cap to 49190. T001's builder outgrew it to 120020 with no engine
    // error: the cap is that peak, not 0.8 of it (ADR 0043).
    assert_eq!(held, (120_020, Some(120_020)));
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
